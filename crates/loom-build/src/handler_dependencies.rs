//! Content-addressed handlers are statically linked Rust definitions, not plugins.
use std::collections::BTreeMap;
use loom_check::{CheckedDef, SourceBundle, SourceFile};
use crate::BuildError;

pub(crate) fn validate(definition: &CheckedDef, dependencies: &BTreeMap<String, CheckedDef>) -> Result<(), BuildError> {
    for (alias, hash) in &definition.deps {
        if !alias.starts_with("loom_handler_") { continue; }
        // The alias is a source label: upgrades rewrite its pinned value while
        // retaining the already-normalized Rust path. Identity covers that edge.
        let target = dependencies.get(hash).ok_or_else(|| BuildError::Rejected(format!("handler definition {hash} is not stored")))?;
        if target.lang != loom_proto::Lang::Rust {
            return Err(BuildError::Rejected(format!("handler {hash} must be a Rust definition sharing the caller's SDK")));
        }
        let source = if target.source.trim_start().starts_with('{') {
            let bundle: SourceBundle = serde_json::from_str(&target.source).map_err(|error| BuildError::Rejected(error.to_string()))?;
            bundle.files.get("src/lib.rs").and_then(SourceFile::as_text).ok_or_else(|| BuildError::Rejected(format!("handler {hash} requires src/lib.rs")))?.to_owned()
        } else { target.source.clone() };
        let file = syn::parse_file(&source).map_err(|error| BuildError::Rejected(error.to_string()))?;
        if !file.items.iter().any(|item| matches!(item, syn::Item::Fn(function) if function.sig.ident == "handle" && matches!(function.vis, syn::Visibility::Public(_)))) {
            return Err(BuildError::Rejected(format!("handler {hash} must export pub fn handle(Op, Continuation) -> Reply; rustc checks its complete type")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn definition(hash: &str, source: &str) -> CheckedDef {
        CheckedDef { hash: hash.into(), lang: loom_proto::Lang::Rust, name: "handler".into(), source: source.into(), deps: BTreeMap::new(), sig: loom_proto::TypeSig::default(), diagnostics: Vec::new() }
    }
    #[test]
    fn pinned_handler_requires_stored_public_export() {
        let hash = "a".repeat(64);
        let mut caller = definition(&"b".repeat(64), "fn main() {}");
        caller.deps.insert(format!("loom_handler_{hash}"), hash.clone());
        assert!(validate(&caller, &BTreeMap::new()).unwrap_err().to_string().contains("not stored"));
        let mut dependencies = BTreeMap::new();
        dependencies.insert(hash.clone(), definition(&hash, "fn handle() {}"));
        assert!(validate(&caller, &dependencies).unwrap_err().to_string().contains("pub fn handle"));
        dependencies.insert(hash.clone(), definition(&hash, "pub fn handle(op: loom::Op, k: loom::Continuation) -> loom::Reply { loom::Reply::Forward }"));
        assert!(validate(&caller, &dependencies).is_ok());
    }
    #[test]
    fn upgrading_a_pin_preserves_its_source_alias() {
        let old_hash = "a".repeat(64);
        let new_hash = "b".repeat(64);
        let mut caller = definition(&"c".repeat(64), "fn main() {}");
        caller.deps.insert(format!("loom_handler_{old_hash}"), new_hash.clone());
        let target = definition(&new_hash, "pub fn handle(op: loom::Op, k: loom::Continuation) -> loom::Reply { loom::Reply::Forward }");
        let dependencies = BTreeMap::from([(new_hash, target)]);
        assert!(validate(&caller, &dependencies).is_ok());
    }

}
