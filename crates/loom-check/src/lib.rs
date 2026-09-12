//! Language checking before definitions become executable identities.
mod rust_file;
use rust_file::check_rust_file;
mod crates;
pub use crates::{CrateDependency, crate_dependencies};
mod handler_references;
mod rust_effects;
mod safety;
use loom_proto::{DefineRequest, Diagnostic, ExportSig, Lang, ParamSig, TypeSig, ValueShape};
pub use safety::{
    safety_policy_bytes, untrusted_package_diagnostics, untrusted_source_diagnostics,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckedDef {
    pub hash: String,
    pub lang: Lang,
    pub name: String,
    pub source: String,
    pub deps: BTreeMap<String, String>,
    pub sig: TypeSig,
    pub diagnostics: Vec<Diagnostic>,
}
#[derive(Debug, thiserror::Error)]
pub enum CheckError {
    #[error("checker I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("checker protocol: {0}")]
    Json(#[from] serde_json::Error),
}
#[derive(Default)]
pub struct Checker;
impl Checker {
    pub fn new() -> Self {
        Self
    }
    pub async fn check(&self, request: &DefineRequest) -> Result<CheckedDef, CheckError> {
        self.check_with_signatures(request, &BTreeMap::new()).await
    }
    pub async fn check_with_signatures(
        &self,
        request: &DefineRequest,
        signatures: &BTreeMap<String, TypeSig>,
    ) -> Result<CheckedDef, CheckError> {
        let mut checked = check_rust(request, signatures);
        for hash in checked.deps.values() {
            if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
                checked.diagnostics.push(diagnostic(
                    checked.lang,
                    "LOOM_DEP",
                    "Dependencies must be resolved 64-character BLAKE3 hashes.",
                ));
            }
        }
        // Structured serialization frames the source and dependency edges unambiguously.
        let identity = loom_proto::definition_identity(
            checked.lang,
            &checked.source,
            &checked.deps,
            request.allowed_effects.as_deref(),
        )?;
        checked.hash = blake3::hash(&identity).to_hex().to_string();
        Ok(checked)
    }
}
fn diagnostic(lang: Lang, code: &str, message: &str) -> Diagnostic {
    Diagnostic {
        lang,
        file: "src/lib.rs".into(),
        line: 1,
        col: 1,
        code: code.into(),
        message: message.into(),
        snippet: None,
        hint: Some("Use loom effects for host I/O.".into()),
    }
}
/// Internal normalized form after an API source-bundle reference is resolved.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceBundle {
    pub files: BTreeMap<String, SourceFile>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SourceFile {
    Text(String),
    Binary { base64: String },
}
impl SourceFile {
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Binary { .. } => None,
        }
    }
    pub fn text_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Text(text) => Some(text),
            Self::Binary { .. } => None,
        }
    }
    pub fn bytes(&self) -> Result<Vec<u8>, String> {
        match self {
            Self::Text(text) => Ok(text.as_bytes().to_vec()),
            Self::Binary { base64 } => {
                base64::Engine::decode(&base64::engine::general_purpose::STANDARD, base64)
                    .map_err(|error| error.to_string())
            }
        }
    }
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        match String::from_utf8(bytes) {
            Ok(text) => Self::Text(text),
            Err(error) => Self::Binary {
                base64: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    error.into_bytes(),
                ),
            },
        }
    }
}
impl SourceBundle {
    pub fn validate(&self) -> Result<(), String> {
        if self.files.len() > 65536 {
            return Err("source bundle exceeds 65536 files".into());
        }
        let mut total = 0usize;
        for (name, source) in &self.files {
            let path = std::path::Path::new(name);
            if name.is_empty()
                || name.contains('\\')
                || !path
                    .components()
                    .all(|component| matches!(component, std::path::Component::Normal(_)))
            {
                return Err(format!("unsafe source path: {name}"));
            }
            total = total
                .checked_add(source.bytes()?.len())
                .ok_or("source size overflow")?;
        }
        if total > 256 * 1024 * 1024 {
            return Err("source bundle exceeds 256 MiB".into());
        }
        if !self.files.contains_key("Cargo.toml") || !self.files.contains_key("src/lib.rs") {
            return Err("crate bundle requires Cargo.toml and src/lib.rs".into());
        }
        Ok(())
    }
}
fn check_rust(request: &DefineRequest, signatures: &BTreeMap<String, TypeSig>) -> CheckedDef {
    if !request.source.trim_start().starts_with('{') {
        return check_rust_file(request, signatures);
    }
    let mut checked = CheckedDef {
        hash: String::new(),
        lang: Lang::Rust,
        name: request.name.clone(),
        source: request.source.clone(),
        deps: request.deps.clone(),
        sig: TypeSig::default(),
        diagnostics: Vec::new(),
    };
    let mut bundle: SourceBundle = match serde_json::from_str(&request.source) {
        Ok(bundle) => bundle,
        Err(error) => {
            checked
                .diagnostics
                .push(diagnostic(Lang::Rust, "LOOM_BUNDLE", &error.to_string()));
            return checked;
        }
    };
    if let Err(error) = bundle.validate() {
        checked
            .diagnostics
            .push(diagnostic(Lang::Rust, "LOOM_BUNDLE", &error));
        return checked;
    }
    let package_diagnostics = untrusted_package_diagnostics(&bundle);
    if !package_diagnostics.is_empty() {
        checked.diagnostics.extend(package_diagnostics);
        return checked;
    }
    let manifest = bundle
        .files
        .get("Cargo.toml")
        .and_then(SourceFile::as_text)
        .and_then(|source| source.parse::<toml::Value>().ok());
    let Some(manifest) = manifest else {
        checked.diagnostics.push(diagnostic(
            Lang::Rust,
            "LOOM_MANIFEST",
            "Invalid Cargo.toml",
        ));
        return checked;
    };
    if let Some(deps) = manifest
        .get("loom")
        .and_then(|loom| loom.get("deps"))
        .and_then(toml::Value::as_table)
    {
        for (name, hash) in deps {
            if let Some(hash) = hash.as_str() {
                checked
                    .deps
                    .entry(name.clone())
                    .or_insert_with(|| hash.trim_start_matches('#').to_owned());
            } else {
                checked.diagnostics.push(diagnostic(
                    Lang::Rust,
                    "LOOM_DEP",
                    "loom.deps entries must be hashes",
                ));
            }
        }
    }
    if let Err(error) = crate_dependencies(bundle.files["Cargo.toml"].as_text().unwrap()) {
        checked
            .diagnostics
            .push(diagnostic(Lang::Rust, "LOOM_CRATE", &error));
    }
    for (name, contents) in &mut bundle.files {
        if name.starts_with("vendor/") {
            continue;
        }
        let Some(source) = contents.text_mut() else {
            continue;
        };
        *source = source.replace("\r\n", "\n");
        if name.ends_with(".rs") {
            let file_request = DefineRequest {
                lang: Lang::Rust,
                name: request.name.clone(),
                source: source.clone(),
                deps: checked.deps.clone(),
                allowed_effects: request.allowed_effects.clone(),
            };
            let file = check_rust_file(&file_request, signatures);
            checked.deps.extend(file.deps.clone());
            *source = file.source;
            if name == "src/lib.rs" {
                checked.sig = file.sig;
            }
            checked
                .diagnostics
                .extend(file.diagnostics.into_iter().map(|mut error| {
                    error.file = name.clone();
                    error
                }));
        }
    }
    fn opaque_dependencies(value: &toml::Value, known: &BTreeMap<String, TypeSig>) -> bool {
        let Some(table) = value.as_table() else {
            return false;
        };
        table.iter().any(|(name, value)| {
            if ["dependencies", "build-dependencies", "dev-dependencies"].contains(&name.as_str()) {
                value.as_table().is_some_and(|dependencies| {
                    dependencies.iter().any(|(name, _)| {
                        !["serde", "serde_json", "loom"].contains(&name.as_str())
                            && !known.contains_key(name)
                    })
                })
            } else {
                opaque_dependencies(value, known)
            }
        })
    }
    if manifest
        .get("loom")
        .and_then(|loom| loom.get("crates"))
        .and_then(toml::Value::as_table)
        .is_some_and(|crates| !crates.is_empty())
        || opaque_dependencies(&manifest, signatures)
        || manifest
            .get("package")
            .and_then(|package| package.get("build"))
            .is_some()
        || bundle.files.contains_key("build.rs")
    {
        checked.sig.effects.unknown = true;
        for export in &mut checked.sig.exports {
            export.effects.unknown = true;
            if export.effects.declared.is_none() {
                checked.diagnostics.push(diagnostic(
                    Lang::Rust,
                    "LOOM_EFFECT_ROW",
                    &format!(
                        "{} has unknown dependency effects; declare its residual host row",
                        export.name
                    ),
                ));
            }
        }
    }
    match serde_json::to_string(&bundle) {
        Ok(source) => checked.source = source,
        Err(error) => {
            checked
                .diagnostics
                .push(diagnostic(Lang::Rust, "LOOM_BUNDLE", &error.to_string()))
        }
    }
    checked
}
#[cfg(test)]
mod tests;
