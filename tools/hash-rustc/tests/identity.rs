use std::collections::BTreeMap;
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Document {
    toolchain: String,
    items: BTreeMap<String, Item>,
    entry: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct Item {
    hash: String,
    refs: Vec<String>,
    cycle: Option<Vec<String>>,
}

impl Document {
    fn item(&self, suffix: &str) -> &Item {
        self.items
            .iter()
            .find(|(path, _)| path.as_str() == suffix || path.ends_with(&format!("::{suffix}")))
            .unwrap_or_else(|| panic!("missing {suffix}: {self:#?}"))
            .1
    }

    fn entry(&self) -> &str {
        assert_eq!(self.entry.len(), 1, "{self:#?}");
        self.entry.values().next().unwrap()
    }
}

fn compile(source: &str) -> Document {
    let directory = tempfile::tempdir().unwrap();
    let input = directory.path().join("fixture.rs");
    let output = directory.path().join("hashes.json");
    std::fs::write(&input, source).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_hash-rustc"))
        .args([
            "--crate-name",
            "fixture",
            "--crate-type",
            "rlib",
            "--edition",
            "2024",
            "-A",
            "warnings",
        ])
        .arg(&input)
        .arg("--out-dir")
        .arg(directory.path())
        .env("LOOM_ITEM_HASHES", &output)
        .env("LOOM_ITEM_PREIMAGES", directory.path().join("preimages"))
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}\nsource:\n{source}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(
        directory.path().join("libfixture.rlib").exists(),
        "driver must still produce rustc output"
    );
    let document: Document = serde_json::from_slice(&std::fs::read(output).unwrap()).unwrap();
    assert!(document.toolchain.starts_with("rustc 1.100.0-nightly"));
    document
}

#[test]
fn alpha_equivalent_sources_hash_equal() {
    let before = compile(
        "fn helper(x: i32) -> i32 { x + 1 } pub fn entry(x: i32) -> i32 { let y = helper(x); y * x }",
    );
    let after = compile(
        "pub fn entry(input: i32) -> i32 {\n let renamed = helper(input);\n renamed * input\n}\n fn helper(value: i32) -> i32 { value + 1 }",
    );
    assert_eq!(before.entry(), after.entry());
    assert_eq!(before.item("helper").hash, after.item("helper").hash);
}

#[test]
fn constant_change_moves_hash() {
    let source = "const VALUE: i32 = 7; pub fn entry() -> i32 { VALUE }";
    let before = compile(source);
    let after = compile(&source.replace("= 7", "= 8"));
    assert_ne!(before.entry(), after.entry());
    assert_ne!(before.item("VALUE").hash, after.item("VALUE").hash);
}

#[test]
fn unreachable_helper_does_not_move_entry() {
    let source = "fn unused() -> i32 { 8 } pub fn entry() -> i32 { 7 }";
    let before = compile(source);
    let after = compile(&source.replace("{ 8 }", "{ 9 }"));
    assert_eq!(before.entry(), after.entry());
    assert_ne!(before.item("unused").hash, after.item("unused").hash);
    assert!(before.item("entry").refs.is_empty());
}

#[test]
fn method_call_resolves_to_one_impl() {
    let source = "struct A; struct B; impl A { fn foo(&self) -> i32 { 7 } } impl B { fn foo(&self) -> i32 { 8 } } pub fn entry() -> i32 { A.foo() }";
    let before = compile(source);
    let other_changed = compile(&source.replace("{ 8 }", "{ 9 }"));
    let called_changed = compile(&source.replace("{ 7 }", "{ 6 }"));
    assert_eq!(before.entry(), other_changed.entry());
    assert_ne!(before.entry(), called_changed.entry());
    assert_eq!(
        before
            .item("entry")
            .refs
            .iter()
            .filter(|path| path.ends_with("::foo"))
            .count(),
        1
    );
}

#[test]
fn cycle_hashes_together() {
    let source = "fn a(n: u32) -> u32 { if n == 0 { 1 } else { b(n-1) } } fn b(n: u32) -> u32 { if n == 0 { 2 } else { a(n-1) } } mod other { fn a(x: u32) -> u32 { if x == 0 { 1 } else { b(x-1) } } fn b(x: u32) -> u32 { if x == 0 { 2 } else { a(x-1) } } } pub fn entry(n: u32) -> u32 { a(n) }";
    let before = compile(source);
    for replacement in [
        source.replacen("{ 1 }", "{ 3 }", 1),
        source.replacen("{ 2 }", "{ 4 }", 1),
    ] {
        let after = compile(&replacement);
        assert_ne!(before.item("a").hash, after.item("a").hash);
        assert_ne!(before.item("b").hash, after.item("b").hash);
        assert_ne!(before.entry(), after.entry());
    }
    assert_eq!(before.item("a").hash, before.item("other::a").hash);
    assert_eq!(before.item("b").hash, before.item("other::b").hash);
    assert_eq!(before.item("a").cycle.as_ref().unwrap().len(), 2);
    assert_eq!(before.item("a").cycle, before.item("b").cycle);
}

#[test]
fn associated_type_shorthand_resolves_in_signatures_and_bodies() {
    let source = "pub trait Source { type Item; type Other: Copy; fn get(x: Self::Item); } pub fn take<T: Source>(x: T::Item) -> T::Item { let y: T::Item = x; y }";
    let first = compile(source);
    let renamed = compile(
        &source
            .replace("T", "Renamed")
            .replace("x:", "value:")
            .replace("= x;", "= value;"),
    );
    assert_eq!(first.item("take").hash, renamed.item("take").hash);
    let changed = compile(&source.replace("T::Item", "T::Other"));
    assert_ne!(first.item("take").hash, changed.item("take").hash);
}

#[test]
fn associated_type_shorthand_in_impl_signature() {
    let source = "trait Source { type Item; fn get(x: Self::Item) -> Self::Item; } struct Value; impl Source for Value { type Item = u32; fn get(x: Self::Item) -> Self::Item { x } } pub fn entry(x: u32) -> u32 { Value::get(x) }";
    let first = compile(source);
    let renamed = compile(&source.replace("x", "argument"));
    assert_eq!(first.entry(), renamed.entry());
    let changed = compile(&source.replace("u32", "u64"));
    assert_ne!(first.entry(), changed.entry());
}

#[test]
fn distinct_associated_slots_do_not_collapse() {
    let source = "pub trait Source { type First; type Second; } pub fn entry<T: Source>(value: T::First) -> T::First { value }";
    let first = compile(source);
    assert_eq!(
        first.entry(),
        compile(&source.replace("First", "Renamed")).entry()
    );
    assert_ne!(
        first.entry(),
        compile(&source.replace("T::First", "T::Second")).entry()
    );
}
