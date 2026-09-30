//! Host-owned ABI generation. The wrappers are `loom-guest-rs` macro calls
//! appended to the checked guest source before the one root compile, so the
//! guest is compiled once. The driver's item document is the contract those
//! calls must agree with.
use super::*;

#[derive(serde::Deserialize)]
struct Contract {
    entry: BTreeMap<String, String>,
}

/// The comment line `generate` writes between the guest source and the
/// wrappers it appends. Its 1-based line in the compiled text is where the
/// definition's own lines end and generated code begins.
pub const WRAPPER_MARKER: &str = "// loom: generated entry wrappers follow";

/// The guest source with the entry wrappers appended: the source, the marker
/// line, then one macro call per root `pub fn` (and one for a root `pub const
/// LOOM_SCHEMA`). The compiler sees exactly this text.
///
/// The entry rule is the driver's (`hash-rustc` `entries::is_entry`): a public
/// free function at the crate root. [`check_contract`] compares the two after
/// the compile.
pub(super) fn generate(source: &str) -> Result<String, BuildError> {
    let file = syn::parse_file(source).map_err(rejected)?;
    let mut generated = source.to_owned();
    if !generated.ends_with('\n') {
        generated.push('\n');
    }
    generated.push_str(WRAPPER_MARKER);
    generated.push('\n');
    for name in entries(&file) {
        let function = file
            .items
            .iter()
            .find_map(|item| match item {
                syn::Item::Fn(function) if function.sig.ident == name.as_str() => Some(function),
                _ => None,
            })
            .expect("entries() names root functions");
        if function.sig.asyncness.is_some() || !function.sig.generics.params.is_empty() {
            return Err(rejected(format!(
                "entry {name} must be synchronous and non-generic"
            )));
        }
        let count = function.sig.inputs.len();
        let arguments = (0..count)
            .map(|index| format!("argument_{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        generated.push_str(&format!("::loom::__loom_export_entry!({name}; {arguments});\n"));
    }
    if file.items.iter().any(|item| {
        matches!(item, syn::Item::Const(constant)
            if constant.ident == "LOOM_SCHEMA"
                && matches!(constant.vis, syn::Visibility::Public(_)))
    }) {
        generated.push_str("::loom::__loom_export_schema!();\n");
    }
    Ok(generated)
}

/// The workspace's `src/lib.rs` holding the generated text for the duration of
/// one compile. The original is written back by [`restore`](Self::restore), or
/// by `Drop` on any early return, so wrappers never enter a later build's
/// source identity.
pub(super) struct WrappedSource {
    path: PathBuf,
    original: String,
    generated: String,
    restored: bool,
}

impl WrappedSource {
    /// Generate the wrappers for `directory/src/lib.rs` and write them in.
    /// Rejects an unsupported entry before any compiler starts.
    pub(super) fn write(directory: &Path) -> Result<Self, BuildError> {
        let path = directory.join("src/lib.rs");
        let original = std::fs::read_to_string(&path)?;
        let generated = generate(&original)?;
        std::fs::write(&path, &generated)?;
        Ok(Self {
            path,
            original,
            generated,
            restored: false,
        })
    }

    pub(super) fn restore(&mut self) -> Result<(), BuildError> {
        if !self.restored {
            std::fs::write(&self.path, &self.original)?;
            self.restored = true;
        }
        Ok(())
    }

    /// The exact text the compiler saw.
    pub(super) fn into_text(mut self) -> String {
        std::mem::take(&mut self.generated)
    }
}

impl Drop for WrappedSource {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

/// Root `pub fn` names in source order.
fn entries(file: &syn::File) -> Vec<String> {
    file.items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Fn(function) if matches!(function.vis, syn::Visibility::Public(_)) => {
                Some(function.sig.ident.to_string())
            }
            _ => None,
        })
        .collect()
}

/// The wrappers were generated from the source's syntax and compiled by the
/// driver in the same run; its item document must name the same entries. A
/// difference means the two entry rules drifted, and the artifact is refused
/// rather than served with a missing or extra export.
pub(super) async fn check_contract(identity: &Path, source: &str) -> Result<(), BuildError> {
    let contract: Contract =
        serde_json::from_slice(&fs::read(identity.join("items.json")).await?).map_err(rejected)?;
    let file = syn::parse_file(source).map_err(rejected)?;
    let mut expected = entries(&file);
    expected.sort();
    let actual: Vec<&String> = contract.entry.keys().collect();
    if expected.iter().ne(actual.iter().copied()) {
        return Err(rejected(format!(
            "driver entries {actual:?} differ from the generated wrappers {expected:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_one_wrapper_call_per_root_pub_fn_after_the_marker() {
        let source = "pub fn one(x: i32) -> i32 { x }\nfn hidden() {}\npub fn two() -> i32 { 2 }\npub mod nested { pub fn deep() {} }";
        let generated = generate(source).unwrap();
        syn::parse_file(&generated).unwrap();
        let (own, wrappers) = generated.split_once(WRAPPER_MARKER).unwrap();
        assert!(own.starts_with(source));
        assert!(wrappers.contains("::loom::__loom_export_entry!(one; argument_0);"));
        assert!(wrappers.contains("::loom::__loom_export_entry!(two; );"));
        assert!(!wrappers.contains("hidden") && !wrappers.contains("deep"));
        assert!(!wrappers.contains("schema"));
    }

    #[test]
    fn arity_becomes_one_binding_per_parameter() {
        let generated =
            generate("pub fn two(x: i32, y: String) -> i32 { x }").unwrap();
        assert!(generated.contains("__loom_export_entry!(two; argument_0, argument_1);"));
    }

    #[test]
    fn schema_wrapper_only_when_the_guest_defines_a_public_schema() {
        let with = generate("pub const LOOM_SCHEMA: &str = \"select 1\";\npub fn f() {}").unwrap();
        assert!(with.contains("::loom::__loom_export_schema!();"));
        let private = generate("const LOOM_SCHEMA: &str = \"x\";\npub fn f() {}").unwrap();
        assert!(!private.contains("__loom_export_schema"));
    }

    #[test]
    fn entries_must_be_synchronous_and_non_generic() {
        assert!(generate("pub async fn f() {}").is_err());
        assert!(generate("pub fn f<T>(x: T) {}").is_err());
    }
}
