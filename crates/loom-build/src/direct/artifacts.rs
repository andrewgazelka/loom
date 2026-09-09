//! A compilation unit is keyed by source, dependency keys, compiler and flags.
//! Host proc macros and build-script executables are units too, never rlib aliases.
use super::{Recipe, rejected};
use crate::BuildError;
use loom_proto::{DAG_CBOR_CODEC, RAW_CODEC, Tree, TreeEntry};
use loom_store::Store;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Unit {
    pub key: String,
    pub name: String,
    pub recipe: Recipe,
    pub outputs: Vec<Output>,
    pub dependencies: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct Output {
    pub path: PathBuf,
    pub hash: String,
    pub executable: bool,
}

#[derive(Serialize, Deserialize)]
pub(super) struct Inputs {
    source_tree: String,
    build_output_tree: Option<String>,
    compiler: String,
    arguments: Vec<String>,
    environment: BTreeMap<String, String>,
    dependencies: BTreeMap<String, String>,
}

struct Pending {
    recipe: Recipe,
    name: String,
    outputs: Vec<PathBuf>,
}

pub(super) fn argument<'a>(recipe: &'a Recipe, flag: &str) -> Result<&'a str, BuildError> {
    recipe
        .arguments
        .windows(2)
        .find(|parts| parts[0] == flag)
        .map(|parts| parts[1].as_str())
        .ok_or_else(|| rejected(format!("compilation unit missing {flag}")))
}

pub(super) fn external_paths(recipe: &Recipe) -> Vec<PathBuf> {
    recipe
        .arguments
        .windows(2)
        .filter(|parts| parts[0] == "--extern")
        .filter_map(|parts| parts[1].split_once('=').map(|field| PathBuf::from(field.1)))
        .collect()
}

