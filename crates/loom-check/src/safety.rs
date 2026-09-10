//! Conservative admission for untrusted source, before any build script or macro
//! can execute. Compiler enforcement and pinned trusted provenance are separate.
use crate::{SourceBundle, SourceFile, diagnostic, rust_effects};
use loom_proto::{Diagnostic, Lang};

/// Admission policy compiled into this checker, used to invalidate artifacts
/// when policy changes without consulting mutable runtime source files.
pub fn safety_policy_bytes() -> &'static [u8] {
    concat!(include_str!("safety.rs"), include_str!("rust_effects.rs")).as_bytes()
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
                                });
                                if !valid {
                                    let mut error = diagnostic(
                                        Lang::Rust,
                                        "LOOM_BUILD_POLICY",
                                        &format!(
                                            "Untrusted {kind} target paths must be package-relative Rust .rs files without parent traversal"
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
    fn dependency_macro_unsafe_is_rejected_even_when_rustc_lint_exempts_expansion() {
        assert!(
            !untrusted_source_diagnostics(include_str!("../tests/fixtures/unsafe-macro.rs"))
                .is_empty()
        );
        assert!(
            untrusted_source_diagnostics(
                "macro_rules! safe {()=>{1+2}} pub fn main()->i32 {safe!()}"
            )
            .is_empty()
        );
    }
}
