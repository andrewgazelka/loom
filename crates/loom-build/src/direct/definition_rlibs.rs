//! Definition-dependency rlibs, captured per crates.io graph.
//!
//! A definition that depends on other definitions compiles them as path crates
//! named `loom-definition-<hash16>`; the caller's manifest and lock therefore
//! change with the dependency set, and today that changes the graph key, so
//! every new dependency set pays a cold Cargo resolve even when each dependency
//! rlib already exists in some graph. This module records, after every cold
//! bootstrap, which CAS artifacts are the rlib and rmeta of each definition
//! dependency, keyed by what makes an rlib linkable: the definition hash, the
//! compiler identity, the target, and the crates.io lock with the
//! `loom-definition-*` packages removed. `restore` writes them into a deps
//! directory. Nothing consumes `restore` yet; the graph key still includes the
//! dependency set. The consumer and its done-when tests are in
//! `docs/future/dependency-rlib-reuse.md`.
//!
//! Records live in the CAS as `rust-definition-rlib` values and are indexed by
//! the `rust_definition_rlibs` table; a record is replaced whenever a cold
//! bootstrap captures the same key again (same inputs, so same bytes).
use super::{artifacts, rejected};
use crate::BuildError;
use loom_check::CheckedDef;
use loom_store::Store;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::Path};

/// Crate-name prefix `materialize.rs` gives every definition dependency.
const CRATE_PREFIX: &str = "loom_definition_";
/// Package-name prefix of the same crates in `Cargo.lock`.
const PACKAGE_PREFIX: &str = "loom-definition-";

pub(crate) struct Context<'a> {
    /// `<rustc -vV>\nhash-rustc:<driver hash>`, as folded into the graph key.
    pub compiler_identity: &'a str,
    /// The caller graph's `Cargo.lock` bytes.
    pub lock: &'a [u8],
    /// Rust target triple.
    pub target: &'a str,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub(crate) struct RlibFile {
    /// File name inside the deps directory, e.g. `libloom_definition_0123abcd-<meta>.rlib`.
    pub name: String,
    pub hash: String,
    pub executable: bool,
}

#[derive(Clone, Serialize, Deserialize, PartialEq, Eq, Debug)]
pub(crate) struct DefinitionRlib {
    pub definition: String,
    pub crate_name: String,
    /// The compilation unit (`artifacts::Unit::key`) that produced these files.
    pub unit_key: String,
    pub files: Vec<RlibFile>,
}

/// The lock with every `loom-definition-*` package and every reference to one
/// removed, serialized canonically. Two callers with the same crates.io graph
/// but different definition dependencies normalize to the same bytes.
pub(crate) fn normalized_lock(lock: &[u8]) -> Result<String, BuildError> {
    let text = std::str::from_utf8(lock).map_err(rejected)?;
    let mut document: toml::Value = toml::from_str(text).map_err(rejected)?;
    let Some(packages) = document
        .get_mut("package")
        .and_then(toml::Value::as_array_mut)
    else {
        return Err(rejected("Cargo.lock has no package array"));
    };
    packages.retain(|package| {
        !package
            .get("name")
            .and_then(toml::Value::as_str)
            .is_some_and(|name| name.starts_with(PACKAGE_PREFIX))
    });
    for package in packages.iter_mut() {
        if let Some(dependencies) = package
            .get_mut("dependencies")
            .and_then(toml::Value::as_array_mut)
        {
            dependencies.retain(|reference| {
                !reference
                    .as_str()
                    .is_some_and(|reference| reference.starts_with(PACKAGE_PREFIX))
            });
        }
    }
    toml::to_string(&document).map_err(rejected)
}

