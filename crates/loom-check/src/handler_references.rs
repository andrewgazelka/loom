//! Literal handler references become ordinary, identity-covered dependency edges.
use loom_proto::{Diagnostic, Lang};
use std::collections::BTreeMap;
use syn::visit_mut::{self, VisitMut};

pub(crate) fn lower(
    file: &mut syn::File,
    deps: &mut BTreeMap<String, String>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    struct Lower<'a> {
        deps: &'a mut BTreeMap<String, String>,
        diagnostics: &'a mut Vec<Diagnostic>,
    }
    impl VisitMut for Lower<'_> {
        fn visit_expr_mut(&mut self, expression: &mut syn::Expr) {
            visit_mut::visit_expr_mut(self, expression);
            let syn::Expr::Call(call) = expression else {
                return;
            };
            let syn::Expr::Path(function) = call.func.as_ref() else {
                return;
            };
            let segments = &function.path.segments;
            if segments.len() != 2
                || segments[0].ident != "loom"
                || segments[1].ident != "handle_with"
            {
                return;
            }
            let hash = match call.args.first() {
                Some(syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Str(value),
                    ..
                })) => value.value(),
                _ => String::new(),
            };
            if call.args.len() != 2
                || hash.len() != 64
                || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                self.diagnostics.push(super::diagnostic(Lang::Rust, "LOOM_HANDLER_HASH", "handle_with requires a literal 64-character definition hash and a body closure; runtime handler loading is unavailable."));
                return;
            }
            let alias = format!("loom_handler_{}", hash);
            if self
                .deps
                .get(&alias)
                .is_some_and(|existing| existing != &hash)
            {
                self.diagnostics.push(super::diagnostic(
                    Lang::Rust,
                    "LOOM_HANDLER_HASH",
                    "handler dependency alias conflicts with its content hash",
                ));
                return;
            }
            self.deps.insert(alias.clone(), hash);
            let alias = syn::Ident::new(&alias, proc_macro2::Span::call_site());
            let body = &call.args[1];
            // Absolute paths prevent local modules from replacing either ABI endpoint.
            *expression = syn::parse_quote!(::loom::handle(::#alias::handle, #body));
        }
    }
    Lower { deps, diagnostics }.visit_file_mut(file);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn literal_reference_is_an_identity_edge() {
        let hash = "a".repeat(64);
        let mut file = syn::parse_file(&format!(
            "fn main() {{ loom::handle_with(\"{hash}\", || 7); }}"
        ))
        .unwrap();
        let mut deps = BTreeMap::new();
        let mut errors = Vec::new();
        lower(&mut file, &mut deps, &mut errors);
        assert!(errors.is_empty());
        assert_eq!(deps.get(&format!("loom_handler_{hash}")), Some(&hash));
        assert!(!prettyplease::unparse(&file).contains("handle_with"));
    }
    #[test]
    fn runtime_reference_is_rejected() {
        let mut file = syn::parse_file("fn main() { loom::handle_with(hash, || 7); }").unwrap();
        let mut deps = BTreeMap::new();
        let mut errors = Vec::new();
        lower(&mut file, &mut deps, &mut errors);
        assert_eq!(errors[0].code, "LOOM_HANDLER_HASH");
        assert!(deps.is_empty());
    }
}
