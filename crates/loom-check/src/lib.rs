//! Language checking before definitions become executable identities.
mod rust_effects;
use loom_proto::{DefineRequest, Diagnostic, ExportSig, Lang, ParamSig, TypeSig, ValueShape};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf, process::Stdio};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
};

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
    #[error("checker sidecar: {0}")]
    Sidecar(String),
}
struct Sidecar {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}
pub struct Checker {
    root: PathBuf,
    sidecar: Mutex<Option<Sidecar>>,
}
#[derive(Deserialize)]
struct TsResult {
    canonical: String,
    sig: TypeSig,
    diagnostics: Vec<Diagnostic>,
}
impl Checker {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            sidecar: Mutex::new(None),
        }
    }
    pub async fn check(&self, request: &DefineRequest) -> Result<CheckedDef, CheckError> {
        self.check_with_signatures(request, &BTreeMap::new()).await
    }
    pub async fn check_with_signatures(
        &self,
        request: &DefineRequest,
        signatures: &BTreeMap<String, TypeSig>,
    ) -> Result<CheckedDef, CheckError> {
        let mut checked = match request.lang {
            Lang::Ts => {
                let mut guard = self.sidecar.lock().await;
                if guard.is_none() {
                    let mut child = Command::new("bun")
                        .arg(self.root.join("loom-checker/checker.ts"))
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::inherit())
                        .kill_on_drop(true)
                        .spawn()?;
                    let input = child
                        .stdin
                        .take()
                        .ok_or_else(|| CheckError::Sidecar("missing stdin".into()))?;
                    let output = BufReader::new(
                        child
                            .stdout
                            .take()
                            .ok_or_else(|| CheckError::Sidecar("missing stdout".into()))?,
                    );
                    *guard = Some(Sidecar {
                        child,
                        input,
                        output,
                    });
                }
                let sidecar = guard
                    .as_mut()
                    .ok_or_else(|| CheckError::Sidecar("not running".into()))?;
                let mut message = serde_json::to_value(request)?;
                message["dep_sigs"] = serde_json::to_value(signatures)?;
                let mut bytes = serde_json::to_vec(&message)?;
                bytes.push(b'\n');
                sidecar.input.write_all(&bytes).await?;
                sidecar.input.flush().await?;
                let mut line = String::new();
                if sidecar.output.read_line(&mut line).await? == 0 {
                    let status = sidecar.child.wait().await?;
                    *guard = None;
                    return Err(CheckError::Sidecar(format!("exited {status}")));
                }
                let response: serde_json::Value = serde_json::from_str(&line)?;
                if let Some(error) = response.get("error") {
                    return Err(CheckError::Sidecar(error.to_string()));
                }
                let result: TsResult = serde_json::from_value(response)?;
                CheckedDef {
                    hash: String::new(),
                    lang: request.lang,
                    name: request.name.clone(),
                    source: result.canonical,
                    deps: request.deps.clone(),
                    sig: result.sig,
                    diagnostics: result.diagnostics,
                }
            }
            Lang::Rust => check_rust(request, signatures),
        };
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
        file: if lang == Lang::Rust {
            "src/lib.rs"
        } else {
            "definition.ts"
        }
        .into(),
        line: 1,
        col: 1,
        code: code.into(),
        message: message.into(),
        snippet: None,
        hint: Some("Use loom abilities for host I/O.".into()),
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
    if opaque_dependencies(&manifest, signatures)
        || manifest
            .get("package")
            .and_then(|package| package.get("build"))
            .is_some()
        || bundle.files.contains_key("build.rs")
    {
        checked.sig.effects.unknown = true;
        for export in &mut checked.sig.exports {
            export.effects.unknown = true;
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
fn check_rust_file(request: &DefineRequest, signatures: &BTreeMap<String, TypeSig>) -> CheckedDef {
    let source = request.source.replace("\r\n", "\n");
    let mut diagnostics = Vec::new();
    let mut exports = Vec::new();
    let mut aggregate_effects = loom_proto::EffectSet::default();
    let source = match syn::parse_file(&source) {
        Ok(file) => {
            struct EntryVisitor {
                count: usize,
            }
            impl<'ast> syn::visit::Visit<'ast> for EntryVisitor {
                fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
                    if attribute
                        .path()
                        .segments
                        .last()
                        .is_some_and(|segment| segment.ident == "def" || segment.ident == "actor")
                    {
                        self.count += 1;
                    }
                    syn::visit::visit_attribute(self, attribute);
                }
            }
            let mut entries = EntryVisitor { count: 0 };
            syn::visit::Visit::visit_file(&mut entries, &file);
            if entries.count > 1 {
                diagnostics.push(diagnostic(Lang::Rust,"LOOM_ENTRYPOINT","A definition crate must have one #[loom::def] or #[loom::actor] entrypoint; place reusable functions in separate hashed definitions."));
            }
            let effects = rust_effects::infer(&file, signatures);
            for item in &file.items {
                if let syn::Item::Fn(function) = item
                    && function.attrs.iter().any(|attribute| {
                        attribute
                            .path()
                            .segments
                            .last()
                            .is_some_and(|segment| segment.ident == "def")
                    })
                {
                    let params: Vec<ParamSig> = function
                        .sig
                        .inputs
                        .iter()
                        .filter_map(|argument| {
                            let syn::FnArg::Typed(argument) = argument else {
                                return None;
                            };
                            let name = if let syn::Pat::Ident(binding) = argument.pat.as_ref() {
                                binding.ident.to_string()
                            } else {
                                "argument".into()
                            };
                            Some(ParamSig {
                                name,
                                shape: rust_type_shape(&argument.ty),
                            })
                        })
                        .collect();
                    let returns = match &function.sig.output {
                        syn::ReturnType::Default => ValueShape::Null,
                        syn::ReturnType::Type(_, ty) => rust_type_shape(ty),
                    };
                    exports.push(ExportSig {
                        name: function.sig.ident.to_string(),
                        params,
                        returns,
                        effects: effects
                            .get(&function.sig.ident.to_string())
                            .cloned()
                            .unwrap_or_default(),
                    });
                }
            }
            aggregate_effects = rust_effects::aggregate(&file, signatures, &effects, &exports);
            fn ambient_macro(tokens: proc_macro2::TokenStream) -> bool {
                tokens.into_iter().any(|token| match token {
                    proc_macro2::TokenTree::Ident(name) => [
                        "include",
                        "include_str",
                        "include_bytes",
                        "env",
                        "option_env",
                    ]
                    .contains(&name.to_string().as_str()),
                    proc_macro2::TokenTree::Group(group) => ambient_macro(group.stream()),
                    _ => false,
                })
            }
            struct IoVisitor {
                violations: Vec<String>,
            }
            impl<'ast> syn::visit::Visit<'ast> for IoVisitor {
                fn visit_macro(&mut self, mac: &'ast syn::Macro) {
                    if mac.path.segments.last().is_some_and(|segment| {
                        [
                            "include",
                            "include_str",
                            "include_bytes",
                            "env",
                            "option_env",
                        ]
                        .contains(&segment.ident.to_string().as_str())
                    }) || ambient_macro(mac.tokens.clone())
                    {
                        self.violations
                            .push("compile-time ambient input macro".into());
                    }
                    syn::visit::visit_macro(self, mac);
                }
                fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
                    if attribute.path().is_ident("path") {
                        self.violations.push("external module path".into());
                    }
                    fn conditional_path(meta: &syn::Meta) -> bool {
                        if meta.path().is_ident("path") {
                            return true;
                        }
                        if let syn::Meta::List(list) = meta
                            && list.path.is_ident("cfg_attr")
                            && let Ok(attributes)=list.parse_args_with(syn::punctuated::Punctuated::<syn::Meta,syn::Token![,]>::parse_terminated){return attributes.iter().skip(1).any(conditional_path);}
                        false
                    }
                    if conditional_path(&attribute.meta) {
                        self.violations.push("external module path".into());
                    }
                    if let syn::Meta::List(list) = &attribute.meta
                        && ambient_macro(list.tokens.clone())
                    {
                        self.violations
                            .push("compile-time ambient input attribute".into());
                    }
                    syn::visit::visit_attribute(self, attribute);
                }
                fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
                    fn forbidden(prefix: &[String]) -> bool {
                        prefix.last().is_some_and(|name| {
                            [
                                "include",
                                "include_str",
                                "include_bytes",
                                "env",
                                "option_env",
                            ]
                            .contains(&name.as_str())
                        }) || (prefix.first().is_some_and(|name| name == "std")
                            && prefix.get(1).is_some_and(|name| {
                                ["fs", "net", "time", "env", "process"].contains(&name.as_str())
                            }))
                    }
                    fn inspect(tree: &syn::UseTree, mut prefix: Vec<String>) -> bool {
                        match tree {
                            syn::UseTree::Path(path) => {
                                prefix.push(path.ident.to_string());
                                inspect(&path.tree, prefix)
                            }
                            syn::UseTree::Group(group) => {
                                group.items.iter().any(|item| inspect(item, prefix.clone()))
                            }
                            syn::UseTree::Name(name) => {
                                prefix.push(name.ident.to_string());
                                forbidden(&prefix)
                            }
                            syn::UseTree::Rename(rename) => {
                                prefix.push(rename.ident.to_string());
                                forbidden(&prefix)
                            }
                            syn::UseTree::Glob(_) => {
                                prefix.first().is_some_and(|name| name == "std")
                            }
                        }
                    }
                    if inspect(&item.tree, Vec::new()) {
                        self.violations.push("ambient import".into());
                    }
                    syn::visit::visit_item_use(self, item);
                }
                fn visit_path(&mut self, path: &'ast syn::Path) {
                    let segments: Vec<String> = path
                        .segments
                        .iter()
                        .map(|segment| segment.ident.to_string())
                        .collect();
                    if segments.first().is_some_and(|s| s == "std")
                        && segments.get(1).is_some_and(|s| {
                            ["fs", "net", "time", "env", "process"].contains(&s.as_str())
                        })
                    {
                        self.violations.push(segments.join("::"));
                    }
                    syn::visit::visit_path(self, path);
                }
            }
            let mut visitor = IoVisitor {
                violations: Vec::new(),
            };
            syn::visit::Visit::visit_file(&mut visitor, &file);
            for path in visitor.violations {
                diagnostics.push(diagnostic(
                    Lang::Rust,
                    "LOOM_IO",
                    &format!("{path} is unavailable; use loom abilities."),
                ));
            }
            prettyplease::unparse(&file)
        }
        Err(error) => {
            diagnostics.push(diagnostic(Lang::Rust, "RUST_PARSE", &error.to_string()));
            source
        }
    };
    CheckedDef {
        hash: String::new(),
        lang: Lang::Rust,
        name: request.name.clone(),
        source,
        deps: request.deps.clone(),
        sig: TypeSig {
            exports,
            effects: aggregate_effects,
        },
        diagnostics,
    }
}

