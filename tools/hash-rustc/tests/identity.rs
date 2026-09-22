use std::collections::BTreeMap;
use std::process::Command;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Document {
    toolchain: String,
    items: BTreeMap<String, Item>,
    entry: BTreeMap<String, String>,
    exports: BTreeMap<String, String>,
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

#[test]
fn cycle_function_renaming_preserves_content_identity() {
    let source = "fn first(n: u32) -> u32 { if n == 0 { 7 } else { second(n - 1) } } fn second(n: u32) -> u32 { if n == 0 { 9 } else { first(n - 1) } } pub fn entry(n: u32) -> u32 { first(n) }";
    let before = compile(source);
    let renamed = compile(&source.replace("first", "zeta").replace("second", "alpha"));
    assert_eq!(before.entry(), renamed.entry());
    assert_eq!(before.item("first").hash, renamed.item("zeta").hash);
    assert_eq!(before.item("second").hash, renamed.item("alpha").hash);
}

#[test]
fn identical_functions_share_hashes_inside_nominal_cycles() {
    let source = "struct Alpha; impl Alpha { fn one(&self) -> u32 { 7 } fn two(&self) -> u32 { 7 } } pub fn entry() -> u32 { Alpha.one() + Alpha.two() }";
    let before = compile(source);
    assert_eq!(before.item("one").hash, before.item("two").hash);
    let renamed = compile(&source.replace("one", "zeta").replace("two", "alpha"));
    assert_eq!(before.entry(), renamed.entry());
    assert_eq!(before.item("Alpha").hash, renamed.item("Alpha").hash);
}

#[test]
fn trait_impl_member_bindings_are_not_a_bag_of_bodies() {
    let source = "struct Alpha; trait Read { fn first(&self) -> u32; fn second(&self) -> u32; } impl Read for Alpha { fn first(&self) -> u32 { 7 } fn second(&self) -> u32 { 9 } } pub fn entry() -> u32 { Alpha.first() }";
    let before = compile(source);
    let swapped = compile(
        &source
            .replace("{ 7 }", "{ TEMP }")
            .replace("{ 9 }", "{ 7 }")
            .replace("{ TEMP }", "{ 9 }"),
    );
    assert_ne!(before.item("Alpha").hash, swapped.item("Alpha").hash);
    assert_ne!(before.entry(), swapped.entry());
}

#[test]
fn aliased_self_types_and_empty_impls_are_dependencies() {
    let source = "struct Alpha; type Alias = Alpha; impl Alias { fn read(&self) -> u32 { 7 } } pub fn entry() -> u32 { Alpha.read() }";
    let before = compile(source);
    let changed = compile(&source.replace("{ 7 }", "{ 9 }"));
    assert_ne!(before.item("Alpha").hash, changed.item("Alpha").hash);
    let empty = compile(&format!("{source} impl Alpha {{}}"));
    assert_ne!(before.item("Alpha").hash, empty.item("Alpha").hash);
}

/// The defect this guards: `largest` lives in a public module, is not an
/// entry, and is unreachable from `ping`. Its body change must still move the
/// export set, which is what the definition identity is rooted in.
#[test]
fn nested_public_function_body_change_moves_exports_not_entry() {
    let source = "pub mod shapes { pub trait Area { fn area(&self) -> f64; } pub fn largest<T: Area + Copy>(items: Vec<T>) -> T { let mut best = items[0]; for item in items.into_iter().skip(1) { if item.area() > best.area() { best = item; } } best } } pub fn ping() -> u32 { 1 }";
    let before = compile(source);
    let after = compile(&source.replace("skip(1)", "skip(0)"));
    assert_eq!(before.entry(), after.entry());
    assert_eq!(before.entry, before.exports_named(&["ping"]));
    assert_ne!(
        before.exports["shapes::largest"],
        after.exports["shapes::largest"]
    );
    assert_ne!(before.exports, after.exports);
    assert_eq!(
        before.exports.keys().collect::<Vec<_>>(),
        ["ping", "shapes::Area", "shapes::Area::area", "shapes::largest"]
    );
}

/// Alpha-renaming a local inside the nested function leaves every export
/// unchanged; renaming the function itself only renames its key.
#[test]
fn nested_public_function_alpha_renaming_keeps_exports() {
    let source = "pub mod shapes { pub fn largest(items: Vec<u32>) -> u32 { let mut best = items[0]; for elem in items { if elem > best { best = elem; } } best } } pub fn ping() -> u32 { 1 }";
    let before = compile(source);
    let renamed_locals = compile(&source.replace("best", "winner").replace("elem", "candidate"));
    assert_eq!(before.exports, renamed_locals.exports);
    let renamed_function = compile(&source.replace("largest", "biggest"));
    assert_eq!(
        before.exports["shapes::largest"],
        renamed_function.exports["shapes::biggest"]
    );
}

/// Exports follow rustc's effective visibility: a `pub fn` inside a private
/// module is not exported, a `pub use` of it is, impl members of a public type
/// are exported by their own visibility, and private items never are.
#[test]
fn exports_are_the_publicly_reachable_definitions() {
    let hidden = compile(
        "mod inner { pub fn hidden() -> u32 { 1 } fn private() -> u32 { 2 } } pub fn entry() -> u32 { 3 }",
    );
    assert_eq!(hidden.exports.keys().collect::<Vec<_>>(), ["entry"]);
    assert!(hidden.items.contains_key("inner::hidden"));
    let reexported = compile(
        "mod inner { pub fn hidden() -> u32 { 1 } } pub use inner::hidden; pub fn entry() -> u32 { 3 }",
    );
    assert_eq!(
        reexported.exports.keys().collect::<Vec<_>>(),
        ["entry", "inner::hidden"]
    );
    let with_type = compile(
        "pub struct Alpha(pub u32); impl Alpha { pub fn read(&self) -> u32 { self.0 } fn secret(&self) -> u32 { 0 } } pub fn entry() -> u32 { 3 }",
    );
    assert_eq!(
        with_type.exports.keys().collect::<Vec<_>>(),
        ["Alpha", "entry", "{impl#0}", "{impl#0}::read"]
    );
}

#[test]
fn entries_are_a_subset_of_exports() {
    let document = compile(
        "pub fn entry() -> u32 { 1 } pub fn other() -> u32 { 2 } fn private() -> u32 { 3 }",
    );
    for (name, hash) in &document.entry {
        assert_eq!(document.exports.get(name), Some(hash), "{document:#?}");
    }
    assert_eq!(document.entry.len(), 2);
    assert!(!document.exports.contains_key("private"));
}

impl Document {
    fn exports_named(&self, names: &[&str]) -> BTreeMap<String, String> {
        names
            .iter()
            .map(|name| ((*name).to_owned(), self.exports[*name].clone()))
            .collect()
    }
}

#[test]
fn builtin_derives_are_expanded_impls_inside_adt_identity() {
    let plain = "pub struct Square(pub f64); pub fn entry(value: Square) -> f64 { value.0 }";
    let derived = "#[derive(Clone, Copy, PartialEq)] pub struct Square(pub f64); pub fn entry(value: Square) -> f64 { value.0 }";
    let first = compile(derived);
    let second = compile(derived);
    assert_eq!(first.item("Square").hash, second.item("Square").hash);
    assert_eq!(first.entry(), second.entry());
    // The generated impl methods are ordinary encoded items.
    for method in ["clone", "eq"] {
        assert!(
            first
                .items
                .keys()
                .any(|path| path.ends_with(&format!("::{method}"))),
            "missing derived {method}: {:#?}",
            first.items.keys().collect::<Vec<_>>()
        );
    }
    // Impls belong to the ADT's identity, so the derive moves the struct and
    // its users; removing one derive moves it again.
    let undecorated = compile(plain);
    assert_ne!(undecorated.item("Square").hash, first.item("Square").hash);
    assert_ne!(undecorated.entry(), first.entry());
    let narrowed = compile(&derived.replace("Clone, Copy, PartialEq", "Clone, Copy"));
    assert_ne!(narrowed.item("Square").hash, first.item("Square").hash);
}

#[test]
fn builtin_derives_on_enums_expand_to_supported_hir() {
    let source = "#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)] pub enum Kind { #[default] Alpha, Beta(u8) } pub fn entry(kind: Kind) -> bool { kind == Kind::default() && kind <= Kind::Beta(1) }";
    let before = compile(source);
    assert_eq!(before.entry(), compile(source).entry());
    let widened = compile(&source.replace("Beta(u8)", "Beta(u16)"));
    assert_ne!(before.item("Kind").hash, widened.item("Kind").hash);
    assert_ne!(before.entry(), widened.entry());
}

#[test]
fn format_and_vec_expansions_hash_their_literals_not_their_spellings() {
    let source = r#"pub fn entry(count: usize) -> String { let items: Vec<usize> = vec![1, 2, count]; format!("{} out of {count}", items.len()) }"#;
    let before = compile(source);
    assert_eq!(before.entry(), compile(source).entry());
    // Renaming locals, including one captured by the format string, is free.
    let renamed = compile(&source.replace("items", "values").replace("count", "total"));
    assert_eq!(before.entry(), renamed.entry());
    // A change inside the format string is a literal change in HIR.
    let reworded = compile(&source.replace(" out of ", " within "));
    assert_ne!(before.entry(), reworded.entry());
    // So is a change inside the vector literal.
    let regrown = compile(&source.replace("vec![1, 2, count]", "vec![1, 3, count]"));
    assert_ne!(before.entry(), regrown.entry());
}

#[test]
fn matches_assert_and_write_expansions_hash() {
    let source = r#"use std::fmt::Write; pub fn entry(value: Option<u8>) -> String { assert!(value.is_none() || value.is_some(), "{value:?}"); let mut out = String::new(); write!(out, "{}", matches!(value, Some(7))).unwrap(); out }"#;
    let before = compile(source);
    assert_eq!(before.entry(), compile(source).entry());
    let changed = compile(&source.replace("Some(7)", "Some(8)"));
    assert_ne!(before.entry(), changed.entry());
}
