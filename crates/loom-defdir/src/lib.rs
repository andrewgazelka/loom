//! Definitions as files: the directory form that goes in git (`docs/design/definitions-in-git.md`).
//!
//! ```text
//! defs/
//!   loom.lock            generated: toolchain hash and name -> definition hash, sorted
//!   surface/lib.rs       one directory per definition (its name; `a/b` nests)
//!   surface/def.toml     deps = { height = "terrain/height" }, allowed_effects = [...]
//!   surface/Cargo.toml   optional (with Cargo.lock): crates.io dependencies
//!   terrain/height/lib.rs
//! ```
//!
//! This crate only reads and writes that layout. The daemon's `export_defs` and `import_defs` verbs take and
//! give [`Doc`]s, so the server never touches the client's files and any client (CLI, MCP, a script) can use
//! the same documents.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

/// One definition as the verbs carry it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Doc {
    pub name: String,
    pub source: String,
    #[serde(default)]
    pub deps: BTreeMap<String, String>,
    #[serde(default)]
    pub allowed_effects: Option<Vec<String>>,
    #[serde(default)]
    pub manifest: Option<String>,
    #[serde(default)]
    pub lock: Option<String>,
}

/// `loom.lock`: what the directory's definitions hashed to when it was written, and with which compiler. A
/// different compiler can legitimately change hashes, so a mismatch is a warning that names both.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lock {
    #[serde(default)]
    pub toolchain: String,
    #[serde(default)]
    pub definitions: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Default)]
struct DefToml {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    deps: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allowed_effects: Option<Vec<String>>,
}

const LOCK_FILE: &str = "loom.lock";
const SOURCE_FILE: &str = "lib.rs";
const META_FILE: &str = "def.toml";

/// A definition name is a relative path of plain segments.
fn check_name(name: &str) -> Result<()> {
    ensure!(!name.is_empty(), "a definition name is empty");
    for segment in name.split('/') {
        ensure!(
            !segment.is_empty()
                && segment != "."
                && segment != ".."
                && !segment.starts_with('.')
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')),
            "definition name {name:?}: each segment is letters, digits, '-' or '_' and does not start with '.'"
        );
    }
    Ok(())
}

/// Every definition under `root`, sorted by name, and its lock if there is one. A directory is a definition
/// when it holds `lib.rs`; anything else in the tree is ignored except the files named above.
pub fn read_dir(root: &Path) -> Result<(Vec<Doc>, Option<Lock>)> {
    let mut docs = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = fs::read_dir(&directory).with_context(|| format!("read {}", directory.display()))?;
        for entry in entries {
            let entry = entry?;
            let path = entry.path();
            if entry.file_type()?.is_dir() {
                if !entry.file_name().to_string_lossy().starts_with('.') {
                    stack.push(path);
                }
                continue;
            }
            if path.file_name().is_some_and(|name| name == SOURCE_FILE) && directory != root {
                docs.push(read_doc(root, &directory)?);
            }
        }
    }
    docs.sort_by(|a, b| a.name.cmp(&b.name));
    let lock = match fs::read_to_string(root.join(LOCK_FILE)) {
        Ok(text) => Some(toml::from_str::<Lock>(&text).with_context(|| format!("parse {LOCK_FILE}"))?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    Ok((docs, lock))
}

fn read_doc(root: &Path, directory: &Path) -> Result<Doc> {
    let name = directory
        .strip_prefix(root)?
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    check_name(&name)?;
    let source = fs::read_to_string(directory.join(SOURCE_FILE))
        .with_context(|| format!("read {name}/{SOURCE_FILE}"))?;
    let meta: DefToml = match fs::read_to_string(directory.join(META_FILE)) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("parse {name}/{META_FILE}"))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DefToml::default(),
        Err(error) => return Err(error.into()),
    };
    let optional = |file: &str| -> Result<Option<String>> {
        match fs::read_to_string(directory.join(file)) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("read {name}/{file}")),
        }
    };
    let manifest = optional("Cargo.toml")?;
    let lock = optional("Cargo.lock")?;
    Ok(Doc {
        name,
        source,
        deps: meta.deps,
        allowed_effects: meta.allowed_effects,
        manifest,
        lock,
    })
}

/// Write `docs` under `root` and, if given, `lock` (sorted, one line per name). Files of a definition that the
/// documents no longer carry (a removed `Cargo.toml`, `def.toml`) are removed so the directory is exactly what
/// the documents say; other definitions already in the directory are left alone.
pub fn write_dir(root: &Path, docs: &[Doc], lock: Option<&Lock>) -> Result<()> {
    for doc in docs {
        check_name(&doc.name)?;
        let directory = root.join(&doc.name);
        fs::create_dir_all(&directory).with_context(|| format!("create {}", directory.display()))?;
        fs::write(directory.join(SOURCE_FILE), &doc.source)?;
        let meta = DefToml { deps: doc.deps.clone(), allowed_effects: doc.allowed_effects.clone() };
        let meta_text = toml::to_string(&meta)?;
        sync(&directory.join(META_FILE), (!meta_text.is_empty()).then_some(meta_text.as_str()))?;
        sync(&directory.join("Cargo.toml"), doc.manifest.as_deref())?;
        sync(&directory.join("Cargo.lock"), doc.lock.as_deref())?;
    }
    if let Some(lock) = lock {
        fs::write(root.join(LOCK_FILE), toml::to_string(lock)?)?;
    }
    Ok(())
}

fn sync(path: &Path, content: Option<&str>) -> Result<()> {
    match content {
        Some(text) => fs::write(path, text)?,
        None => match fs::remove_file(path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        },
    }
    Ok(())
}

