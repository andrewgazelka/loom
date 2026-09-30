//! Raw blobs of 1 MiB and up are immutable files under `objects/`; smaller ones stay in SQLite.
use anyhow::{Context, Result};
use loom_proto::{CasListRequest, Def, Lang};
use loom_store::{IntakePublication, Store, content_hash};
use serde_json::json;
use std::{
    collections::BTreeMap,
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, SystemTime},
};

const MIB: usize = 1 << 20;

fn pattern(length: usize) -> Vec<u8> {
    (0..length)
        .map(|i| (i.wrapping_mul(31) % 251) as u8)
        .collect()
}

/// Every object file under `<directory>/objects/<shard>/`, not counting `tmp/`.
fn object_files(directory: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for shard in std::fs::read_dir(directory.join("objects"))? {
        let shard = shard?;
        if shard.file_name() == "tmp" {
            continue;
        }
        for file in std::fs::read_dir(shard.path())? {
            files.push(file?.path());
        }
    }
    Ok(files)
}

fn object_path(directory: &Path, hash: &str) -> PathBuf {
    directory.join("objects").join(&hash[..1]).join(hash)
}

/// Overwrite the first byte of an object file in place, as a same-uid writer could, keeping its size.
fn corrupt_in_place(path: &Path) -> Result<()> {
    std::thread::sleep(Duration::from_millis(50));
    let mut permissions = std::fs::metadata(path)?.permissions();
    permissions.set_readonly(false);
    std::fs::set_permissions(path, permissions)?;
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)?
        .write_all(b"X")?;
    Ok(())
}

fn list_size(store: &Store, hash: &str) -> Result<u64> {
    let page = store.cas_list(&CasListRequest {
        limit: 100,
        after: None,
        kind: None,
        q: None,
    })?;
    Ok(page
        .items
        .iter()
        .find(|entry| entry.hash == hash)
        .context("object missing from the CAS listing")?
        .size)
}

#[test]
fn large_blobs_are_single_files_and_small_ones_stay_inline() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path().join("store.db"))?;
    let big = pattern(2 * MIB);
    let hash = store.put("blob", &big)?;
    assert_eq!(hash, content_hash(&big));
    assert_eq!(store.put("blob", &big)?, hash, "a second put is idempotent");
    let expected = object_path(directory.path(), &hash);
    assert_eq!(object_files(directory.path())?, vec![expected.clone()]);
    assert_eq!(std::fs::metadata(&expected)?.len(), big.len() as u64);
    assert!(
        std::fs::metadata(&expected)?.permissions().readonly(),
        "object files are immutable"
    );
    assert_eq!(store.get(&hash)?.as_deref(), Some(big.as_slice()));

    let small = pattern(100 * 1024);
    let small_hash = store.put("blob", &small)?;
    assert_eq!(
        object_files(directory.path())?.len(),
        1,
        "100 KB stays inline"
    );
    assert_eq!(store.get(&small_hash)?.as_deref(), Some(small.as_slice()));

    // The threshold is exact: 1 MiB is a file, one byte less is not.
    let edge = store.put("blob", &pattern(MIB))?;
    assert_eq!(object_files(directory.path())?.len(), 2);
    store.put("blob", &pattern(MIB - 1))?;
    assert_eq!(object_files(directory.path())?.len(), 2);
    assert!(object_path(directory.path(), &edge).exists());

    // Sizes and previews come from the index and the file, not from `length(bytes)`.
    assert_eq!(store.size_of(&hash)?, Some(big.len() as u64));
    assert_eq!(store.size_of(&small_hash)?, Some(small.len() as u64));
    assert_eq!(store.size_of(&"0".repeat(64))?, None);
    assert_eq!(
        store.cas_entry(&hash)?.context("entry")?.size,
        big.len() as u64
    );
    assert_eq!(list_size(&store, &hash)?, big.len() as u64);
    assert_eq!(list_size(&store, &small_hash)?, small.len() as u64);
    assert_eq!(store.cas_prefix(&hash, 16)?, Some(big[..16].to_vec()));
    assert_eq!(
        store.cas_prefix(&small_hash, 16)?,
        Some(small[..16].to_vec())
    );
    let reference = store.reference(&hash, loom_proto::RAW_CODEC)?;
    assert!(
        store
            .guest_cas_effect("cas.get_bytes", json!({"reference": reference}))
            .is_err(),
        "a spilled object is over the guest limit"
    );
    store.flush()?;
    Ok(())
}

