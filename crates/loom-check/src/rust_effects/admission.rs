use super::*;

pub(crate) fn unsafe_source_diagnostics(file: &syn::File) -> Vec<loom_proto::Diagnostic> {
    struct UnsafeSource {
        messages: BTreeSet<String>,
    }
    impl UnsafeSource {
        fn reject(&mut self, kind: &str) {
            self.messages.insert(format!(
                "{kind} is forbidden in definition source; definition sources must use safe Rust"
            ));
        }
    }
    impl<'ast> Visit<'ast> for UnsafeSource {
        fn visit_macro(&mut self, node: &'ast syn::Macro) {
            if node
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "include")
            {
                self.reject("external source inclusion");
            }
            fn contains_unsafe(tokens: proc_macro2::TokenStream) -> bool {
                tokens.into_iter().any(|token| match token {
                    proc_macro2::TokenTree::Ident(name) => name == "unsafe" || name == "include",
                    proc_macro2::TokenTree::Group(group) => contains_unsafe(group.stream()),
                    _ => false,
                })
            }
            if contains_unsafe(node.tokens.clone()) {
                self.reject("unsafe tokens in macro input or definition");
            }
            visit::visit_macro(self, node);
        }
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            fn includes(tree: &syn::UseTree) -> bool {
                match tree {
                    syn::UseTree::Name(name) => name.ident == "include",
                    syn::UseTree::Rename(name) => name.ident == "include",
                    syn::UseTree::Path(path) => includes(&path.tree),
                    syn::UseTree::Group(group) => group.items.iter().any(includes),
                    syn::UseTree::Glob(_) => false,
                }
            }
            if includes(&item.tree) {
                self.reject("source inclusion macro import");
            }
            visit::visit_item_use(self, item);
        }
        fn visit_expr_unsafe(&mut self, expression: &'ast syn::ExprUnsafe) {
            self.reject("unsafe block");
            visit::visit_expr_unsafe(self, expression);
        }
        fn visit_signature(&mut self, signature: &'ast syn::Signature) {
            if signature.unsafety.is_some() {
                self.reject("unsafe function");
            }
            visit::visit_signature(self, signature);
        }
        fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
            if item.unsafety.is_some() {
                self.reject("unsafe impl");
            }
            visit::visit_item_impl(self, item);
        }
        fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
            if item.unsafety.is_some() {
                self.reject("unsafe trait");
            }
            visit::visit_item_trait(self, item);
        }
        fn visit_item_foreign_mod(&mut self, item: &'ast syn::ItemForeignMod) {
            self.reject("foreign extern block");
            visit::visit_item_foreign_mod(self, item);
        }
        fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
            fn inspect(meta: &syn::Meta, checker: &mut UnsafeSource) {
                let path = meta.path();
                let name = path
                    .segments
                    .last()
                    .map(|segment| segment.ident.to_string())
                    .unwrap_or_default();
                if name.starts_with("rustc_")
                    || [
                        "unsafe",
                        "feature",
                        "allow_internal_unsafe",
                        "allow_internal_unstable",
                        "no_core",
                        "lang",
                        "prelude_import",
                        "global_allocator",
                        "panic_handler",
                        "alloc_error_handler",
                        "no_mangle",
                        "export_name",
                        "link_section",
                        "link",
                        "link_name",
                        "path",
                        "proc_macro",
                        "proc_macro_attribute",
                        "proc_macro_derive",
                    ]
                    .contains(&name.as_str())
                {
                    checker.reject(&format!("compiler or ABI attribute {name}"));
                }
                if let syn::Meta::List(list) = meta {
                    if name == "cfg_attr" {
                        use syn::parse::Parser;
                        if let Ok(nested)=syn::punctuated::Punctuated::<syn::Meta,syn::Token![,]>::parse_terminated.parse2(list.tokens.clone()) {
                            for attribute in nested.iter().skip(1) {inspect(attribute,checker);}
                        } else {checker.reject("unparseable conditional attribute");}
                    }
                    if (name == "allow" || name == "expect")
                        && list.tokens.clone().into_iter().any(|token|matches!(token,proc_macro2::TokenTree::Ident(name) if name=="unsafe_code" || name=="unsafe_op_in_unsafe_fn"))
                    {
                        checker.reject("unsafe lint override");
                    }
                }
            }
            inspect(&attribute.meta, self);
            visit::visit_attribute(self, attribute);
        }
    }
    let mut checker = UnsafeSource {
        messages: BTreeSet::new(),
    };
    checker.visit_file(file);
    checker
        .messages
        .into_iter()
        .map(|message| crate::diagnostic(loom_proto::Lang::Rust, "LOOM_UNSAFE", &message))
        .collect()
}