pub(super) fn capture(
    store: &Store,
    target: &Path,
    compiler: &str,
    cargo_output: &str,
    source_roots: &[PathBuf],
    shareable: bool,
) -> Result<Vec<Unit>, BuildError> {
    let reports = cargo_output
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .filter(|message| message["reason"] == "compiler-artifact")
        .collect::<Vec<_>>();
    let capture_dir = target.join("root-rustc.recipe.units");
    let mut pending = Vec::new();
    for entry in std::fs::read_dir(capture_dir)? {
        let path = entry?.path();
        if path
            .extension()
            .is_none_or(|extension| extension != "recipe")
        {
            continue;
        }
        let mut recipe = Recipe::parse(&std::fs::read(&path)?, Path::new(""))?;
        if recipe
            .environment
            .get("CARGO_PRIMARY_PACKAGE")
            .is_some_and(|value| value == "1")
        {
            continue;
        }
        recipe.source = PathBuf::from(
            recipe
                .environment
                .get("CARGO_MANIFEST_DIR")
                .ok_or_else(|| rejected("unit has no source directory"))?,
        );
        let name = argument(&recipe, "--crate-name")?.to_owned();
        let directory = PathBuf::from(argument(&recipe, "--out-dir")?);
        let manifest = std::fs::canonicalize(recipe.source.join("Cargo.toml"))?;
        let reported = reports
            .iter()
            .filter(|message| {
                message["target"]["name"]
                    .as_str()
                    .is_some_and(|target| target.replace('-', "_") == name)
                    && message["manifest_path"].as_str().is_some_and(|path| {
                        std::fs::canonicalize(path).ok().as_ref() == Some(&manifest)
                    })
            })
            .filter_map(|message| message["filenames"].as_array())
            .flatten()
            .filter_map(serde_json::Value::as_str)
            .map(PathBuf::from)
            .filter(|path| path.starts_with(&directory))
            .collect::<Vec<_>>();
        // Build scripts may invoke rustc to probe language support. Only Cargo
        // artifact records introduce compilation graph nodes.
        if reported.is_empty() {
            continue;
        }
        let canonical_source = std::fs::canonicalize(&recipe.source)?;
        if !source_roots
            .iter()
            .any(|root| canonical_source.starts_with(root))
        {
            return Err(rejected(format!(
                "compiler source escaped admitted roots: {}",
                recipe.source.display()
            )));
        }
        let compiler_messages = std::fs::read_to_string(path.with_extension("stderr"))?;
        let outputs = compiler_messages
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter_map(|message| message["artifact"].as_str().map(PathBuf::from))
            .filter(|path| path.extension().is_none_or(|extension| extension != "d"))
            .collect::<Vec<_>>();
        if outputs.is_empty() {
            return Err(rejected(format!(
                "Cargo unit {name} has no rustc artifact notification"
            )));
        }
        for output in &outputs {
            if !std::fs::canonicalize(output)?.starts_with(target)
                || !std::fs::symlink_metadata(output)?.file_type().is_file()
            {
                return Err(rejected("rustc artifact escaped the build target"));
            }
        }
        pending.push(Pending {
            recipe,
            name,
            outputs,
        });
    }
    let mut units = Vec::new();
    let mut owners = BTreeMap::<PathBuf, String>::new();
    let mut sources = BTreeMap::<PathBuf, String>::new();
    while !pending.is_empty() {
        let Some(index) = pending.iter().position(|unit| {
            external_paths(&unit.recipe)
                .iter()
                .all(|path| owners.contains_key(path))
        }) else {
            return Err(rejected(
                "captured dependency graph is incomplete or cyclic",
            ));
        };
        let unit = pending.remove(index);
        let source_tree = if let Some(hash) = sources.get(&unit.recipe.source) {
            hash.clone()
        } else {
            let hash = source_tree(store, &unit.recipe.source)?;
            sources.insert(unit.recipe.source.clone(), hash.clone());
            hash
        };
        let build_output_tree = unit
            .recipe
            .environment
            .get("OUT_DIR")
            .map(|path| source_tree_fn(store, Path::new(path)))
            .transpose()?;
        let dependencies: BTreeMap<_, _> = external_paths(&unit.recipe)
            .iter()
            .map(|path| (path.to_string_lossy().into_owned(), owners[path].clone()))
            .collect();
        let dependency_keys: Vec<_> = dependencies.values().cloned().collect();
        let inputs = inputs(
            &unit.recipe,
            target,
            compiler,
            source_tree,
            build_output_tree,
            &dependencies,
        );
        let key = store
            .put_value("rust-compilation-inputs", &inputs)
            .map_err(rejected)?;
        let mut outputs = Vec::new();
        for path in unit.outputs {
            let hash = store
                .put("rust-artifact", &std::fs::read(&path)?)
                .map_err(rejected)?;
            let executable = executable(&path)?;
            owners.insert(path.clone(), key.clone());
            outputs.push(Output {
                path,
                hash,
                executable,
            });
        }
        let artifact = Unit {
            key,
            name: unit.name,
            recipe: unit.recipe,
            outputs,
            dependencies: dependency_keys,
        };
        units.push(artifact);
    }
    publish_units(store, shareable, &units)?;
    Ok(units)
}

pub(super) fn initialize_index(store: &Store) -> Result<(), BuildError> {
    store.with_connection(|connection| {
        let initialized: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='rust_artifact_policy')", [], |row| row.get(0))?;
        if !initialized {
            connection.execute_batch("CREATE TABLE IF NOT EXISTS rust_artifacts (key TEXT PRIMARY KEY, artifact_hash TEXT NOT NULL); DELETE FROM rust_artifacts; CREATE TABLE rust_artifact_policy (version INTEGER PRIMARY KEY); INSERT INTO rust_artifact_policy VALUES (1)")?;
        }
        Ok(())
    }).map_err(rejected)
}

pub(super) fn publish_units(
    store: &Store,
    shareable: bool,
    units: &[Unit],
) -> Result<(), BuildError> {
    // Admission is computed from Cargo metadata BEFORE any build script or
    // proc macro executes. Captured compiler recipes are not trust evidence.
    if !shareable {
        return Ok(());
    }
    for artifact in units {
        let hash = store
            .put_value("rust-compilation-artifact", artifact)
            .map_err(rejected)?;
        store.with_connection(|connection| {
            connection.execute_batch("CREATE TABLE IF NOT EXISTS rust_artifacts (key TEXT PRIMARY KEY, artifact_hash TEXT NOT NULL)")?;
            connection.execute("INSERT INTO rust_artifacts VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET artifact_hash=excluded.artifact_hash", [&artifact.key, &hash])?;
            Ok(())
        }).map_err(rejected)?;
    }
    Ok(())
}