#[test]
fn a_reopened_store_sees_the_blob_and_a_missing_file_is_a_clear_error() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("store.db");
    let big = pattern(3 * MIB + 5);
    let hash = {
        let store = Store::open(&database)?;
        let hash = store.put("blob", &big)?;
        store.flush()?;
        hash
    };
    let store = Store::open(&database)?;
    assert_eq!(store.get(&hash)?.as_deref(), Some(big.as_slice()));
    drop(store);

    std::fs::remove_file(object_path(directory.path(), &hash))?;
    let store = Store::open(&database)?;
    let error = store
        .get(&hash)
        .expect_err("missing file must not read as empty");
    assert!(format!("{error:#}").contains("missing"), "{error:#}");
    assert!(
        store
            .restore_to(&hash, &directory.path().join("out"))
            .is_err()
    );
    assert_eq!(
        store.size_of(&hash)?,
        Some(big.len() as u64),
        "the index still knows it"
    );
    assert!(
        !store.has_object(&hash)?,
        "but the object is not usable without its file"
    );
    assert!(!store.has_object(&"0".repeat(64))?);
    // Putting the same bytes again rewrites the missing file.
    store.put("blob", &big)?;
    assert_eq!(store.get(&hash)?.as_deref(), Some(big.as_slice()));
    Ok(())
}

#[test]
fn streamed_files_spill_once_and_an_old_store_keeps_its_inline_rows() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let source = directory.path().join("source");
    let big = pattern(2 * MIB + 7);
    std::fs::write(&source, &big)?;
    let store = Store::open(directory.path().join("store.db"))?;
    let hash = store.put_file("uploaded_file", &source)?;
    assert_eq!(store.put_file("uploaded_file", &source)?, hash);
    assert_eq!(
        store.put("uploaded_file", &big)?,
        hash,
        "put and put_file address alike"
    );
    assert_eq!(object_files(directory.path())?.len(), 1);
    assert_eq!(store.get(&hash)?.as_deref(), Some(big.as_slice()));
    assert_eq!(store.size_of(&hash)?, Some(big.len() as u64));
    drop(store);

    // A store created before spilling: no external/size columns, one large inline row.
    let old = directory.path().join("old");
    std::fs::create_dir(&old)?;
    let database = old.join("store.db");
    let inline = pattern(2 * MIB + 11);
    let inline_hash = content_hash(&inline);
    let connection = rusqlite::Connection::open(&database)?;
    connection.execute_batch(
        "CREATE TABLE cas(hash TEXT PRIMARY KEY,kind TEXT NOT NULL,bytes BLOB NOT NULL,created_at INTEGER NOT NULL,codec INTEGER NOT NULL CHECK(codec IN (85,113)));",
    )?;
    connection.execute(
        "INSERT INTO cas VALUES (?,?,?,0,85)",
        rusqlite::params![inline_hash, "blob", inline],
    )?;
    drop(connection);
    let store = Store::open(&database)?;
    assert_eq!(store.get(&inline_hash)?.as_deref(), Some(inline.as_slice()));
    assert_eq!(store.size_of(&inline_hash)?, Some(inline.len() as u64));
    assert_eq!(store.put("blob", &inline)?, inline_hash);
    assert!(
        object_files(&old)?.is_empty(),
        "an existing inline row wins; no duplicate file"
    );
    store.put("blob", &pattern(2 * MIB + 13))?;
    assert_eq!(object_files(&old)?.len(), 1, "new large blobs spill");
    Ok(())
}

