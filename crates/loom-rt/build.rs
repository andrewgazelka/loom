//! Bind persistent native code to the backend sources compiled into this build.
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    env, fs,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    resolve: Resolve,
    workspace_root: PathBuf,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    version: String,
    manifest_path: PathBuf,
}

#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}

#[derive(Deserialize)]
struct Node {
    id: String,
    features: Vec<String>,
    deps: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
    pkg: String,
}

struct PackageInput {
    id: String,
    digest: blake3::Hash,
}

fn main() {
    if let Err(error) = namespace() {
        panic!("Loom compilation backend namespace: {error:#}");
    }
}

fn namespace() -> Result<()> {
    watch(Path::new("build.rs"))?;
    let manifest = PathBuf::from(required_env("CARGO_MANIFEST_DIR")?).join("Cargo.toml");
    let target = required_env("TARGET")?;
    let metadata = metadata(&manifest, &target)?;
    watch(&metadata.workspace_root.join("Cargo.lock"))?;
    watch(&metadata.workspace_root.join("Cargo.toml"))?;
    for package in &metadata.packages {
        // Dependency declarations outside the backend can change unified features.
        watch(&package.manifest_path)?;
    }

    let packages: BTreeMap<_, _> = metadata
        .packages
        .iter()
        .map(|package| (package.id.as_str(), package))
        .collect();
    let nodes: BTreeMap<_, _> = metadata
        .resolve
        .nodes
        .iter()
        .map(|node| (node.id.as_str(), node))
        .collect();
    let own = metadata
        .packages
        .iter()
        .find(|package| package.manifest_path == manifest)
        .context("metadata omitted the loom-rt manifest")?;
    let own_node = nodes.get(own.id.as_str()).context("missing loom-rt node")?;
    let backend = own_node
        .deps
        .iter()
        .find(|dependency| dependency.name == "wasmtime")
        .context("loom-rt has no resolved wasmtime dependency")?;
    let closure = dependency_closure(&backend.pkg, &nodes)?;
    let mut inputs = BTreeMap::new();
    for id in &closure {
        let package = packages
            .get(id.as_str())
            .with_context(|| format!("missing backend package {id}"))?;
        let node = nodes
            .get(id.as_str())
            .with_context(|| format!("missing backend dependency node {id}"))?;
        let mut hash = blake3::Hasher::new();
        frame(&mut hash, b"loom-backend-package-v1");
        frame(&mut hash, package.name.as_bytes());
        frame(&mut hash, package.version.as_bytes());
        let features: BTreeSet<_> = node.features.iter().collect();
        for feature in features {
            frame(&mut hash, feature.as_bytes());
        }
        frame(&mut hash, b"sources");
        let root = package.manifest_path.parent().context("manifest parent")?;
        hash_directory(&mut hash, root, root)?;
        inputs.insert(
            id.as_str(),
            PackageInput {
                id: id.clone(),
                digest: hash.finalize(),
            },
        );
    }

    let mut graph = Vec::new();
    for input in inputs.values() {
        let mut hash = blake3::Hasher::new();
        frame(&mut hash, input.digest.as_bytes());
        let node = nodes
            .get(input.id.as_str())
            .context("backend graph node disappeared")?;
        let mut edges = Vec::new();
        for dependency in &node.deps {
            let child = inputs
                .get(dependency.pkg.as_str())
                .context("backend dependency missing from source closure")?;
            let mut edge = blake3::Hasher::new();
            frame(&mut edge, dependency.name.as_bytes());
            frame(&mut edge, child.digest.as_bytes());
            edges.push(*edge.finalize().as_bytes());
        }
        edges.sort();
        for edge in edges {
            frame(&mut hash, &edge);
        }
        graph.push(*hash.finalize().as_bytes());
    }
    graph.sort();
    let mut hash = blake3::Hasher::new();
    frame(&mut hash, b"loom-compilation-backend-v1");
    frame(
        &mut hash,
        inputs
            .get(backend.pkg.as_str())
            .context("backend root missing from source closure")?
            .digest
            .as_bytes(),
    );
    for package in graph {
        frame(&mut hash, &package);
    }
    let rustc = required_env("RUSTC")?;
    frame(&mut hash, &output(Command::new(rustc).arg("-vV"))?);
    hash_environment(&mut hash)?;
    println!(
        "cargo:rustc-env=LOOM_COMPILATION_BACKEND_NAMESPACE={}",
        hash.finalize().to_hex()
    );
    Ok(())
}

