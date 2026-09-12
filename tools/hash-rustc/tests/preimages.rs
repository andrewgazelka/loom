use std::path::Path;
use std::process::{Command, Output};

fn compile(directory: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
        .current_dir(directory)
        .args([
            "fixture.rs",
            "--crate-name=fixture",
            "--crate-type=rlib",
            "--edition=2024",
            "-Awarnings",
        ])
        .env("LOOM_ITEM_HASHES", directory.join("hashes.json"))
        .env("LOOM_ITEM_PREIMAGES", directory.join("preimages"))
        .output()
        .unwrap()
}

#[test]
fn preimage_rehashes_to_item_hash() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("fixture.rs"),
        r#"
        const VALUE: u32 = 5;
        const _: () = ();
        const _: () = ();
        struct A;
        impl A { fn value(&self) -> u32 { VALUE } fn duplicate(&self) -> u32 { VALUE } }
        fn a(n: u32) -> u32 { if n == 0 { VALUE } else { b(n - 1) } }
        fn b(n: u32) -> u32 { if n == 0 { 1 } else { a(n - 1) } }
        fn recursive(n: u32) -> u32 { if n == 0 { 0 } else { recursive(n - 1) } }
        fn duplicate_one() -> u32 { 17 }
        fn duplicate_two() -> u32 { 17 }
        pub fn entry(n: u32) -> u32 { a(n) + A.value() + core::cmp::min(n, VALUE) }
    "#,
    )
    .unwrap();
    let output = compile(directory.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("hashes.json")).unwrap())
            .unwrap();
    let items = document["items"].as_object().unwrap();
    assert!(items.contains_key("_"));
    assert!(items.contains_key("_#1"));
    let mut cycles_checked = 0;
    for (path, item) in items {
        let hash = item["hash"].as_str().unwrap();
        let preimage = std::fs::read(directory.path().join("preimages").join(hash)).unwrap();
        assert_eq!(blake3::hash(&preimage).to_hex().as_str(), hash, "{path}");
        if let Some(members) = item["cycle"].as_array() {
            assert_eq!(preimage.len(), 40, "cycle member preimage");
            let cycle_hash = blake3::Hash::from_bytes(preimage[..32].try_into().unwrap());
            let cycle = std::fs::read(
                directory
                    .path()
                    .join("preimages/cycles")
                    .join(cycle_hash.to_hex().as_str()),
            )
            .unwrap();
            assert_eq!(blake3::hash(&cycle), cycle_hash);
            let index = u64::from_le_bytes(preimage[32..].try_into().unwrap()) as usize;
            assert_eq!(members[index], path.as_str());
            let mut remaining = cycle.as_slice();
            for _ in members {
                let length = u64::from_le_bytes(remaining[..8].try_into().unwrap()) as usize;
                remaining = &remaining[8..];
                assert!(length > 0);
                remaining = &remaining[length..];
            }
            assert!(
                remaining.is_empty(),
                "cycle must contain exactly its member streams"
            );
            cycles_checked += 1;
        }
    }
    assert_eq!(
        cycles_checked, 7,
        "function recursion plus the ADT/impl/method cycle"
    );
    assert_eq!(
        items["duplicate_one"]["hash"],
        items["duplicate_two"]["hash"]
    );
    let repeated = compile(directory.path());
    assert!(
        repeated.status.success(),
        "identical preimages must be reusable"
    );

    let entry_hash = document["entry"]["entry"].as_str().unwrap();
    let path = directory.path().join("preimages").join(entry_hash);
    std::fs::write(&path, b"corrupt").unwrap();
    let failure = compile(directory.path());
    assert!(!failure.status.success());
    assert!(
        String::from_utf8_lossy(&failure.stderr).contains("existing preimage has different bytes")
    );
    assert!(!directory.path().join("hashes.json").exists());
    assert_eq!(
        std::fs::read(path).unwrap(),
        b"corrupt",
        "never overwrite conflicting CAS content"
    );
}