pub(super) fn inputs(
    recipe: &Recipe,
    target: &Path,
    compiler: &str,
    source_tree: String,
    build_output_tree: Option<String>,
    dependencies: &BTreeMap<String, String>,
) -> Inputs {
    let normalize = |value: &str| {
        let mut value = value.to_owned();
        for (path, key) in dependencies {
            value = value.replace(path, &format!("$DEP/{key}"));
        }
        value = value.replace(recipe.source.to_string_lossy().as_ref(), "$SOURCE");
        if let Some(sysroot) = Path::new(&recipe.compiler).parent().and_then(Path::parent) {
            value = value.replace(sysroot.to_string_lossy().as_ref(), "$SYSROOT");
        }
        if let Some(path) = recipe.environment.get("OUT_DIR") {
            value = value.replace(path, "$BUILD_OUTPUT");
        }
        value.replace(target.to_string_lossy().as_ref(), "$ARTIFACTS")
    };
    let environment = recipe
        .environment
        .iter()
        .filter(|entry| {
            ![
                "PATH",
                "HOME",
                "TMPDIR",
                "PWD",
                "OLDPWD",
                "SHLVL",
                "_",
                "RUSTUP_HOME",
                "CARGO_HOME",
                "RUSTC_WRAPPER",
            ]
            .contains(&entry.0.as_str())
                && !entry.0.starts_with("LOOM_")
        })
        .map(|entry| (entry.0.clone(), normalize(entry.1)))
        .collect();
    let arguments = recipe
        .arguments
        .iter()
        .map(|value| normalize(value))
        .collect();
    let dependency_keys: Vec<_> = dependencies.values().cloned().collect();
    Inputs {
        source_tree,
        build_output_tree,
        compiler: compiler.into(),
        arguments,
        environment,
        dependencies: dependency_keys
            .iter()
            .map(|key| (key.clone(), key.clone()))
            .collect(),
    }
}

fn source_tree_fn(store: &Store, path: &Path) -> Result<String, BuildError> {
    source_tree(store, path)
}

fn source_tree(store: &Store, path: &Path) -> Result<String, BuildError> {
    tree_hash(Some(store), path)
}

pub(super) fn tree_hash(store: Option<&Store>, path: &Path) -> Result<String, BuildError> {
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| rejected("non-UTF8 crate source filename"))?;
        let path = entry.path();
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            return Err(rejected(format!(
                "source symlink is not content addressed: {}",
                path.display()
            )));
        }
        let directory = kind.is_dir();
        let hash = if directory {
            tree_hash(store, &path)?
        } else {
            let bytes = std::fs::read(&path)?;
            match store {
                Some(store) => store.put("blob", &bytes).map_err(rejected)?,
                None => blake3::hash(&bytes).to_hex().to_string(),
            }
        };
        entries.push(TreeEntry {
            name,
            directory,
            executable: !directory && executable(&path)?,
            reference: loom_proto::reference(
                &hash,
                if directory { DAG_CBOR_CODEC } else { RAW_CODEC },
            )
            .map_err(rejected)?,
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    let tree = Tree { entries };
    match store {
        Some(store) => store.put_value("tree", &tree).map_err(rejected),
        None => Ok(blake3::hash(&loom_proto::encode(&tree).map_err(rejected)?)
            .to_hex()
            .to_string()),
    }
}

pub(super) fn executable(path: &Path) -> Result<bool, BuildError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(std::fs::metadata(path)?.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(false)
    }
}

pub(super) fn restore(output: &Output, bytes: &[u8]) -> Result<(), BuildError> {
    if blake3::hash(bytes).to_hex().as_str() != output.hash {
        return Err(rejected("corrupt Rust artifact in CAS"));
    }
    if let Some(parent) = output.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = output
        .path
        .with_extension(format!("restore-{}", std::process::id()));
    std::fs::write(&temporary, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            &temporary,
            std::fs::Permissions::from_mode(if output.executable { 0o755 } else { 0o644 }),
        )?;
    }
    std::fs::rename(temporary, &output.path)?;
    Ok(())
}