fn rust_type_shape(ty: &syn::Type) -> ValueShape {
    match ty {
        syn::Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return ValueShape::Value;
            };
            let name = segment.ident.to_string();
            match name.as_str() {
                "u8" | "u16" | "u32" | "u64" | "usize" | "i8" | "i16" | "i32" | "i64" | "isize"
                | "f32" | "f64" => ValueShape::Number,
                "bool" => ValueShape::Boolean,
                "String" | "str" => ValueShape::String,
                "Vec" | "Ref" | "Result" => {
                    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
                        return ValueShape::Value;
                    };
                    let inner = arguments
                        .args
                        .iter()
                        .find_map(|argument| {
                            if let syn::GenericArgument::Type(ty) = argument {
                                Some(rust_type_shape(ty))
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    match name.as_str() {
                        "Vec" => ValueShape::Array {
                            items: Box::new(inner),
                        },
                        "Ref" => ValueShape::Ref {
                            target: Box::new(inner),
                        },
                        _ => inner,
                    }
                }
                _ => ValueShape::Value,
            }
        }
        syn::Type::Reference(reference) => rust_type_shape(&reference.elem),
        syn::Type::Tuple(tuple) if tuple.elems.is_empty() => ValueShape::Null,
        _ => ValueShape::Value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn rust_rejects_ambient_io_and_hashes_formatting_stably() {
        let checker = Checker::new(PathBuf::new());
        let mut request = DefineRequest {
            lang: Lang::Rust,
            name: "test".into(),
            source: "pub fn f()->u32 { 2 }".into(),
            deps: BTreeMap::new(),
            allowed_effects: None,
        };
        let first = checker.check(&request).await.unwrap();
        request.source = "pub fn f() -> u32 {\n 2\n}\n".into();
        assert_eq!(first.hash, checker.check(&request).await.unwrap().hash);
        request.allowed_effects = Some(vec![]);
        assert_ne!(first.hash, checker.check(&request).await.unwrap().hash);
        request.source = "pub fn f() { std::fs::read(\"secret\").unwrap(); }".into();
        assert!(
            !checker
                .check(&request)
                .await
                .unwrap()
                .diagnostics
                .is_empty()
        );
    }
    #[tokio::test]
    async fn external_crate_initialization_is_not_claimed_pure() {
        let request=DefineRequest {
            lang:Lang::Rust,name:"external".into(),deps:BTreeMap::new(),allowed_effects:None,
            source:serde_json::json!({"files":{"Cargo.toml":"[package]\nname='external'\nversion='0.1.0'\n[dependencies]\nthird_party='1'\n","src/lib.rs":"#[loom::def] pub fn main()->i64 {42}"}}).to_string(),
        };
        let checked = Checker::new(PathBuf::new()).check(&request).await.unwrap();
        assert!(checked.diagnostics.is_empty());
        assert!(checked.sig.effects.unknown);
        assert!(checked.sig.exports[0].effects.unknown);
    }
    #[tokio::test]
    async fn rejects_compiler_file_reads_and_macro_aliases() {
        let checker = Checker::new(PathBuf::new());
        for source in [
            r#"#[loom::def] fn f()->String { include_str!("/etc/passwd").into() }"#,
            r#"#[loom::def] fn f()->String { include_str!("/tmp/secret").into() }"#,
            r#"#[loom::def] fn f()->String { env!("LOOM_TOKEN").into() }"#,
            r#"use core::include_str as secret; #[loom::def] fn f()->String {secret!("/etc/passwd").into()}"#,
            r#"#[cfg_attr(all(),path="/etc/passwd")]mod secret;"#,
            r#"use std::{fs as files}; #[loom::def]fn f(){let _=files::read("/tmp/x");}"#,
        ] {
            let request = DefineRequest {
                lang: Lang::Rust,
                name: "test".into(),
                source: source.into(),
                deps: BTreeMap::new(),
                allowed_effects: None,
            };
            assert!(
                checker
                    .check(&request)
                    .await
                    .unwrap()
                    .diagnostics
                    .iter()
                    .any(|error| error.code == "LOOM_IO"),
                "{source}"
            );
        }
    }
    #[test]
    fn bundle_bytes_roundtrip_and_paths_are_bounded() {
        let original = vec![0, 255, 128, 1];
        assert_eq!(
            SourceFile::from_bytes(original.clone()).bytes().unwrap(),
            original
        );
        let mut files = BTreeMap::new();
        files.insert("Cargo.toml".into(), SourceFile::Text("[package]".into()));
        files.insert("src/lib.rs".into(), SourceFile::Text(String::new()));
        files.insert("../escape".into(), SourceFile::Text(String::new()));
        assert!(SourceBundle { files }.validate().is_err());
    }
}
