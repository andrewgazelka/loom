//! Native compiler-cache protocol. The subprocess sees only an artifact mirror,
//! never the application's database. Cargo still schedules the dependency graph.
use super::{Recipe, artifacts, rejected};
use crate::BuildError;
use loom_store::Store;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
struct Mirror {
    compiler: String,
    target: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Owner {
    key: String,
    hash: String,
}

pub(super) fn prepare(
    store: &Store,
    directory: &Path,
    target: &Path,
    compiler: &str,
) -> Result<(), BuildError> {
    std::fs::create_dir_all(directory.join("units"))?;
    std::fs::create_dir_all(directory.join("blobs"))?;
    std::fs::create_dir_all(target.join("unit-owners"))?;
    let hashes = store.with_connection(|connection| {
        connection.execute_batch("CREATE TABLE IF NOT EXISTS rust_artifacts (key TEXT PRIMARY KEY, artifact_hash TEXT NOT NULL)")?;
        let mut statement = connection.prepare("SELECT artifact_hash FROM rust_artifacts")?;
        Ok(statement.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>,_>>()?)
    }).map_err(rejected)?;
    for hash in hashes {
        let Some(unit) = store
            .get_value::<artifacts::Unit>(&hash)
            .map_err(rejected)?
        else {
            continue;
        };
        let mut complete = true;
        for output in &unit.outputs {
            let Some(bytes) = store.get(&output.hash).map_err(rejected)? else {
                complete = false;
                break;
            };
            if blake3::hash(&bytes).to_hex().as_str() != output.hash {
                return Err(rejected("corrupt artifact while creating compiler mirror"));
            }
            let destination = directory.join("blobs").join(&output.hash);
            if !destination.is_file() {
                std::fs::write(destination, bytes)?;
            }
        }
        if complete {
            std::fs::write(
                directory.join("units").join(&unit.key),
                serde_json::to_vec(&unit).map_err(rejected)?,
            )?;
        }
    }
    std::fs::write(
        directory.join("mirror.json"),
        serde_json::to_vec(&Mirror {
            compiler: compiler.into(),
            target: target.into(),
        })
        .map_err(rejected)?,
    )?;
    Ok(())
}

fn owner_path(target: &Path, output: &Path) -> PathBuf {
    target.join("unit-owners").join(
        blake3::hash(output.as_os_str().as_encoded_bytes())
            .to_hex()
            .as_str(),
    )
}

fn publish(target: &Path, path: &Path, key: &str) -> Result<(), BuildError> {
    let bytes = std::fs::read(path)?;
    let owner = Owner {
        key: key.into(),
        hash: blake3::hash(&bytes).to_hex().to_string(),
    };
    let destination = owner_path(target, path);
    let temporary = destination.with_extension(format!("pending-{}", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec(&owner).map_err(rejected)?)?;
    std::fs::rename(temporary, destination)?;
    Ok(())
}

/// Returns false only for a cache miss. Errors are failures, never silent misses.
pub(crate) fn main(
    operation: &str,
    recipe_path: &Path,
    directory: &Path,
) -> Result<bool, BuildError> {
    let mirror: Mirror =
        serde_json::from_slice(&std::fs::read(directory.join("mirror.json"))?).map_err(rejected)?;
    let mut recipe = Recipe::parse(&std::fs::read(recipe_path)?, Path::new(""))?;
    recipe.source = PathBuf::from(
        recipe
            .environment
            .get("CARGO_MANIFEST_DIR")
            .ok_or_else(|| rejected("compiler cache recipe has no package source"))?,
    );
    let mut dependencies = BTreeMap::new();
    for path in artifacts::external_paths(&recipe) {
        let owner_path = owner_path(&mirror.target, &path);
        let owner: Owner = match std::fs::read(owner_path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(rejected)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if blake3::hash(&std::fs::read(&path)?).to_hex().as_str() != owner.hash {
            return Err(rejected(
                "dependency changed after compiler-cache publication",
            ));
        }
        dependencies.insert(path.to_string_lossy().into_owned(), owner.key);
    }
    let inputs = artifacts::inputs(
        &recipe,
        &mirror.target,
        &mirror.compiler,
        artifacts::tree_hash(None, &recipe.source)?,
        recipe
            .environment
            .get("OUT_DIR")
            .map(|path| artifacts::tree_hash(None, Path::new(path)))
            .transpose()?,
        &dependencies,
    );
    let key = blake3::hash(&loom_proto::encode(&inputs).map_err(rejected)?)
        .to_hex()
        .to_string();
    match operation {
        "lookup" => {
            let unit: artifacts::Unit = match std::fs::read(directory.join("units").join(&key)) {
                Ok(bytes) => serde_json::from_slice(&bytes).map_err(rejected)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            let output_directory = Path::new(artifacts::argument(&recipe, "--out-dir")?);
            for output in &unit.outputs {
                let name = output
                    .path
                    .file_name()
                    .ok_or_else(|| rejected("cached artifact has no filename"))?;
                let restored = artifacts::Output {
                    path: output_directory.join(name),
                    hash: output.hash.clone(),
                    executable: output.executable,
                };
                artifacts::restore(
                    &restored,
                    &std::fs::read(directory.join("blobs").join(&output.hash))?,
                )?;
                publish(&mirror.target, &restored.path, &key)?;
                let emit = if restored
                    .path
                    .extension()
                    .is_some_and(|extension| extension == "rmeta")
                {
                    "metadata"
                } else {
                    "link"
                };
                eprintln!(
                    "{}",
                    serde_json::json!({"$message_type":"artifact", "artifact":restored.path,"emit":emit})
                );
            }
            // Cargo owns freshness only during this one graph discovery. Its
            // next execution is the hash-keyed root replay, not an mtime lookup.
            let name = artifacts::argument(&recipe, "--crate-name")?;
            let suffix = recipe
                .arguments
                .iter()
                .find_map(|argument| argument.strip_prefix("extra-filename="))
                .unwrap_or("");
            let dep_info = output_directory.join(format!("{name}{suffix}.d"));
            std::fs::write(
                dep_info,
                format!(
                    "{}: {}\n",
                    output_directory.join(format!("{name}{suffix}")).display(),
                    recipe.source.join("Cargo.toml").display()
                ),
            )?;
            eprintln!("rustc-cache-hit {name} {key}");
            Ok(true)
        }
        "record" => {
            let messages = std::fs::read_to_string(recipe_path.with_extension("stderr"))?;
            for line in messages.lines() {
                let Ok(message) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                if let Some(path) = message["artifact"].as_str() {
                    publish(&mirror.target, Path::new(path), &key)?;
                }
            }
            Ok(true)
        }
        _ => Err(rejected("unknown compiler cache operation")),
    }
}
