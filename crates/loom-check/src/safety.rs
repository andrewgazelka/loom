//! Conservative admission for untrusted source, before any build script or macro
//! can execute. Compiler enforcement and pinned trusted provenance are separate.
use crate::{SourceBundle, SourceFile, diagnostic, rust_effects};
use loom_proto::{Diagnostic, Lang};
use proc_macro2::{TokenStream, TokenTree};
use std::path::{Component, Path, PathBuf};
use syn::{ext::IdentExt, visit::Visit};

/// Top-level names in a package that the dependency scan skips (`inspect_untrusted_source` in
/// loom-build's `direct/admission.rs`, for the root package): the host places vendored sources,
/// pinned crates and cargo configuration there. Nothing a tenant compiles may live in or reach
/// one, or it would be compiled without being read.
const UNSCANNED_ROOTS: [&str; 3] = ["vendor", "loom-crates", ".cargo"];

/// Whether the first directory of `path` (a path relative to the package) is one the scan skips.
/// Compared without regard to ASCII case: on a case-insensitive file system `Vendor/` is `vendor/`.
pub(crate) fn in_unscanned_root(path: &Path) -> bool {
    path.components()
        .find_map(|part| match part {
            Component::Normal(first) => Some(first),
            _ => None,
        })
        .and_then(std::ffi::OsStr::to_str)
        .is_some_and(|first| {
            UNSCANNED_ROOTS
                .iter()
                .any(|root| first.eq_ignore_ascii_case(root))
        })
}