/// How a directory compares with a daemon, per name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Row {
    pub name: String,
    /// `same`, `changed` (the daemon's source or dependencies differ), `new` (only in the directory) or
    /// `removed` (only on the daemon).
    pub state: &'static str,
    /// The hash the lock recorded for this name, when it did and the daemon's differs.
    pub lock: Option<String>,
}

/// Compare the directory's documents with the daemon's (`export_defs` of the same names).
pub fn status(directory: &[Doc], daemon: &[Doc], lock: Option<&Lock>, daemon_hashes: &BTreeMap<String, String>) -> Vec<Row> {
    let mut rows = Vec::new();
    let local: BTreeMap<&str, &Doc> = directory.iter().map(|doc| (doc.name.as_str(), doc)).collect();
    let remote: BTreeMap<&str, &Doc> = daemon.iter().map(|doc| (doc.name.as_str(), doc)).collect();
    for (name, doc) in &local {
        let state = match remote.get(name) {
            None => "new",
            Some(other) if same(doc, other) => "same",
            Some(_) => "changed",
        };
        let lock_hash = lock.and_then(|lock| lock.definitions.get(*name)).filter(|recorded| {
            daemon_hashes.get(*name).is_some_and(|hash| hash != *recorded)
        });
        rows.push(Row { name: name.to_string(), state, lock: lock_hash.cloned() });
    }
    for name in remote.keys().filter(|name| !local.contains_key(*name)) {
        rows.push(Row { name: name.to_string(), state: "removed", lock: None });
    }
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    rows
}

/// Whether two documents describe the same definition input (the name aside).
pub fn same(a: &Doc, b: &Doc) -> bool {
    a.source == b.source
        && a.deps == b.deps
        && a.allowed_effects == b.allowed_effects
        && a.manifest == b.manifest
        && a.lock == b.lock
}

/// `path` as an absolute directory that exists, for CLI messages.
pub fn existing(path: &Path) -> Result<PathBuf> {
    let absolute = fs::canonicalize(path).with_context(|| format!("{} does not exist", path.display()))?;
    if !absolute.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    Ok(absolute)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(name: &str, source: &str) -> Doc {
        Doc { name: name.into(), source: source.into(), deps: BTreeMap::new(), allowed_effects: None, manifest: None, lock: None }
    }

    #[test]
    fn a_directory_round_trips_with_nesting_deps_policy_and_crates() {
        let directory = tempfile::tempdir().unwrap();
        let mut height = doc("terrain/height", "pub fn height(x: f32) -> f32 { x }\n");
        height.manifest = Some("[package]\nname = \"cell\"\n".into());
        height.lock = Some("version = 4\n".into());
        let mut surface = doc("surface", "pub fn f() -> f32 { height::height(1.0) }\n");
        surface.deps.insert("height".into(), "terrain/height".into());
        surface.allowed_effects = Some(vec![]);
        let lock = Lock {
            toolchain: "abc".into(),
            definitions: BTreeMap::from([("surface".into(), "11".into()), ("terrain/height".into(), "22".into())]),
        };
        write_dir(directory.path(), &[surface.clone(), height.clone()], Some(&lock)).unwrap();
        let (read, read_lock) = read_dir(directory.path()).unwrap();
        assert_eq!(read, vec![surface.clone(), height.clone()], "sorted by name, everything back byte for byte");
        assert_eq!(read_lock, Some(lock));
        // The lock file is sorted text, so it merges.
        let text = std::fs::read_to_string(directory.path().join("loom.lock")).unwrap();
        assert!(text.find("surface").unwrap() < text.find("terrain/height").unwrap(), "{text}");
        // Dropping the crate files and the policy removes their files, not just their contents.
        let mut plain = height.clone();
        plain.manifest = None;
        plain.lock = None;
        write_dir(directory.path(), &[plain.clone()], None).unwrap();
        assert!(!directory.path().join("terrain/height/Cargo.toml").exists());
        assert_eq!(read_dir(directory.path()).unwrap().0.len(), 2, "the other definition was left alone");
    }

    #[test]
    fn names_that_escape_the_directory_or_hide_are_refused() {
        let directory = tempfile::tempdir().unwrap();
        for bad in ["../x", "a/../b", "/abs", ".hidden", "a//b", "", "has space", "a/."] {
            assert!(write_dir(directory.path(), &[doc(bad, "x")], None).is_err(), "{bad:?}");
        }
        assert!(read_dir(&directory.path().join("missing")).is_err());
    }

    #[test]
    fn status_says_same_changed_new_removed_and_when_the_lock_disagrees() {
        let a = doc("a", "1");
        let b = doc("b", "2");
        let mut b_changed = b.clone();
        b_changed.source = "3".into();
        let only_local = doc("c", "4");
        let only_daemon = doc("d", "5");
        let lock = Lock { toolchain: "t".into(), definitions: BTreeMap::from([("a".into(), "old".into())]) };
        let hashes = BTreeMap::from([("a".into(), "new".into())]);
        let rows = status(
            &[a.clone(), b, only_local],
            &[a, b_changed, only_daemon],
            Some(&lock),
            &hashes,
        );
        let states: Vec<_> = rows.iter().map(|r| (r.name.as_str(), r.state)).collect();
        assert_eq!(states, [("a", "same"), ("b", "changed"), ("c", "new"), ("d", "removed")]);
        assert_eq!(rows[0].lock.as_deref(), Some("old"), "a hashes differently from what the lock recorded");
    }
}