fn metadata(manifest: &Path, target: &str) -> Result<Metadata> {
    // Metadata does not acquire Cargo's build-directory lock or launch builds.
    // Its feature selection is the full workspace's default selection. Currently
    // only loom-rt directly uses Wasmtime; this describes the supported workspace
    // build, not arbitrary downstream feature-unification or CLI feature flags.
    let cargo = required_env("CARGO")?;
    let bytes = output(
        Command::new(cargo)
            .args(["metadata", "--locked", "--offline", "--format-version", "1"])
            .arg("--manifest-path")
            .arg(manifest)
            .arg("--filter-platform")
            .arg(target),
    )?;
    serde_json::from_slice(&bytes).context("decode locked backend metadata")
}

fn dependency_closure(root: &str, nodes: &BTreeMap<&str, &Node>) -> Result<BTreeSet<String>> {
    let mut closure = BTreeSet::new();
    let mut pending = vec![root.to_owned()];
    while let Some(id) = pending.pop() {
        if !closure.insert(id.clone()) {
            continue;
        }
        let node = nodes
            .get(id.as_str())
            .with_context(|| format!("missing backend dependency node {id}"))?;
        pending.extend(node.deps.iter().map(|dependency| dependency.pkg.clone()));
    }
    Ok(closure)
}

fn hash_directory(hash: &mut blake3::Hasher, root: &Path, directory: &Path) -> Result<()> {
    watch(directory)?;
    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("read backend source directory {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path.strip_prefix(root)?;
        // These root directories are VCS state and Cargo output, not package
        // sources. Nested directories with either name remain part of the hash.
        if directory == root && matches!(entry.file_name().to_str(), Some(".git" | "target")) {
            continue;
        }
        let kind = entry.file_type()?;
        ensure!(
            !kind.is_symlink(),
            "backend source symlink {}",
            path.display()
        );
        let name = relative.to_str().context("non-UTF-8 backend source path")?;
        frame(hash, name.as_bytes());
        if kind.is_dir() {
            frame(hash, b"directory");
            hash_directory(hash, root, &path)?;
        } else if kind.is_file() {
            frame(hash, b"file");
            watch(&path)?;
            frame(
                hash,
                &fs::read(&path)
                    .with_context(|| format!("read backend source {}", path.display()))?,
            );
        } else {
            bail!("backend source is not a regular file: {}", path.display());
        }
    }
    Ok(())
}

fn hash_environment(hash: &mut blake3::Hasher) -> Result<()> {
    let mut names: BTreeSet<String> = [
        "TARGET",
        "HOST",
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "RUSTC_BOOTSTRAP",
        "CARGO_PROFILE_DEV_OPT_LEVEL",
        "CARGO_PROFILE_RELEASE_OPT_LEVEL",
        "CARGO_PROFILE_RELEASE_LTO",
        "CARGO_PROFILE_DEV_LTO",
        "CARGO_INCREMENTAL",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    names.extend(env::vars_os().filter_map(|entry| {
        let name = entry.0.into_string().ok()?;
        (name.starts_with("CARGO_CFG_") || name.starts_with("CARGO_FEATURE_")).then_some(name)
    }));
    for name in names {
        println!("cargo:rerun-if-env-changed={name}");
        frame(hash, name.as_bytes());
        match env::var_os(&name) {
            Some(value) => {
                frame(hash, b"present");
                frame(
                    hash,
                    value.to_str().context("non-UTF-8 build flag")?.as_bytes(),
                );
            }
            None => frame(hash, b"absent"),
        }
    }
    Ok(())
}

fn required_env(name: &str) -> Result<String> {
    println!("cargo:rerun-if-env-changed={name}");
    env::var(name).with_context(|| format!("missing build environment {name}"))
}

fn watch(path: &Path) -> Result<()> {
    let name = path.to_str().context("non-UTF-8 watched source path")?;
    ensure!(
        !name.contains(['\n', '\r']),
        "newline in watched source path"
    );
    let kind = fs::symlink_metadata(path)
        .with_context(|| format!("missing backend input {}", path.display()))?;
    ensure!(
        !kind.is_symlink(),
        "backend input symlink {}",
        path.display()
    );
    println!("cargo:rerun-if-changed={name}");
    Ok(())
}

fn output(command: &mut Command) -> Result<Vec<u8>> {
    let output = command
        .output()
        .with_context(|| format!("run {command:?}"))?;
    ensure!(
        output.status.success(),
        "{command:?} failed ({}): {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(output.stdout)
}

fn frame(hash: &mut blake3::Hasher, bytes: &[u8]) {
    hash.update(&(bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}