#[test]
fn restore_to_replaces_atomically_and_matches_the_stored_bytes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path().join("store.db"))?;
    let big = pattern(2 * MIB);
    let hash = store.put("blob", &big)?;
    let small = pattern(4096);
    let small_hash = store.put("blob", &small)?;

    let out = directory.path().join("restored");
    store.restore_to(&hash, &out)?;
    assert_eq!(std::fs::read(&out)?, big);
    // The destination exists: stale content, then a previous restore.
    let existing = directory.path().join("existing");
    std::fs::write(&existing, b"stale")?;
    store.restore_to(&hash, &existing)?;
    assert_eq!(std::fs::read(&existing)?, big);
    store.restore_to(&hash, &existing)?;
    assert_eq!(std::fs::read(&existing)?, big);
    // A CID works, and an inline value is written as bytes.
    let reference = store.reference(&small_hash, loom_proto::RAW_CODEC)?;
    let cid = reference["$ref"].as_str().context("cid")?;
    store.restore_to(cid, &existing)?;
    assert_eq!(std::fs::read(&existing)?, small);
    // The store's own file is unaffected by replacing a restored copy.
    assert_eq!(store.get(&hash)?.as_deref(), Some(big.as_slice()));
    let leftovers: Vec<_> = std::fs::read_dir(directory.path())?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "no temp files left beside the destination"
    );

    assert!(
        store.restore_to(&"0".repeat(64), &out).is_err(),
        "unknown object"
    );
    let structured = store.put_value("blob", &json!({"a": 1}))?;
    assert!(
        store.restore_to(&structured, &out).is_err(),
        "DAG-CBOR is not a file"
    );
    Ok(())
}

#[test]
fn intake_moves_spilled_blobs_as_files() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let open = |name: &str| -> Result<(PathBuf, Store)> {
        let path = directory.path().join(name);
        std::fs::create_dir(&path)?;
        let store = Store::open(path.join("store.db"))?;
        Ok((path, store))
    };
    let definition = |seed: &[u8]| Def {
        hash: content_hash(seed),
        lang: Lang::Rust,
        component_hash: None,
        sig: Default::default(),
        allowed_effects: None,
        observed_effects: Vec::new(),
    };
    let deps = BTreeMap::new();
    let (a_path, a) = open("a")?;
    let (b_path, b) = open("b")?;
    let big = pattern(2 * MIB + 3);
    let hash = a.put("blob", &big)?;

    // Staged from A, committed into another store: the file is copied (hashed on the way), not linked.
    let staged = a.stage_intake()?;
    assert_eq!(staged.get(&hash)?.as_deref(), Some(big.as_slice()));
    let def = definition(b"into b");
    b.commit_intake(
        &staged,
        IntakePublication {
            def: &def,
            name: Some("built"),
            source: "source",
            deps: &deps,
            identity: None,
            build_event: &json!({"type": "component_built"}),
        },
    )?;
    assert_eq!(b.get(&hash)?.as_deref(), Some(big.as_slice()));
    assert_eq!(object_files(&b_path)?, vec![object_path(&b_path, &hash)]);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_eq!(
            std::fs::metadata(object_path(&b_path, &hash))?.nlink(),
            1,
            "an independent copy: no inode is shared with the source store"
        );
    }
    assert_eq!(a.get(&hash)?.as_deref(), Some(big.as_slice()));
    b.flush()?;

    // A large object built in the staged store lands in the live directory once.
    let staged = a.stage_intake()?;
    let built = pattern(2 * MIB + 9);
    let built_hash = staged.put("component", &built)?;
    let def = definition(b"into a");
    a.commit_intake(
        &staged,
        IntakePublication {
            def: &def,
            name: Some("built"),
            source: "source",
            deps: &deps,
            identity: None,
            build_event: &json!({"type": "component_built"}),
        },
    )?;
    assert_eq!(a.get(&built_hash)?.as_deref(), Some(built.as_slice()));
    assert_eq!(object_files(&a_path)?.len(), 2);
    Ok(())
}

