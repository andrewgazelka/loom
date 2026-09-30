//! rustc as the first real workload: one crate compiled to an rlib inside the sandbox, keyed on the
//! compiler binary and the source, answered from the store the second time.
//! Needs `LOOM_ACTION_SYSROOT` (a rustc sysroot, e.g. `rustc --print sysroot`).
use loom_action::{Action, Input, Runner};
use loom_store::Store;
use std::{collections::BTreeMap, path::PathBuf};

fn action(sysroot: &PathBuf, store: &Store, source: &str) -> Action {
    let rustc = sysroot.join("bin/rustc");
    Action {
        tool: rustc,
        tool_identity: "rustc-under-test".into(),
        runtime: vec![sysroot.clone()],
        args: [
            "--edition=2024",
            "--crate-type=rlib",
            "--crate-name=demo",
            "-Copt-level=1",
            "-Zremap-cwd-prefix=/work",
            "--remap-path-prefix=@ROOT@=/work",
            "--emit=link=libdemo.rlib",
            "src/lib.rs",
        ]
        .map(String::from)
        .to_vec(),
        env: BTreeMap::from([("TMPDIR".to_owned(), ".".to_owned())]),
        inputs: BTreeMap::from([(
            "src/lib.rs".to_owned(),
            Input {
                hash: store.put("blob", source.as_bytes()).unwrap(),
                executable: false,
            },
        )]),
        outputs: vec!["libdemo.rlib".into()],
        network: false,
    }
}

#[tokio::test]
#[ignore = "needs LOOM_ACTION_SYSROOT pointing at a rustc sysroot"]
async fn rustc_is_a_cached_hermetic_action() {
    let sysroot = PathBuf::from(std::env::var("LOOM_ACTION_SYSROOT").expect("LOOM_ACTION_SYSROOT"));
    let store = Store::memory().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let runner = Runner::new(store.clone(), scratch.path()).unwrap();
    let source = "pub fn add(a: u64, b: u64) -> u64 { a + b }\n";
    let first = runner.run(&action(&sysroot, &store, source)).await.unwrap();
    assert_eq!(
        first.result.exit_code,
        0,
        "{}",
        String::from_utf8_lossy(&runner.read(&first.result.stderr).unwrap())
    );
    let rlib = runner
        .read(&first.result.outputs["libdemo.rlib"].hash)
        .unwrap();
    assert!(rlib.starts_with(b"!<arch>"), "not an rlib archive");

    let again = runner.run(&action(&sysroot, &store, source)).await.unwrap();
    assert!(again.cached);
    println!(
        "rustc miss {:?}, hit {:?}, rlib {} bytes",
        first.elapsed,
        again.elapsed,
        rlib.len()
    );

    // An edit is a new key; a rebuild of the original after forgetting the record must give the same bytes.
    let edited = runner
        .run(&action(
            &sysroot,
            &store,
            "pub fn add(a: u64, b: u64) -> u64 { a + b + 1 }\n",
        ))
        .await
        .unwrap();
    assert!(!edited.cached);
    assert_ne!(
        edited.result.outputs["libdemo.rlib"].hash,
        first.result.outputs["libdemo.rlib"].hash
    );
    store.clear_action_results().unwrap();
    let rebuilt = runner.run(&action(&sysroot, &store, source)).await.unwrap();
    assert!(!rebuilt.cached);
    assert_eq!(
        rebuilt.result.outputs["libdemo.rlib"].hash, first.result.outputs["libdemo.rlib"].hash,
        "rustc is not reproducible under the sandbox for this crate"
    );
}
