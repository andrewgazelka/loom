use super::*;

impl Recipe {
    pub(super) fn parse(bytes: &[u8], source: &Path) -> Result<Self, BuildError> {
        let values = bytes
            .split(|byte| *byte == 0)
            .filter(|value| !value.is_empty())
            .map(|value| String::from_utf8(value.to_vec()).map_err(rejected))
            .collect::<Result<Vec<_>, _>>()?;
        let separator = values
            .iter()
            .position(|value| value == "LOOM_RUSTC_ARGUMENTS")
            .ok_or_else(|| rejected("rustc capture has no argument boundary"))?;
        let mut environment = BTreeMap::new();
        for field in &values[..separator] {
            let (name, value) = field
                .split_once('=')
                .ok_or_else(|| rejected("invalid captured rustc environment"))?;
            if ["CARGO_MAKEFLAGS", "MAKEFLAGS", "MFLAGS"].contains(&name) {
                continue;
            }
            environment.insert(name.into(), value.into());
        }
        let compiler = values
            .get(separator + 1)
            .ok_or_else(|| rejected("rustc capture has no compiler"))?
            .clone();
        Ok(Self {
            source: source.into(),
            compiler,
            environment,
            arguments: values[separator + 2..].to_vec(),
            artifacts: BTreeMap::new(),
            units: Vec::new(),
            layout: None,
        })
    }

    pub(super) fn working_directory(&self) -> &Path {
        self.environment
            .get("LOOM_RUSTC_CWD")
            .or_else(|| self.environment.get("PWD"))
            .map(Path::new)
            .unwrap_or(&self.source)
    }

    pub(super) fn output(&self) -> Result<PathBuf, BuildError> {
        let value = |flag: &str| {
            self.arguments
                .windows(2)
                .find(|parts| parts[0] == flag)
                .map(|parts| parts[1].as_str())
                .ok_or_else(|| rejected(format!("root rustc invocation missing {flag}")))
        };
        Ok(PathBuf::from(value("--out-dir")?).join(format!("{}.wasm", value("--crate-name")?)))
    }

    pub(super) fn relocate(
        &mut self,
        directory: &Path,
        output: &Path,
        incremental: &Path,
    ) -> Result<(), BuildError> {
        let old = self
            .source
            .to_str()
            .ok_or_else(|| rejected("non-UTF8 source path"))?;
        let new = directory
            .to_str()
            .ok_or_else(|| rejected("non-UTF8 source path"))?;
        for argument in &mut self.arguments {
            *argument = argument.replace(old, new);
        }
        for value in self.environment.values_mut() {
            *value = value.replace(old, new);
        }
        std::fs::create_dir_all(output)?;
        let mut index = 0;
        while index < self.arguments.len() {
            if self.arguments[index] == "--out-dir" {
                self.arguments[index + 1] = output.to_string_lossy().into_owned();
            }
            if self.arguments[index] == "-C"
                && self
                    .arguments
                    .get(index + 1)
                    .is_some_and(|value| value.starts_with("incremental="))
            {
                self.arguments.drain(index..index + 2);
                continue;
            }
            index += 1;
        }
        self.arguments.extend([
            "-C".into(),
            format!("incremental={}", incremental.display()),
        ]);
        let mut previous = std::mem::take(&mut self.arguments).into_iter();
        while let Some(argument) = previous.next() {
            if argument == "--cap-lints" {
                previous.next();
            } else if !argument.starts_with("--cap-lints=") {
                self.arguments.push(argument);
            }
        }
        if !self
            .arguments
            .iter()
            .any(|argument| argument == "-Funsafe-code")
        {
            self.arguments.push("-Funsafe-code".into());
        }
        self.source = directory.into();
        Ok(())
    }

    pub(super) fn capture_artifacts(
        &mut self,
        store: &Store,
        target: &Path,
    ) -> Result<(), BuildError> {
        fn collect(directory: &Path, files: &mut Vec<PathBuf>) -> Result<(), BuildError> {
            for entry in std::fs::read_dir(directory)? {
                let entry = entry?;
                let name = entry.file_name();
                if name == "incremental"
                    || name == ".fingerprint"
                    || name == "root-rustc.recipe.units"
                {
                    continue;
                }
                let path = entry.path();
                if entry.file_type()?.is_dir() {
                    collect(&path, files)?;
                } else if entry.file_type()?.is_file() {
                    files.push(path);
                }
            }
            Ok(())
        }
        let root_name = self
            .arguments
            .windows(2)
            .find(|parts| parts[0] == "--crate-name")
            .ok_or_else(|| rejected("root crate has no name"))?[1]
            .clone();
        let mut files = Vec::new();
        collect(target, &mut files)?;
        for path in files {
            let name = path.file_name().unwrap().to_string_lossy();
            if name == format!("{root_name}.wasm")
                || name == format!("{root_name}.d")
                || name == "root-rustc.recipe"
                || name == "direct.sh"
                || name.ends_with(".pending")
            {
                continue;
            }
            let hash = store
                .put("rust-artifact", &std::fs::read(&path)?)
                .map_err(rejected)?;
            let executable = artifacts::executable(&path)?;
            self.artifacts
                .insert(path, ArtifactFile { hash, executable });
        }
        Ok(())
    }