/// What makes a definition rlib linkable into a caller graph.
pub(crate) fn key(definition: &str, context: &Context<'_>) -> Result<String, BuildError> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"loom-definition-rlib-v1");
    for field in [
        definition.as_bytes(),
        context.compiler_identity.as_bytes(),
        context.target.as_bytes(),
        normalized_lock(context.lock)?.as_bytes(),
    ] {
        hasher.update(&(field.len() as u64).to_le_bytes());
        hasher.update(field);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Record every `loom_definition_*` unit of a cold bootstrap. Returns the number
/// of records written. A unit whose name matches no dependency, or more than
/// one, is an error: the caller's dependency closure is the only source of
/// full hashes, and a wrong record would link a wrong crate later.
pub(crate) fn capture(
    store: &Store,
    units: &[artifacts::Unit],
    dependencies: &BTreeMap<String, CheckedDef>,
    context: &Context<'_>,
) -> Result<usize, BuildError> {
    let mut written = 0;
    for unit in units {
        let Some(prefix) = unit.name.strip_prefix(CRATE_PREFIX) else {
            continue;
        };
        let mut matching = dependencies.keys().filter(|hash| hash.starts_with(prefix));
        let definition = match (matching.next(), matching.next()) {
            (Some(hash), None) => hash.clone(),
            (None, _) => {
                return Err(rejected(format!(
                    "compilation unit {} names no dependency of this definition",
                    unit.name
                )));
            }
            (Some(_), Some(_)) => {
                return Err(rejected(format!(
                    "compilation unit {} names more than one dependency",
                    unit.name
                )));
            }
        };
        let files = unit
            .outputs
            .iter()
            .map(|output| {
                Ok(RlibFile {
                    name: output
                        .path
                        .file_name()
                        .ok_or_else(|| rejected("definition rlib output has no file name"))?
                        .to_string_lossy()
                        .into_owned(),
                    hash: output.hash.clone(),
                    executable: output.executable,
                })
            })
            .collect::<Result<Vec<_>, BuildError>>()?;
        if files.is_empty() {
            return Err(rejected(format!(
                "compilation unit {} produced no artifact",
                unit.name
            )));
        }
        let record = DefinitionRlib {
            definition: definition.clone(),
            crate_name: unit.name.clone(),
            unit_key: unit.key.clone(),
            files,
        };
        let record_key = key(&definition, context)?;
        let hash = store
            .put_value("rust-definition-rlib", &record)
            .map_err(rejected)?;
        store
            .with_connection(|connection| {
                connection.execute_batch("CREATE TABLE IF NOT EXISTS rust_definition_rlibs (key TEXT PRIMARY KEY, record_hash TEXT NOT NULL)")?;
                connection.execute("INSERT INTO rust_definition_rlibs VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET record_hash=excluded.record_hash", [&record_key, &hash])?;
                Ok(())
            })
            .map_err(rejected)?;
        written += 1;
    }
    Ok(written)
}

// Consumer: docs/future/dependency-rlib-reuse.md step 2 (the warm replay's
// `--extern` synthesis). Until it lands only the tests read records.
#[allow(dead_code)]
pub(crate) fn lookup(store: &Store, key: &str) -> Result<Option<DefinitionRlib>, BuildError> {
    let hash = store
        .with_connection(|connection| {
            connection.execute_batch("CREATE TABLE IF NOT EXISTS rust_definition_rlibs (key TEXT PRIMARY KEY, record_hash TEXT NOT NULL)")?;
            let mut statement =
                connection.prepare("SELECT record_hash FROM rust_definition_rlibs WHERE key=?")?;
            let mut rows = statement.query_map([key], |row| row.get::<_, String>(0))?;
            Ok(rows.next().transpose()?)
        })
        .map_err(rejected)?;
    let Some(hash) = hash else {
        return Ok(None);
    };
    store.get_value(&hash).map_err(rejected)
}

/// Write the record's files into `deps`, verifying each against its hash. An
/// artifact absent from the CAS is an error, never a partial restore: the
/// caller must then fall back to the cold bootstrap explicitly.
// Consumer: docs/future/dependency-rlib-reuse.md step 2; see `lookup`.
#[allow(dead_code)]
pub(crate) fn restore(
    store: &Store,
    record: &DefinitionRlib,
    deps: &Path,
) -> Result<Vec<std::path::PathBuf>, BuildError> {
    let mut restored = Vec::new();
    for file in &record.files {
        let bytes = store.get(&file.hash).map_err(rejected)?.ok_or_else(|| {
            rejected(format!(
                "definition rlib {} for {} is missing from the CAS",
                file.name, record.definition
            ))
        })?;
        let output = artifacts::Output {
            path: deps.join(&file.name),
            hash: file.hash.clone(),
            executable: file.executable,
        };
        artifacts::restore(&output, &bytes)?;
        restored.push(output.path);
    }
    Ok(restored)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::direct::Recipe;

    const LOCK_WITH_DEPENDENCY: &str = r#"version = 4

[[package]]
name = "loom-definition"
version = "0.1.0"
dependencies = [
 "loom-definition-0123456789abcdef",
 "serde",
]

[[package]]
name = "loom-definition-0123456789abcdef"
version = "0.1.0"
dependencies = [
 "serde",
]

[[package]]
name = "serde"
version = "1.0.210"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "c8e3592472072e6e22e0a54d5904d9febf8508f65fb8552499a1abc7d1078c3a"
"#;

    const LOCK_WITHOUT_DEPENDENCY: &str = r#"version = 4

[[package]]
name = "loom-definition"
version = "0.1.0"
dependencies = [
 "serde",
]

[[package]]
name = "serde"
version = "1.0.210"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "c8e3592472072e6e22e0a54d5904d9febf8508f65fb8552499a1abc7d1078c3a"
"#;

    fn dependency(hash: &str) -> CheckedDef {
        CheckedDef {
            hash: hash.into(),
            lang: loom_proto::Lang::Rust,
            name: "dependency".into(),
            source: "pub fn dependency() -> i64 { 1 }".into(),
            deps: BTreeMap::new(),
            sig: Default::default(),
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn key_ignores_definition_packages_but_tracks_registry_pins_and_compiler() {
        let definition = "0123456789abcdef".repeat(4);
        let with = Context {
            compiler_identity: "rustc 1.0\nhash-rustc:a",
            lock: LOCK_WITH_DEPENDENCY.as_bytes(),
            target: "wasm32-unknown-unknown",
        };
        let without = Context {
            lock: LOCK_WITHOUT_DEPENDENCY.as_bytes(),
            ..with
        };
        assert_eq!(
            key(&definition, &with).unwrap(),
            key(&definition, &without).unwrap()
        );
        let repinned = Context {
            lock: LOCK_WITHOUT_DEPENDENCY
                .replace("1.0.210", "1.0.211")
                .leak()
                .as_bytes(),
            ..with
        };
        assert_ne!(
            key(&definition, &with).unwrap(),
            key(&definition, &repinned).unwrap()
        );
        let other_compiler = Context {
            compiler_identity: "rustc 1.1\nhash-rustc:b",
            ..with
        };
        assert_ne!(
            key(&definition, &with).unwrap(),
            key(&definition, &other_compiler).unwrap()
        );
        assert_ne!(
            key(&definition, &with).unwrap(),
            key(&"fedcba9876543210".repeat(4), &with).unwrap()
        );
    }

    #[test]
    fn capture_records_dependency_units_and_restore_rewrites_their_files() {
        let store = Store::memory().unwrap();
        let directory =
            std::env::temp_dir().join(format!("loom-definition-rlibs-{}", std::process::id()));
        if directory.exists() {
            std::fs::remove_dir_all(&directory).unwrap();
        }
        std::fs::create_dir_all(&directory).unwrap();
        let definition = "0123456789abcdef".repeat(4);
        let rlib = store.put("rust-artifact", b"rlib bytes").unwrap();
        let rmeta = store.put("rust-artifact", b"rmeta bytes").unwrap();
        let recipe = Recipe::parse(b"LOOM_RUSTC_ARGUMENTS\0rustc\0", &directory).unwrap();
        let unit = |name: &str, outputs: Vec<artifacts::Output>| artifacts::Unit {
            key: format!("unit-{name}"),
            name: name.into(),
            recipe: recipe.clone(),
            outputs,
            dependencies: Vec::new(),
        };
        let units = vec![
            unit(
                "serde",
                vec![artifacts::Output {
                    path: directory.join("libserde-1.rlib"),
                    hash: rlib.clone(),
                    executable: false,
                }],
            ),
            unit(
                "loom_definition_0123456789abcdef",
                vec![
                    artifacts::Output {
                        path: directory.join("libloom_definition_0123456789abcdef-9f.rlib"),
                        hash: rlib.clone(),
                        executable: false,
                    },
                    artifacts::Output {
                        path: directory.join("libloom_definition_0123456789abcdef-9f.rmeta"),
                        hash: rmeta.clone(),
                        executable: false,
                    },
                ],
            ),
        ];
        let context = Context {
            compiler_identity: "rustc 1.0\nhash-rustc:a",
            lock: LOCK_WITH_DEPENDENCY.as_bytes(),
            target: "wasm32-unknown-unknown",
        };
        let dependencies = BTreeMap::from([(definition.clone(), dependency(&definition))]);
        assert_eq!(capture(&store, &units, &dependencies, &context).unwrap(), 1);
        let record = lookup(&store, &key(&definition, &context).unwrap())
            .unwrap()
            .expect("captured record");
        assert_eq!(record.definition, definition);
        assert_eq!(record.crate_name, "loom_definition_0123456789abcdef");
        assert_eq!(record.unit_key, "unit-loom_definition_0123456789abcdef");
        assert_eq!(record.files.len(), 2);
        // A caller graph with a different dependency set finds the same record.
        let other_caller = Context {
            lock: LOCK_WITHOUT_DEPENDENCY.as_bytes(),
            ..context
        };
        assert!(
            lookup(&store, &key(&definition, &other_caller).unwrap())
                .unwrap()
                .is_some()
        );
        let deps = directory.join("other-graph/deps");
        let restored = restore(&store, &record, &deps).unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(
            std::fs::read(deps.join("libloom_definition_0123456789abcdef-9f.rlib")).unwrap(),
            b"rlib bytes"
        );
        assert_eq!(
            std::fs::read(deps.join("libloom_definition_0123456789abcdef-9f.rmeta")).unwrap(),
            b"rmeta bytes"
        );
        // A unit naming an unknown dependency is refused, not guessed.
        let unknown = vec![unit("loom_definition_ffffffffffffffff", Vec::new())];
        assert!(
            capture(&store, &unknown, &dependencies, &context)
                .unwrap_err()
                .to_string()
                .contains("names no dependency")
        );
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn restore_refuses_a_partial_record() {
        let store = Store::memory().unwrap();
        let directory = std::env::temp_dir().join(format!(
            "loom-definition-rlibs-partial-{}",
            std::process::id()
        ));
        let record = DefinitionRlib {
            definition: "0".repeat(64),
            crate_name: "loom_definition_0000000000000000".into(),
            unit_key: "unit".into(),
            files: vec![RlibFile {
                name: "libloom_definition_0000000000000000-1.rlib".into(),
                hash: blake3::hash(b"never stored").to_hex().to_string(),
                executable: false,
            }],
        };
        let error = restore(&store, &record, &directory)
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing from the CAS"), "{error}");
        assert!(!directory.exists(), "partial restore must write nothing");
    }
}