/// The path an `include*!` argument names, when the argument is one string literal (a trailing
/// comma is fine) that is relative and stays below the including file's directory: no root, no
/// `..`; a leading `./` is dropped. `None` for anything else, a computed path included.
pub(crate) fn relative_literal(tokens: &TokenStream) -> Option<PathBuf> {
    let mut tokens: Vec<TokenTree> = tokens.clone().into_iter().collect();
    if matches!(tokens.last(), Some(TokenTree::Punct(punct)) if punct.as_char() == ',') {
        tokens.pop();
    }
    let literal = syn::parse2::<syn::LitStr>(tokens.into_iter().collect()).ok()?;
    let value = literal.value();
    let mut path = PathBuf::new();
    for part in Path::new(&value).components() {
        match part {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!path.as_os_str().is_empty()).then_some(path)
}

/// Every `include!("..")` in `file`, in code and in macro tokens, as the path it names relative
/// to the including file.
fn included_sources(file: &syn::File) -> Vec<PathBuf> {
    struct Inclusions(Vec<PathBuf>);
    impl Inclusions {
        fn tokens(&mut self, stream: TokenStream) {
            let tokens: Vec<TokenTree> = stream.into_iter().collect();
            for (index, token) in tokens.iter().enumerate() {
                match token {
                    TokenTree::Group(group) => self.tokens(group.stream()),
                    TokenTree::Ident(ident)
                        if ident.unraw() == "include"
                            && matches!(tokens.get(index + 1), Some(TokenTree::Punct(punct)) if punct.as_char() == '!') =>
                    {
                        if let Some(TokenTree::Group(group)) = tokens.get(index + 2) {
                            self.0.extend(relative_literal(&group.stream()));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    impl<'ast> Visit<'ast> for Inclusions {
        fn visit_macro(&mut self, node: &'ast syn::Macro) {
            if node
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident.unraw() == "include")
            {
                self.0.extend(relative_literal(&node.tokens));
            }
            self.tokens(node.tokens.clone());
            syn::visit::visit_macro(self, node);
        }
    }
    let mut inclusions = Inclusions(Vec::new());
    inclusions.visit_file(file);
    inclusions.0
}

/// Admission policy compiled into this checker, used to invalidate artifacts
/// when policy changes without consulting mutable runtime source files.
pub fn safety_policy_bytes() -> &'static [u8] {
    concat!(
        include_str!("safety.rs"),
        include_str!("rust_effects.rs"),
        include_str!("rust_effects/admission.rs"),
        include_str!("rust_effects/macros.rs"),
        include_str!("handler_references.rs")
    )
    .as_bytes()
}

pub fn untrusted_source_diagnostics(source: &str) -> Vec<Diagnostic> {
    match syn::parse_file(source) {
        Ok(file) => rust_effects::unsafe_source_diagnostics(&file),
        Err(error) => vec![diagnostic(
            Lang::Rust,
            "LOOM_UNTRUSTED_SOURCE",
            &format!("Untrusted Rust source cannot be parsed: {error}"),
        )],
    }
}

pub fn untrusted_package_diagnostics(bundle: &SourceBundle) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    for (name, file) in &bundle.files {
        let parts: Vec<_> = name.split('/').collect();
        let leaf = parts.last().copied().unwrap_or_default();
        let configuration = parts.contains(&".cargo")
            || matches!(leaf, "rust-toolchain" | "rust-toolchain.toml" | "build.rs");
        if configuration {
            let mut error = diagnostic(
                Lang::Rust,
                "LOOM_BUILD_POLICY",
                "Untrusted packages cannot supply Cargo configuration, toolchain overrides, or build scripts",
            );
            error.file = name.clone();
            diagnostics.push(error);
        }
        if leaf == "Cargo.toml" {
            let manifest = file
                .as_text()
                .and_then(|text| text.parse::<toml::Value>().ok());
            if let Some(manifest) = manifest {
                for kind in ["lib", "bin", "example", "test", "bench"] {
                    if let Some(targets) = manifest.get(kind) {
                        let targets = match targets.as_array() {
                            Some(targets) => targets.iter().collect::<Vec<_>>(),
                            None => vec![targets],
                        };
                        for target in targets {
                            if let Some(path) = target.get("path") {
                                let valid = path.as_str().is_some_and(|value| {
                                    let path = std::path::Path::new(value);
                                    !value.contains('\\')
                                        && path
                                            .extension()
                                            .is_some_and(|extension| extension == "rs")
                                        && path.components().all(|part| {
                                            matches!(
                                                part,
                                                std::path::Component::Normal(_)
                                                    | std::path::Component::CurDir
                                            )
                                        })
                                        // Not compiled from a directory the scan skips.
                                        && !in_unscanned_root(path)
                                });
                                if !valid {
                                    let mut error = diagnostic(
                                        Lang::Rust,
                                        "LOOM_BUILD_POLICY",
                                        &format!(
                                            "Untrusted {kind} target paths must be package-relative Rust .rs files without parent traversal, outside vendor/, loom-crates/ and .cargo/"
                                        ),
                                    );
                                    error.file = name.clone();
                                    diagnostics.push(error);
                                }
                            }
                        }
                    }
                }
                let package = manifest.get("package");
                let build = package.and_then(|package| package.get("build"));
                let proc_macro = manifest.get("lib").and_then(|library| {
                    library
                        .get("proc-macro")
                        .or_else(|| library.get("proc_macro"))
                });
                let proc_macro_type = manifest
                    .get("lib")
                    .and_then(|library| library.get("crate-type"))
                    .and_then(toml::Value::as_array)
                    .is_some_and(|types| {
                        types.iter().any(|kind| kind.as_str() == Some("proc-macro"))
                    });
                if proc_macro_type
                    || build.is_some_and(|build| build.as_bool() != Some(false))
                    || proc_macro.is_some_and(|value| value.as_bool() != Some(false))
                {
                    let mut error = diagnostic(
                        Lang::Rust,
                        "LOOM_BUILD_POLICY",
                        "Untrusted packages cannot execute build scripts or procedural macros",
                    );
                    error.file = name.clone();
                    diagnostics.push(error);
                }
                if manifest.get("cargo-features").is_some() {
                    let mut error = diagnostic(
                        Lang::Rust,
                        "LOOM_BUILD_POLICY",
                        "Untrusted packages cannot enable unstable Cargo features",
                    );
                    error.file = name.clone();
                    diagnostics.push(error);
                }
            } else {
                let mut error = diagnostic(
                    Lang::Rust,
                    "LOOM_BUILD_POLICY",
                    "Untrusted Cargo.toml must be valid UTF-8 TOML",
                );
                error.file = name.clone();
                diagnostics.push(error);
            }
        }
        if name.ends_with(".rs") {
            let mut errors = match file {
                SourceFile::Text(source) => untrusted_source_diagnostics(source),
                SourceFile::Binary { .. } => vec![diagnostic(
                    Lang::Rust,
                    "LOOM_UNTRUSTED_SOURCE",
                    "Rust sources must be UTF-8 text",
                )],
            };
            // An included file lies where its including file's directory puts it, and the scan
            // reads none of the root package's host-owned top-level directories.
            if let SourceFile::Text(source) = file
                && let Ok(parsed) = syn::parse_file(source)
            {
                let directory = Path::new(name).parent().unwrap_or(Path::new(""));
                for included in included_sources(&parsed) {
                    let resolved = directory.join(&included);
                    if in_unscanned_root(&resolved) {
                        errors.push(diagnostic(
                            Lang::Rust,
                            "LOOM_UNTRUSTED_SOURCE",
                            &format!(
                                "include! of {} lies in {}, a directory the package scan does not read",
                                included.display(),
                                resolved.display()
                            ),
                        ));
                    }
                }
            }
            for error in &mut errors {
                error.file = name.clone();
            }
            diagnostics.extend(errors);
        }
    }
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    fn package(manifest: &str, extra: Option<&str>) -> SourceBundle {
        let mut files = std::collections::BTreeMap::new();
        files.insert("Cargo.toml".into(), SourceFile::Text(manifest.into()));
        files.insert(
            "src/lib.rs".into(),
            SourceFile::Text("pub fn answer()->u32 {42}".into()),
        );
        if let Some(extra) = extra {
            files.insert(extra.into(), SourceFile::Text(String::new()));
        }
        SourceBundle { files }
    }
    #[test]
    fn host_code_and_configuration_are_rejected_before_building() {
        for manifest in [
            "[package]\nbuild='custom.rs'",
            "[lib]\nproc-macro=true",
            "[lib]\nproc_macro=true",
            "[lib]\ncrate-type=['proc-macro']",
            "cargo-features=['edition2027']",
        ] {
            assert!(!untrusted_package_diagnostics(&package(manifest, None)).is_empty());
        }
        for path in [
            "build.rs",
            ".cargo/config",
            "nested/.cargo/config.toml",
            "rust-toolchain.toml",
        ] {
            assert!(
                !untrusted_package_diagnostics(&package("[package]\nbuild=false", Some(path)))
                    .is_empty()
            );
        }
        assert!(untrusted_package_diagnostics(&package("[package]\nbuild=false", None)).is_empty());
    }
    #[test]
    fn declared_targets_cannot_hide_rust_source_under_another_extension() {
        for kind in ["lib", "bin", "example", "test", "bench"] {
            let section = if kind == "lib" {
                "[lib]".to_owned()
            } else {
                format!("[[{kind}]]")
            };
            for path in [
                "payload.txt",
                "payload",
                "../outside.rs",
                "/tmp/outside.rs",
                "src\\outside.rs",
            ] {
                let manifest = format!("{section}\npath={path:?}");
                let mut bundle = package(&manifest, None);
                bundle.files.insert(
                    "payload.txt".into(),
                    SourceFile::Text(include_str!("../tests/fixtures/unsafe-macro.rs").into()),
                );
                assert!(
                    untrusted_package_diagnostics(&bundle)
                        .iter()
                        .any(|error| error.code == "LOOM_BUILD_POLICY"),
                    "accepted {manifest}"
                );
            }
            let manifest = format!("{section}\npath='src/lib.rs'");
            assert!(untrusted_package_diagnostics(&package(&manifest, None)).is_empty());
        }
        assert!(!untrusted_package_diagnostics(&package("[lib]\npath=42", None)).is_empty());
    }
    #[test]
    fn macro_consumer_requires_dependency_admission_and_proc_macros_are_denied() {
        assert!(
            untrusted_source_diagnostics(include_str!(
                "../tests/fixtures/unsafe-macro-consumer.rs"
            ))
            .is_empty()
        );
        assert!(
            !untrusted_source_diagnostics(include_str!("../tests/fixtures/unsafe-proc-macro.rs"))
                .is_empty()
        );
    }
    #[test]
    fn a_macro_that_expands_to_unsafe_is_allowed_but_one_that_includes_files_is_not() {
        assert!(
            untrusted_source_diagnostics(include_str!("../tests/fixtures/unsafe-macro.rs"))
                .is_empty()
        );
        assert!(
            !untrusted_source_diagnostics(
                "macro_rules! leak {()=>{include_str!(\"/etc/passwd\")}}"
            )
            .is_empty()
        );
        assert!(
            untrusted_source_diagnostics(
                "macro_rules! safe {()=>{1+2}} pub fn main()->i32 {safe!()}"
            )
            .is_empty()
        );
    }
    #[test]
    fn include_compiles_only_rust_files_the_scan_reads_and_data_macros_take_any_extension() {
        for (source, admitted) in [
            ("include!(\"x.rs\");", true),
            ("include!(\"sub/x.rs\");", true),
            ("include!(\"x.txt\");", false),
            ("include!(\"/abs/x.rs\");", false),
            ("include!(\"../x.rs\");", false),
            ("include!(\"./x.rs\");", true),
            ("const S: &str = include_str!(\"x.txt\",);", true),
            ("const S: &str = include_str!(\"../README.md\");", false),
            ("const S: &str = include_str!(\"x.txt\");", true),
            ("const B: &[u8] = include_bytes!(\"x.bin\");", true),
            ("const S: &str = include_str!(\"/etc/passwd\");", false),
            ("const S: &str = include_str!(\"../x.txt\");", false),
            ("const S: &str = r#include_str!(\"/etc/passwd\");", false),
            ("const S: &str = concat!(\"a\", \"b\");", true),
            ("const S: &str = concat!(env!(\"HOME\"), \"\");", false),
        ] {
            assert_eq!(
                untrusted_source_diagnostics(source).is_empty(),
                admitted,
                "{source}"
            );
        }
        // The same verdict through a package: a `.txt` payload is never read by the scan, so
        // `include!` of it is refused and the payload cannot hide `#[no_mangle]` or `env!`.
        let mut bundle = package("[package]\nbuild=false", None);
        bundle.files.insert(
            "src/lib.rs".into(),
            SourceFile::Text("include!(\"payload.txt\");".into()),
        );
        bundle.files.insert(
            "src/payload.txt".into(),
            SourceFile::Text("#[no_mangle] fn exported() {}".into()),
        );
        assert!(!untrusted_package_diagnostics(&bundle).is_empty());
        bundle.files.insert(
            "src/lib.rs".into(),
            SourceFile::Text("include!(\"payload.rs\");".into()),
        );
        bundle.files.insert(
            "src/payload.rs".into(),
            SourceFile::Text("#[no_mangle] fn exported() {}".into()),
        );
        assert!(
            !untrusted_package_diagnostics(&bundle).is_empty(),
            "an included .rs file is scanned like any other"
        );
    }
    #[test]
    fn an_included_file_or_a_target_may_not_lie_where_the_package_scan_does_not_read() {
        let bundle = |lib: &str, file: &str, text: &str| {
            let mut bundle = package("[package]\nbuild=false", None);
            bundle
                .files
                .insert("src/lib.rs".into(), SourceFile::Text(lib.into()));
            bundle
                .files
                .insert(file.into(), SourceFile::Text(text.into()));
            bundle
        };
        let refused = |bundle: &SourceBundle| !untrusted_package_diagnostics(bundle).is_empty();
        // The path is resolved against the including file: from the package root it names
        // `vendor/`, from `src/` it names an ordinary directory the scan reads.
        for include in [
            "vendor/x.rs",
            "Vendor/x.rs",
            "./vendor/x.rs",
            "loom-crates/x.rs",
            ".cargo/x.rs",
        ] {
            let text = format!("include!(\"{include}\");");
            assert!(refused(&bundle("", "main.rs", &text)), "{include}");
            assert!(
                refused(&bundle(
                    "",
                    "main.rs",
                    &format!("fn m() {{ generate!({{ {text} }}); }}")
                )),
                "{include} in macro tokens"
            );
            if !include.starts_with(".cargo") {
                // (A `.cargo` path anywhere is cargo configuration, refused for that reason.)
                assert!(
                    !refused(&bundle(&text, "src/vendor/x.rs", "pub fn x() {}")),
                    "{include} from src/"
                );
            }
        }
        assert!(!refused(&bundle("", "main.rs", "include!(\"src/x.rs\");")));
        // A declared target path in a directory the scan skips, in any case.
        for path in [
            "vendor/lib.rs",
            "Vendor/lib.rs",
            "loom-crates/lib.rs",
            "./vendor/lib.rs",
        ] {
            for section in ["[lib]", "[[bin]]"] {
                let manifest = format!("{section}\npath={path:?}");
                assert!(refused(&package(&manifest, None)), "{section} {path}");
            }
        }
        assert!(!refused(&package("[lib]\npath='src/vendor/lib.rs'", None)));
        assert!(
            in_unscanned_root(Path::new("VENDOR/x.rs"))
                && !in_unscanned_root(Path::new("src/vendor/x.rs"))
        );
    }
}