    pub(super) fn restore_artifacts(&self, store: &Store) -> Result<bool, BuildError> {
        let mut complete = true;
        for (path, artifact) in &self.artifacts {
            let hash = &artifact.hash;
            if let Ok(bytes) = std::fs::read(path)
                && blake3::hash(&bytes).to_hex().as_str() == hash
                && store.codec(hash).map_err(rejected)?.is_some()
                && artifacts::executable(path)? == artifact.executable
            {
                continue;
            }
            let Some(bytes) = store.get(hash).map_err(rejected)? else {
                complete = false;
                continue;
            };
            if blake3::hash(&bytes).to_hex().as_str() != hash {
                return Err(rejected("corrupt Rust artifact in CAS"));
            }
            artifacts::restore(
                &artifacts::Output {
                    path: path.clone(),
                    hash: hash.clone(),
                    executable: artifact.executable,
                },
                &bytes,
            )?;
        }
        Ok(complete)
    }
}

impl Recipe {
    pub(super) fn rebase_graph(
        &mut self,
        root: &Path,
        cache: &Path,
        directory: &Path,
        sysroot: &Path,
        compiler: &str,
    ) -> Result<(), BuildError> {
        let layout = self
            .layout
            .clone()
            .ok_or_else(|| rejected("compiler graph has no path layout"))?;
        let old_source = self.source.clone();
        let replace = |value: &str| {
            value
                .replace(
                    old_source.to_string_lossy().as_ref(),
                    directory.to_string_lossy().as_ref(),
                )
                .replace(
                    layout.cache.to_string_lossy().as_ref(),
                    cache.to_string_lossy().as_ref(),
                )
                .replace(
                    layout.root.to_string_lossy().as_ref(),
                    root.to_string_lossy().as_ref(),
                )
                .replace(
                    layout.sysroot.to_string_lossy().as_ref(),
                    sysroot.to_string_lossy().as_ref(),
                )
        };
        fn rebase(recipe: &mut Recipe, replace: &impl Fn(&str) -> String) {
            recipe.source = PathBuf::from(replace(recipe.source.to_string_lossy().as_ref()));
            for argument in &mut recipe.arguments {
                *argument = replace(argument);
            }
            for value in recipe.environment.values_mut() {
                *value = replace(value);
            }
            recipe.artifacts = std::mem::take(&mut recipe.artifacts)
                .into_iter()
                .map(|entry| {
                    (
                        PathBuf::from(replace(entry.0.to_string_lossy().as_ref())),
                        entry.1,
                    )
                })
                .collect();
        }
        rebase(self, &replace);
        self.compiler = compiler.into();
        for unit in &mut self.units {
            rebase(&mut unit.recipe, &replace);
            unit.recipe.compiler = compiler.into();
            for output in &mut unit.outputs {
                output.path = PathBuf::from(replace(output.path.to_string_lossy().as_ref()));
            }
        }
        self.layout = Some(Layout {
            root: root.into(),
            cache: cache.into(),
            sysroot: sysroot.into(),
        });
        Ok(())
    }

    pub(super) fn restore_sources(
        &mut self,
        store: &Store,
        cache: &Path,
        graph: &Path,
    ) -> Result<(), BuildError> {
        #[derive(serde::Deserialize)]
        struct SourceIdentity {
            source_tree: String,
        }
        for unit in &mut self.units {
            if unit.recipe.source.is_dir() {
                continue;
            }
            let inputs: SourceIdentity = store
                .get_value(&unit.key)
                .map_err(rejected)?
                .ok_or_else(|| rejected("compilation source identity missing"))?;
            let directory = graph.join("sources").join(&unit.key);
            crate::preparation::materialize_tree(store, cache, &directory, &inputs.source_tree)?;
            let old = unit.recipe.source.to_string_lossy().into_owned();
            let new = std::fs::canonicalize(directory)?;
            for argument in &mut unit.recipe.arguments {
                *argument = argument.replace(&old, new.to_string_lossy().as_ref());
            }
            for value in unit.recipe.environment.values_mut() {
                *value = value.replace(&old, new.to_string_lossy().as_ref());
            }
            unit.recipe.source = new;
        }
        Ok(())
    }

    pub(super) fn shell(&self) -> String {
        fn quote(value: &str) -> String {
            format!("'{}'", value.replace('\'', "'\\''"))
        }
        let mut script = String::from("#!/bin/sh\nset -eu\n");
        script.push_str(&format!(
            "cd {}\n",
            quote(self.working_directory().to_string_lossy().as_ref())
        ));
        for (name, value) in &self.environment {
            script.push_str(&format!("export {}={}\n", name, quote(value)));
        }
        script.push_str("exec ");
        script.push_str(&quote(&self.compiler));
        for argument in &self.arguments {
            script.push(' ');
            script.push_str(&quote(argument));
        }
        script.push('\n');
        script
    }
}