#[test]
fn opening_sweeps_stale_temp_files_only() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let database = directory.path().join("store.db");
    drop(Store::open(&database)?);
    let temp = directory.path().join("objects").join("tmp");
    let stale = temp.join("stale");
    let fresh = temp.join("fresh");
    std::fs::write(&stale, b"left by a dead writer")?;
    std::fs::write(&fresh, b"another process is writing")?;
    File::options()
        .write(true)
        .open(&stale)?
        .set_modified(SystemTime::now() - Duration::from_secs(2 * 3600))?;
    drop(Store::open(&database)?);
    assert!(!stale.exists());
    assert!(fresh.exists());
    Ok(())
}

#[test]
fn documents_read_inside_sql_stay_inline_whatever_their_size() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path().join("s.sqlite"))?;
    // An item document is read with json_each over `cas.bytes`: an external row's empty bytes would
    // make every later lookup fail (found by a 142 KB definition whose item document passed 1 MiB).
    let document =
        serde_json::to_vec(&json!({"entry": {"main": "x".repeat(2 * MIB)}, "exports": ["main"]}))?;
    let hash = store.put("item-hashes", &document)?;
    assert!(
        object_files(directory.path())?.is_empty(),
        "no file for a document kind"
    );
    assert_eq!(store.get(&hash)?.as_deref(), Some(document.as_slice()));
    let opaque = store.put("component", &pattern(2 * MIB))?;
    assert!(
        object_path(directory.path(), &opaque).exists(),
        "an opaque kind still spills"
    );
    Ok(())
}

#[test]
fn a_same_size_corruption_of_a_verified_file_is_caught_and_a_reput_repairs_it() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path().join("store.db"))?;
    let big = pattern(2 * MIB);
    let hash = store.put("blob", &big)?;
    let file = object_path(directory.path(), &hash);
    // Verified once in this process; a hash-keyed "verified" flag would now wave corruption through.
    assert_eq!(store.get(&hash)?.as_deref(), Some(big.as_slice()));
    store.restore_to(&hash, &directory.path().join("before"))?;

    corrupt_in_place(&file)?;
    assert_eq!(
        std::fs::metadata(&file)?.len(),
        big.len() as u64,
        "same size"
    );
    let error = store
        .get(&hash)
        .expect_err("corrupt bytes must not be served");
    assert!(format!("{error:#}").contains("mismatch"), "{error:#}");
    let out = directory.path().join("after");
    assert!(store.restore_to(&hash, &out).is_err());
    assert!(!out.exists(), "a refused restore leaves nothing behind");

    // Putting the right bytes again replaces the corrupt file (the size check alone would not).
    assert_eq!(store.put("blob", &big)?, hash);
    assert_eq!(store.get(&hash)?.as_deref(), Some(big.as_slice()));
    store.restore_to(&hash, &out)?;
    assert_eq!(std::fs::read(&out)?, big);
    Ok(())
}

#[test]
fn a_restored_file_is_an_independent_writable_copy_of_the_store_file() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path().join("store.db"))?;
    let big = pattern(2 * MIB);
    let hash = store.put("blob", &big)?;
    let inline = store.put("blob", &pattern(4096))?;
    let stored = object_path(directory.path(), &hash);
    let out = directory.path().join("out");
    store.restore_to(&hash, &out)?;
    let small = directory.path().join("small");
    store.restore_to(&inline, &small)?;
    for restored in [&out, &small] {
        assert!(
            !std::fs::metadata(restored)?.permissions().readonly(),
            "{restored:?} is writable"
        );
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let (restored, original) = (std::fs::metadata(&out)?, std::fs::metadata(&stored)?);
        assert_ne!(restored.ino(), original.ino(), "no shared inode");
        assert_eq!(original.nlink(), 1, "the store file has no other names");
        assert_eq!(
            std::fs::metadata(&small)?.mode() & 0o777,
            restored.mode() & 0o777,
            "inline and spilled restores have the same mode"
        );
    }
    // A consumer writing through its copy cannot reach the store's bytes.
    std::fs::OpenOptions::new()
        .write(true)
        .open(&out)?
        .write_all(b"tampered")?;
    assert_eq!(std::fs::read(&stored)?, big);
    assert_eq!(store.get(&hash)?.as_deref(), Some(big.as_slice()));
    store.restore_to(&hash, &out)?;
    assert_eq!(std::fs::read(&out)?, big);
    Ok(())
}

