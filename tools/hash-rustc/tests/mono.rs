use std::collections::BTreeMap;
use std::process::Command;

struct Audit {
    hashes: BTreeMap<String, String>,
    hir: serde_json::Value,
}

fn compile(source: &str) -> Audit {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("input.rs"), source).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
        .current_dir(directory.path())
        .args([
            "input.rs",
            "--crate-name=fixture",
            "--crate-type=rlib",
            "--edition=2024",
            "-Copt-level=0",
            "-Awarnings",
        ])
        .env_remove("LOOM_OBJECT_CACHE")
        .env("LOOM_ITEM_COVERAGE", directory.path().join("coverage.json"))
        .env("LOOM_ITEM_HASHES", directory.path().join("hir.json"))
        .env("LOOM_ITEM_PREIMAGES", directory.path().join("preimages"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{source}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.path().join("coverage.json")).unwrap())
            .unwrap();
    assert_eq!(report["refused"], 0, "{report}");
    assert_eq!(report["mono"]["refused_unique_items"], 0, "{report}");
    Audit {
        hashes: serde_json::from_value(report["mono"]["hashes"].clone()).unwrap(),
        hir: serde_json::from_slice(&std::fs::read(directory.path().join("hir.json")).unwrap())
            .unwrap(),
    }
}

impl Audit {
    fn generic(&self) -> &str {
        let matches: Vec<_> = self
            .hashes
            .iter()
            .filter(|entry| entry.0.contains("::generic)"))
            .collect();
        assert_eq!(matches.len(), 1, "{:?}", self.hashes);
        matches[0].1
    }
}

// The generic HIR stays fixed; only the substitution changes between controls.
fn structural(declarations: &str, first: &str, second: &str) {
    let source = format!(
        "{declarations} #[inline(never)] pub fn generic<T>(value: T) -> T {{ value }} pub fn entry(value: {first}) -> {first} {{ generic(value) }}"
    );
    let before = compile(&source);
    let renamed = compile(&source.replace("value", "renamed").replace(
        "generic<T>(renamed: T) -> T",
        "generic<Element>(renamed: Element) -> Element",
    ));
    assert_eq!(before.generic(), renamed.generic());
    let changed = compile(&source.replace(first, second));
    assert_eq!(
        before.hir["items"]["generic"]["hash"],
        changed.hir["items"]["generic"]["hash"]
    );
    assert_ne!(before.generic(), changed.generic());
}

#[test]
fn references() {
    structural("", "&u32", "&u64");
}
#[test]
fn raw_pointers() {
    structural("", "*const u32", "*mut u32");
}
#[test]
fn arrays() {
    structural("", "[u8; 3]", "[u8; 4]");
}
#[test]
fn slices() {
    structural("", "&[u8]", "&[u16]");
}
#[test]
fn tuples() {
    structural("", "(u8, bool)", "(u16, bool)");
}
#[test]
fn function_pointers() {
    structural("", "fn(u32) -> u32", "fn(u32) -> u64");
}
#[test]
fn higher_ranked_function_pointers() {
    structural(
        "",
        "for<'a> fn(&'a u32) -> &'a u32",
        "for<'a> fn(&'a u64) -> &'a u64",
    );
}
#[test]
fn external_adts() {
    structural("", "Vec<u32>", "Vec<u64>");
}
#[test]
fn local_structs() {
    structural(
        "pub struct Record<T> { pub field: T }",
        "Record<u32>",
        "Record<u64>",
    );
}
#[test]
fn local_enums() {
    structural(
        "pub enum Choice<T> { One(T), Empty }",
        "Choice<u32>",
        "Choice<u64>",
    );
}
#[test]
fn local_unions() {
    structural(
        "pub union Bits<T: Copy> { pub field: T }",
        "Bits<u32>",
        "Bits<u64>",
    );
}
#[test]
fn trait_objects() {
    structural("", "&dyn Send", "&dyn Sync");
}
#[test]
fn strings() {
    structural("", "&str", "&[u8]");
}