#[cfg(test)]
mod unsafe_tests {
    use super::*;
    #[test]
    fn rejects_unsafe_constructs_even_inside_safe_helpers() {
        for source in [
            "fn main() { unsafe { operation(); } }",
            "unsafe fn operation() {}",
            "unsafe trait Marker {}",
            "unsafe impl Send for User {}",
            "unsafe extern \"C\" { fn foreign(); }",
            "#[unsafe(no_mangle)] fn exported() {}",
            "struct User; impl User { unsafe fn operation() {} }",
            "#![allow(unsafe_code)] fn main() { unsafe { operation(); } }",
            "macro_rules! hidden { () => { unsafe { operation(); } } }",
            "fn main() { generate!({unsafe fn hidden() {}}); }",
            "extern \"C\" { fn legacy_foreign(); }",
            "#![feature(core_intrinsics)] fn main() {}",
            "#![cfg_attr(any(), feature(core_intrinsics))] fn main() {}",
            "#![cfg_attr(all(), cfg_attr(all(), allow(unsafe_code)))] fn main() {}",
            "#[allow_internal_unsafe] macro_rules! bad {()=>{0}}",
            "#[rustc_allow_const_fn_unstable(foo)] fn main() {}",
            "#![expect(unsafe_code)] fn main() {}",
            "#[path=\"../outside.rs\"] mod outside;",
            "include!(\"outside.rs\");",
            "use core::include as load; load!(\"outside.rs\");",
        ] {
            let file = syn::parse_file(source).unwrap();
            assert!(
                !unsafe_source_diagnostics(&file).is_empty(),
                "accepted {source}"
            );
        }
        let safe = syn::parse_file(
            "#[loom::def] fn main() { let values = vec![1, 2]; let _ = values[0]; }",
        )
        .unwrap();
        assert!(unsafe_source_diagnostics(&safe).is_empty());
    }
}

pub(crate) fn unsupported_mode_diagnostics(file: &syn::File) -> Vec<loom_proto::Diagnostic> {
    struct Attributes {
        threaded: bool,
    }
    impl<'ast> Visit<'ast> for Attributes {
        fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
            if attribute
                .path()
                .segments
                .last()
                .is_some_and(|part| part.ident == "def")
            {
                let _ = attribute.parse_nested_meta(|meta| {
                    self.threaded |= meta.path.is_ident("threads");
                    Ok(())
                });
            }
        }
    }
    let mut attributes = Attributes { threaded: false };
    attributes.visit_file(file);
    if attributes.threaded {
        vec![crate::diagnostic(
            loom_proto::Lang::Rust,
            "LOOM_THREADS_UNSUPPORTED",
            "#[loom::def(threads)] is unsupported; use the ordinary #[loom::def] entrypoint",
        )]
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod unsupported_mode_tests {
    use super::*;
    #[test]
    fn threaded_abi_is_rejected_and_isolated_definition_is_allowed() {
        let threaded = syn::parse_file("#[loom::def(threads)] fn main() {} ").unwrap();
        assert_eq!(
            unsupported_mode_diagnostics(&threaded)[0].code,
            "LOOM_THREADS_UNSUPPORTED"
        );
        let isolated = syn::parse_file("#[loom::def] fn main() {} ").unwrap();
        assert!(unsupported_mode_diagnostics(&isolated).is_empty());
    }
}