#[test]
fn intake_refuses_a_corrupt_source_file_instead_of_carrying_it_over() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let open = |name: &str| -> Result<(PathBuf, Store)> {
        let path = directory.path().join(name);
        std::fs::create_dir(&path)?;
        let store = Store::open(path.join("store.db"))?;
        Ok((path, store))
    };
    let (a_path, a) = open("a")?;
    let (b_path, b) = open("b")?;
    let hash = a.put("blob", &pattern(2 * MIB + 3))?;
    corrupt_in_place(&object_path(&a_path, &hash))?;
    let staged = a.stage_intake()?;
    let def = Def {
        hash: content_hash(b"into b"),
        lang: Lang::Rust,
        component_hash: None,
        sig: Default::default(),
        allowed_effects: None,
        observed_effects: Vec::new(),
    };
    let deps = BTreeMap::new();
    let result = b.commit_intake(
        &staged,
        IntakePublication {
            def: &def,
            name: Some("built"),
            source: "source",
            deps: &deps,
            identity: None,
            build_event: &json!({"type": "component_built"}),
        },
    );
    assert!(result.is_err(), "a corrupt source object was imported");
    assert!(
        object_files(&b_path)?.is_empty(),
        "and nothing was left in the destination"
    );
    assert_eq!(b.get(&hash)?, None);
    Ok(())
}

#[test]
fn damage_is_a_typed_error_and_putting_the_object_again_heals_an_inline_row() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path().join("store.db"))?;
    let is_damage = |error: &anyhow::Error| loom_store::ObjectError::is_in(error);

    // A missing spilled file and a corrupt one are damage; an unrelated failure is not.
    let big = pattern(2 * MIB);
    let hash = store.put("blob", &big)?;
    let file = object_path(directory.path(), &hash);
    std::fs::remove_file(&file)?;
    assert!(is_damage(&store.get(&hash).unwrap_err()));
    assert!(is_damage(
        &store
            .restore_to(&hash, &directory.path().join("out"))
            .unwrap_err()
    ));
    store.put("blob", &big)?;
    corrupt_in_place(&file)?;
    assert!(is_damage(&store.get(&hash).unwrap_err()));
    let not_damage = store.get("not a hash").unwrap_err();
    assert!(!is_damage(&not_damage), "{not_damage:#}");

    // A corrupt inline row is damage too, and INSERT OR IGNORE used to keep it for ever.
    let small = pattern(4096);
    let small_hash = store.put("blob", &small)?;
    store.with_connection(|c| {
        Ok(c.execute(
            "UPDATE cas SET bytes=zeroblob(4096) WHERE hash=?",
            [&small_hash],
        )?)
    })?;
    assert!(is_damage(&store.get(&small_hash).unwrap_err()));
    assert!(!store.verify_object(&small_hash)?);
    store.put("blob", &small)?;
    assert_eq!(store.get(&small_hash)?.as_deref(), Some(small.as_slice()));
    assert!(store.verify_object(&small_hash)?);
    assert!(
        !store.verify_object(&hash)?,
        "the corrupt spilled file is not intact"
    );
    Ok(())
}