#[test]
fn local_adt_referent_changes_and_renames() {
    let source = "pub struct Record { pub field: u32 } #[inline(never)] pub fn generic<T>(value: T) -> T { value } pub fn entry(value: Record) -> Record { generic(value) }";
    let first = compile(source);
    assert_eq!(
        first.generic(),
        compile(&source.replace("Record", "Renamed")).generic()
    );
    assert_ne!(
        first.generic(),
        compile(&source.replace("u32", "u64")).generic()
    );
}

#[test]
fn evaluated_const_arguments() {
    let source = "#[inline(never)] pub fn generic<const N: usize>(value: u32) -> u32 { value + N as u32 } pub fn entry(value: u32) -> u32 { generic::<3>(value) }";
    let first = compile(source);
    assert_eq!(
        first.generic(),
        compile(&source.replace("N", "COUNT").replace("value", "renamed")).generic()
    );
    assert_ne!(
        first.generic(),
        compile(&source.replace("::<3>", "::<4>")).generic()
    );
}

#[test]
fn function_item_arguments() {
    let source = "fn helper(value: u32) -> u32 { value + 1 } #[inline(never)] pub fn generic<T: Fn(u32) -> u32>(value: T) -> u32 { value(7) } pub fn entry() -> u32 { generic(helper) }";
    let first = compile(source);
    assert_eq!(
        first.generic(),
        compile(&source.replace("helper", "renamed")).generic()
    );
    assert_ne!(
        first.generic(),
        compile(&source.replace("+ 1", "+ 2")).generic()
    );
}

#[test]
fn closure_arguments() {
    let source = "#[inline(never)] pub fn generic<T: Fn(u32) -> u32>(value: T) -> u32 { value(7) } pub fn entry() -> u32 { generic(|input| input + 1) }";
    let first = compile(source);
    assert_eq!(
        first.generic(),
        compile(&source.replace("input", "renamed")).generic()
    );
    assert_ne!(
        first.generic(),
        compile(&source.replace("+ 1", "+ 2")).generic()
    );
}

#[test]
fn coroutine_arguments() {
    let source = "#[inline(never)] pub fn generic<T>(value: T) -> T { value } pub fn entry() -> impl std::future::Future<Output = u32> { generic(async { 7 }) }";
    let first = compile(source);
    assert_eq!(
        first.generic(),
        compile(&source.replace("value", "renamed")).generic()
    );
    assert_ne!(
        first.generic(),
        compile(&source.replace("{ 7 }", "{ 8 }")).generic()
    );
}

#[test]
fn compiler_drop_shims() {
    let source = "pub struct Record(pub String); pub fn entry(value: Record) { drop(value); }";
    let first = compile(source);
    let renamed = compile(&source.replace("Record", "Renamed"));
    let changed = compile(&source.replace("pub String", "pub Vec<String>"));
    let shims = |audit: &Audit| {
        audit
            .hashes
            .iter()
            .filter(|entry| entry.0.contains("DropGlue") && entry.0.contains("args: [Record]"))
            .map(|entry| entry.1.clone())
            .collect::<Vec<_>>()
    };
    let before = shims(&first);
    assert!(!before.is_empty(), "{:?}", first.hashes);
    let renamed_shims: Vec<_> = renamed
        .hashes
        .iter()
        .filter(|entry| entry.0.contains("DropGlue") && entry.0.contains("args: [Renamed]"))
        .map(|entry| entry.1.clone())
        .collect();
    assert_eq!(before, renamed_shims);
    assert_ne!(before, shims(&changed));
}

#[test]
fn static_identity() {
    let source = "pub static VALUE: u32 = 7; pub fn entry() -> &'static u32 { &VALUE }";
    let before = compile(source);
    let renamed = compile(&source.replace("VALUE", "RENAMED"));
    let changed = compile(&source.replace("= 7", "= 9"));
    let static_hash = |audit: &Audit| {
        let matches: Vec<_> = audit
            .hashes
            .iter()
            .filter(|entry| entry.0.starts_with("Static("))
            .collect();
        assert_eq!(matches.len(), 1, "{:?}", audit.hashes);
        matches[0].1.clone()
    };
    assert_eq!(static_hash(&before), static_hash(&renamed));
    assert_ne!(static_hash(&before), static_hash(&changed));
}
