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
#[serde(deny_unknown_fields)]
pub struct Lock {
    #[serde(default)]
    pub toolchain: String,
    #[serde(default)]
    pub definitions: BTreeMap<String, String>,
}

/// `def.toml`. Unknown keys are an error: a typo such as `allowed_effect` must not silently mean "no policy".
#[derive(Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct DefToml {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    deps: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    allowed_effects: Option<Vec<String>>,
}

const LOCK_FILE: &str = "loom.lock";
const SOURCE_FILE: &str = "lib.rs";
const META_FILE: &str = "def.toml";

/// Labels sorted and without repeats, the form the daemon stores a policy in.
pub fn normalized_labels(mut labels: Vec<String>) -> Vec<String> {
    labels.sort();
    labels.dedup();
    labels
}

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
    let lock = match read_regular(&root.join(LOCK_FILE))? {
        Some(text) => Some(toml::from_str::<Lock>(&text).with_context(|| format!("parse {LOCK_FILE}"))?),
        None => None,
    };
    Ok((docs, lock))
}

/// The text of a regular file. A symlink (or anything else) is refused: a cloned repository must not be able to
/// make `import-dir` upload `~/.ssh/id_rsa` by linking `lib.rs` to it.
fn read_regular(path: &Path) -> Result<Option<String>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(Some(
            fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?,
        )),
        Ok(_) => bail!("{} is not a regular file (symlinks are not followed)", path.display()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

fn read_doc(root: &Path, directory: &Path) -> Result<Doc> {
    let name = directory
        .strip_prefix(root)?
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/");
    check_name(&name)?;
    let source = read_regular(&directory.join(SOURCE_FILE))?
        .with_context(|| format!("{name}/{SOURCE_FILE} is missing"))?;
    let meta: DefToml = match read_regular(&directory.join(META_FILE))? {
        Some(text) => toml::from_str(&text).with_context(|| format!("parse {name}/{META_FILE}"))?,
        None => DefToml::default(),
    };
    let optional = |file: &str| -> Result<Option<String>> { read_regular(&directory.join(file)) };
    let manifest = optional("Cargo.toml")?;
    let lock = optional("Cargo.lock")?;
    Ok(Doc {
        name,
        source,
        deps: meta.deps,
        allowed_effects: meta.allowed_effects.map(normalized_labels),
        manifest,
        lock,
    })
}

/// Write `docs` under `root`. Everything is validated before the first byte is written: each name, and names
/// that differ only by case (they would share a directory on a case-insensitive filesystem and overwrite each
/// other). No path component under `root` may be a symlink, and each file is written to a temporary name and
/// renamed into place, so a link that is already there is replaced, never written through. Files of a definition
/// that the documents no longer carry (a removed `Cargo.toml` or `def.toml`) are removed; other definitions already
/// in the directory are left alone. The lock is separate ([`write_lock`]).
pub fn write_dir(root: &Path, docs: &[Doc]) -> Result<()> {
    let mut lowered: BTreeMap<String, &str> = BTreeMap::new();
    for doc in docs {
        check_name(&doc.name)?;
        if let Some(other) = lowered.insert(doc.name.to_lowercase(), &doc.name) {
            ensure!(
                other == doc.name,
                "definitions {other:?} and {:?} differ only by case and would collide on a case-insensitive filesystem",
                doc.name
            );
        }
    }
    for doc in docs {
        let directory = safe_directory(root, &doc.name)?;
        replace_file(&directory.join(SOURCE_FILE), Some(&doc.source))?;
        let meta = DefToml { deps: doc.deps.clone(), allowed_effects: doc.allowed_effects.clone() };
        let meta_text = toml::to_string(&meta)?;
        replace_file(&directory.join(META_FILE), (!meta_text.is_empty()).then_some(meta_text.as_str()))?;
        replace_file(&directory.join("Cargo.toml"), doc.manifest.as_deref())?;
        replace_file(&directory.join("Cargo.lock"), doc.lock.as_deref())?;
    }
    Ok(())
}

/// Write `loom.lock`. With `merge`, names already in the file that `lock` does not mention keep their entries
/// (exporting a subset must not drop the rest); the toolchain is the newest one written.
pub fn write_lock(root: &Path, lock: &Lock, merge: bool) -> Result<()> {
    let mut merged = lock.clone();
    if merge && let Some(existing) = read_regular(&root.join(LOCK_FILE))? {
        let old: Lock = toml::from_str(&existing).with_context(|| format!("parse {LOCK_FILE}"))?;
        for (name, hash) in old.definitions {
            merged.definitions.entry(name).or_insert(hash);
        }
    }
    if let Ok(metadata) = fs::symlink_metadata(root) {
        ensure!(!metadata.file_type().is_symlink() || root.is_dir(), "{} is not a directory", root.display());
    }
    replace_file(&root.join(LOCK_FILE), Some(&toml::to_string(&merged)?))
}

/// `root/name` as a directory that exists, created segment by segment, refusing a symlink at any step.
fn safe_directory(root: &Path, name: &str) -> Result<PathBuf> {
    fs::create_dir_all(root).with_context(|| format!("create {}", root.display()))?;
    let mut path = root.to_path_buf();
    for segment in name.split('/') {
        path.push(segment);
        match fs::symlink_metadata(&path) {
            Ok(metadata) => ensure!(
                metadata.file_type().is_dir(),
                "{} is not a plain directory (symlinks are not followed)",
                path.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir(&path).with_context(|| format!("create {}", path.display()))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(path)
}

static TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Make `path` hold `content` (or not exist, for `None`): a temporary file renamed over the target, so an
/// existing symlink at `path` is replaced rather than written through.
fn replace_file(path: &Path, content: Option<&str>) -> Result<()> {
    let Some(text) = content else {
        return match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).with_context(|| format!("remove {}", path.display())),
        };
    };
    let temporary = path.with_extension(format!(
        "loom-tmp-{}-{}",
        std::process::id(),
        TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .with_context(|| format!("create {}", temporary.display()))?;
    std::io::Write::write_all(&mut file, text.as_bytes())?;
    drop(file);
    fs::rename(&temporary, path).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })?;
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
        write_dir(directory.path(), &[surface.clone(), height.clone()]).unwrap();
        write_lock(directory.path(), &lock, false).unwrap();
        let (read, read_lock) = read_dir(directory.path()).unwrap();
        assert_eq!(read, vec![surface.clone(), height.clone()], "sorted by name, everything back byte for byte");
        assert_eq!(read_lock, Some(lock));
        // The lock file is sorted text, so it merges.
        let text = std::fs::read_to_string(directory.path().join("loom.lock")).unwrap();
        assert!(text.find("surface").unwrap() < text.find("terrain/height").unwrap(), "{text}");
        // Dropping the crate files removes their files, not just their contents.
        let mut plain = height.clone();
        plain.manifest = None;
        plain.lock = None;
        write_dir(directory.path(), &[plain.clone()]).unwrap();
        assert!(!directory.path().join("terrain/height/Cargo.toml").exists());
        assert_eq!(read_dir(directory.path()).unwrap().0.len(), 2, "the other definition was left alone");
    }

    #[test]
    fn exporting_a_subset_keeps_the_lock_entries_of_the_rest_unless_asked_to_replace() {
        let directory = tempfile::tempdir().unwrap();
        let full = Lock {
            toolchain: "t1".into(),
            definitions: BTreeMap::from([("a".into(), "1".into()), ("b".into(), "2".into())]),
        };
        write_lock(directory.path(), &full, false).unwrap();
        let subset = Lock { toolchain: "t2".into(), definitions: BTreeMap::from([("a".into(), "9".into())]) };
        write_lock(directory.path(), &subset, true).unwrap();
        let (_, merged) = read_dir(directory.path()).unwrap();
        let merged = merged.unwrap();
        assert_eq!(merged.definitions, BTreeMap::from([("a".into(), "9".into()), ("b".into(), "2".into())]));
        assert_eq!(merged.toolchain, "t2");
        write_lock(directory.path(), &subset, false).unwrap();
        assert_eq!(read_dir(directory.path()).unwrap().1.unwrap().definitions.len(), 1, "a full export replaces");
    }

    #[test]
    fn names_that_escape_hide_or_collide_are_refused_before_anything_is_written() {
        let directory = tempfile::tempdir().unwrap();
        for bad in ["../x", "a/../b", "/abs", ".hidden", "a//b", "", "has space", "a/.", "v1.2"] {
            assert!(write_dir(directory.path(), &[doc(bad, "x")]).is_err(), "{bad:?}");
        }
        // One bad name among good ones writes nothing at all.
        assert!(write_dir(directory.path(), &[doc("good", "x"), doc("bad name", "y")]).is_err());
        assert!(!directory.path().join("good").exists(), "validation happens before the first write");
        // Two names that differ only by case would overwrite each other on macOS.
        let error = write_dir(directory.path(), &[doc("Foo", "1"), doc("foo", "2")]).unwrap_err();
        assert!(error.to_string().contains("only by case"), "{error:#}");
        assert!(!directory.path().join("Foo").exists());
        assert!(read_dir(&directory.path().join("missing")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_never_followed_on_read_or_write() {
        use std::os::unix::fs::symlink;
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret");
        std::fs::write(&secret, "TOP SECRET").unwrap();
        // A repository that links lib.rs to a file outside it: reading is refused, not uploaded.
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("d")).unwrap();
        symlink(&secret, directory.path().join("d/lib.rs")).unwrap();
        let error = read_dir(directory.path()).unwrap_err();
        assert!(format!("{error:#}").contains("symlink"), "{error:#}");
        // A linked definition directory is skipped on read, and refused on write.
        let linked = tempfile::tempdir().unwrap();
        symlink(outside.path(), linked.path().join("escape")).unwrap();
        assert!(read_dir(linked.path()).unwrap().0.is_empty());
        assert!(write_dir(linked.path(), &[doc("escape", "pub fn f() {}\n")]).is_err());
        assert!(!outside.path().join("lib.rs").exists(), "nothing was written through the link");
        // A link where a file goes is replaced, not written through.
        let replaced = tempfile::tempdir().unwrap();
        std::fs::create_dir(replaced.path().join("d")).unwrap();
        symlink(&secret, replaced.path().join("d/lib.rs")).unwrap();
        write_dir(replaced.path(), &[doc("d", "pub fn f() {}\n")]).unwrap();
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "TOP SECRET", "the target was not touched");
        assert_eq!(std::fs::read_to_string(replaced.path().join("d/lib.rs")).unwrap(), "pub fn f() {}\n");
        // And a linked loom.lock is replaced too.
        symlink(&secret, replaced.path().join("loom.lock")).unwrap();
        write_lock(replaced.path(), &Lock::default(), false).unwrap();
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "TOP SECRET");
    }

    #[test]
    fn a_misspelled_key_in_def_toml_or_the_lock_is_an_error_not_a_missing_policy() {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("d")).unwrap();
        std::fs::write(directory.path().join("d/lib.rs"), "x").unwrap();
        std::fs::write(directory.path().join("d/def.toml"), "allowed_effect = []\n").unwrap();
        let error = read_dir(directory.path()).unwrap_err();
        assert!(format!("{error:#}").contains("allowed_effect"), "{error:#}");
        std::fs::write(directory.path().join("d/def.toml"), "allowed_effects = [\"b\", \"a\", \"a\"]\n").unwrap();
        let (docs, _) = read_dir(directory.path()).unwrap();
        assert_eq!(docs[0].allowed_effects, Some(vec!["a".to_string(), "b".to_string()]), "sorted, without repeats");
        std::fs::write(directory.path().join("loom.lock"), "tolchain = \"x\"\n").unwrap();
        assert!(read_dir(directory.path()).is_err());
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