#[test]
fn map_object_maps_a_private_clone_so_the_store_file_cannot_change_what_a_reader_sees() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let store = Store::open(directory.path().join("store.db"))?;
    let big = pattern(3 * MIB + 123);
    let hash = store.put("blob", &big)?;
    let mapped = store.map_object(&hash)?.context("spilled object")?;
    assert!(mapped.is_file_backed());
    assert_eq!(&*mapped, big.as_slice());
    assert_eq!(mapped.hash(), hash);
    // A file mapped from offset 0 starts on a page boundary, and its padded length is a page multiple that
    // covers it (what a no-copy GPU buffer needs).
    let page = unsafe { libc_page() };
    assert_eq!(mapped.as_ptr() as usize % page, 0);
    let padded = mapped.page_aligned_len().context("file-backed")?;
    assert_eq!(padded % page, 0);
    assert!(padded >= big.len() && padded < big.len() + page);
    // The padding reads as zero (no SIGBUS inside the last page).
    let tail = unsafe { std::slice::from_raw_parts(mapped.as_ptr().add(big.len()), padded - big.len()) };
    assert!(tail.iter().all(|&b| b == 0));
    // No stray temp file is left behind: the private copy was unlinked at once.
    assert!(std::fs::read_dir(directory.path().join("objects").join("tmp"))?.next().is_none());

    // A length that is already a page multiple is not padded further.
    let exact = store.put("blob", &pattern(2 * MIB))?;
    let exact = store.map_object(&exact)?.context("exact")?;
    assert_eq!(exact.page_aligned_len(), Some(2 * MIB));

    // Small values are not files: an owned copy with no page alignment to offer.
    let small = pattern(10 * 1024);
    let small_hash = store.put("blob", &small)?;
    let copy = store.map_object(&small_hash)?.context("inline object")?;
    assert!(!copy.is_file_backed() && !copy.is_cloned());
    assert_eq!(copy.page_aligned_len(), None);
    assert_eq!(&*copy, small.as_slice());
    assert!(store.map_object(&"0".repeat(64))?.is_none());

    // A same-uid writer rewriting the store's object in place (size kept) does not change an existing
    // mapping: it is of a private copy. A fresh map refuses the corrupt file.
    corrupt_in_place(&object_path(directory.path(), &hash))?;
    assert_eq!(&*mapped, big.as_slice(), "the mapping is of its own inode");
    let error = store.map_object(&hash).err().context("corrupt file must not map")?;
    assert!(loom_store::ObjectError::is_in(&error), "{error:#}");

    // A missing file is the typed error too, and a wrong-kind hash is invisible to `of_kind`.
    let gone = store.put("blob", &pattern(MIB + 7))?;
    std::fs::remove_file(object_path(directory.path(), &gone))?;
    let error = store.map_object(&gone).err().context("missing file")?;
    assert!(loom_store::ObjectError::is_in(&error), "{error:#}");
    let other = store.put("component", &pattern(MIB + 9))?;
    assert!(store.map_object_of_kind(&other, "kernel-blob")?.is_none());
    assert!(store.map_object_of_kind(&other, "component")?.is_some());
    // A store reference whose length disagrees with the object is refused, not trusted.
    let real = store.put("blob", &pattern(MIB + 11))?;
    let mut hash_bytes = [0u8; 32];
    for (index, pair) in real.as_bytes().chunks(2).enumerate() {
        hash_bytes[index] = u8::from_str_radix(std::str::from_utf8(pair)?, 16)?;
    }
    let good = loom_proto::StoreRef { hash: hash_bytes, len: (MIB + 11) as u64 };
    assert!(store.map_store_ref(&good, None)?.is_some());
    let lying = loom_proto::StoreRef { len: 5, ..good };
    assert!(store.map_store_ref(&lying, None).is_err());
    Ok(())
}

unsafe fn libc_page() -> usize {
    unsafe extern "C" {
        fn getpagesize() -> i32;
    }
    unsafe { getpagesize() as usize }
}
