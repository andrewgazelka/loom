use super::*;

/// Macros that read the host at compile time: files and environment variables end up in the
/// module, outside the runtime's per-tenant boundary.
const HOST_READING_MACROS: [&str; 5] = ["include", "include_str", "include_bytes", "env", "option_env"];

/// `include!`, `include_str!` and `include_bytes!` read a file. One whose only argument is a
/// relative string literal with no `..` names a file inside the package: it sits under the scanned
/// source directory (its text is checked like any other), so it reaches nothing outside. An
/// absolute path, a parent escape or a computed path (`concat!(env!(..), ..)`) is refused, and so
/// are `env!` and `option_env!`, which read the host's environment.
fn is_file_include(name: &str) -> bool {
    matches!(name, "include" | "include_str" | "include_bytes")
}

fn in_package_path(tokens: &proc_macro2::TokenStream) -> bool {
    let Ok(literal) = syn::parse2::<syn::LitStr>(tokens.clone()) else {
        return false;
    };
    let path = std::path::Path::new(literal.value().as_str()).to_owned();
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
}

/// Attributes that name a symbol, section or import of the module. They must not
/// appear in source, nor in any macro's tokens, where a `macro_rules!` body or a
/// macro argument could expand to one.
const ABI_ATTRIBUTES: [&str; 4] = ["no_mangle", "export_name", "link_section", "link_name"];

/// What definition source may not do. `unsafe` code is allowed: a guest runs in its own
/// wasm linear memory in a per-tenant runtime, so memory unsafety reaches only itself, and
/// the host checks every import against the call's declared effects and records what runs.
/// What stays refused is what reaches the HOST or the COMPILER, outside that boundary:
/// source inclusion and `#[path]` (compile-time host file reads), foreign `extern` blocks
/// and symbol or section attributes (forged imports and exports), compiler-internal
/// attributes and unconditional feature gates (a gate inside `cfg_attr` is dormant, or a
/// compile error under `-Zallow-features=`), and macro tokens that name a symbol, section
/// or import attribute. Build scripts, proc macros and symlinks are refused by
/// `safety::untrusted_package_diagnostics`.
pub(crate) fn unsafe_source_diagnostics(file: &syn::File) -> Vec<loom_proto::Diagnostic> {
    struct UnsafeSource {
        messages: BTreeSet<String>,
    }
    impl UnsafeSource {
        fn reject(&mut self, kind: &str) {
            self.messages.insert(format!(
                "{kind} is forbidden in definition source"
            ));
        }
    }
    impl<'ast> Visit<'ast> for UnsafeSource {
        fn visit_macro(&mut self, node: &'ast syn::Macro) {
            if let Some(segment) = node.path.segments.last()
                && HOST_READING_MACROS.iter().any(|name| segment.ident == name)
                && !(is_file_include(&segment.ident.to_string()) && in_package_path(&node.tokens))
            {
                self.reject("compile-time host access (include, env)");
            }
            fn contains_include(tokens: proc_macro2::TokenStream) -> bool {
                tokens.into_iter().any(|token| match token {
                    proc_macro2::TokenTree::Ident(name) => name == "include" || name == "include_str" || name == "include_bytes",
                    proc_macro2::TokenTree::Group(group) => contains_include(group.stream()),
                    _ => false,
                })
            }
            if contains_include(node.tokens.clone()) {
                self.reject("source inclusion in macro input or definition");
            }
            fn names_abi_attribute(tokens: proc_macro2::TokenStream) -> bool {
                use syn::ext::IdentExt;
                tokens.into_iter().any(|token| match token {
                    proc_macro2::TokenTree::Ident(name) => {
                        ABI_ATTRIBUTES.contains(&name.unraw().to_string().as_str())
                    }
                    proc_macro2::TokenTree::Group(group) => names_abi_attribute(group.stream()),
                    _ => false,
                })
            }
            if names_abi_attribute(node.tokens.clone()) {
                self.reject(
                    "symbol, section or import attribute (no_mangle, export_name, link_section, link_name) in macro tokens",
                );
            }
            visit::visit_macro(self, node);
        }
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            fn includes(tree: &syn::UseTree) -> bool {
                match tree {
                    syn::UseTree::Name(name) => HOST_READING_MACROS.iter().any(|host| name.ident == host),
                    syn::UseTree::Rename(name) => HOST_READING_MACROS.iter().any(|host| name.ident == host),
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
        fn visit_item_foreign_mod(&mut self, item: &'ast syn::ItemForeignMod) {
            self.reject("foreign extern block");
            visit::visit_item_foreign_mod(self, item);
        }
        fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
            // `conditional`: nested inside `cfg_attr`. A feature gate there is
            // dormant until its condition holds, and an active one is a compile
            // error because dependency and guest units build with
            // `-Zallow-features=` (`rustc/capture.sh`); registry crates carry
            // `#![cfg_attr(feature = "specialization", feature(specialization))]`.
            fn inspect(meta: &syn::Meta, checker: &mut UnsafeSource, conditional: bool) {
                use syn::ext::IdentExt;
                let path = meta.path();
                let name = path
                    .segments
                    .last()
                    .map(|segment| segment.ident.unraw().to_string())
                    .unwrap_or_default();
                if name.starts_with("rustc_")
                    || (name == "feature" && !conditional)
                    || [
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
                    // `#[unsafe(no_mangle)]` is `no_mangle`: look inside.
                    if name == "unsafe" {
                        use syn::parse::Parser;
                        if let Ok(nested)=syn::punctuated::Punctuated::<syn::Meta,syn::Token![,]>::parse_terminated.parse2(list.tokens.clone()) {
                            for attribute in nested.iter() {inspect(attribute,checker,conditional);}
                        } else {checker.reject("unparseable unsafe attribute");}
                    }
                    if name == "cfg_attr" {
                        use syn::parse::Parser;
                        if let Ok(nested)=syn::punctuated::Punctuated::<syn::Meta,syn::Token![,]>::parse_terminated.parse2(list.tokens.clone()) {
                            for attribute in nested.iter().skip(1) {inspect(attribute,checker,true);}
                        } else {checker.reject("unparseable conditional attribute");}
                    }
                }
            }
            inspect(&attribute.meta, self, false);
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
    fn rejects_what_reaches_the_host_or_compiler_not_unsafe_code_itself() {
        for source in [
            "unsafe extern \"C\" { fn foreign(); }",
            "#[unsafe(no_mangle)] fn exported() {}",
            "#[no_mangle] fn exported() {}",
            "#[unsafe(export_name = \"loom_call_main\")] fn exported() {}",
            "extern \"C\" { fn legacy_foreign(); }",
            "#![feature(core_intrinsics)] fn main() {}",
            "#![cfg_attr(all(), cfg_attr(all(), no_mangle))] fn main() {}",
            "#[cfg_attr(all(), no_mangle)] fn exported() {}",
            "#[cfg_attr(docsrs, unsafe(export_name = \"loom_call_main\"))] fn exported() {}",
            "#[cfg_attr(docsrs, link_section = \".init\")] static S: u8 = 0;",
            "#[r#no_mangle] fn exported() {}",
            "macro_rules! bad { () => { #[unsafe(no_mangle)] pub fn exported() {} } }",
            "macro_rules! bad { ($item:item) => { $item } } bad! { #[no_mangle] fn exported() {} }",
            "macro_rules! bad { ($attribute:meta) => { #[$attribute] fn exported() {} } } bad!(export_name = \"x\");",
            "macro_rules! bad { () => { extern \"C\" { #[link_name = \"secret\"] fn hidden(); } } }",
            "#[allow_internal_unsafe] macro_rules! bad {()=>{0}}",
            "#[rustc_allow_const_fn_unstable(foo)] fn main() {}",
            "#[path=\"../outside.rs\"] mod outside;",
            "include!(\"../outside.rs\");",
            "include!(\"/etc/passwd\");",
            "include!(concat!(env!(\"HOME\"), \"/x.rs\"));",
            "const S: &str = include_str!(\"/etc/passwd\");",
            "const B: &[u8] = include_bytes!(\"../secret\");",
            "const H: &str = env!(\"HOME\");",
            "const H: Option<&str> = option_env!(\"HOME\");",
            "use core::include as load; load!(\"inside.rs\");",
            "fn main() { generate!({include!(\"outside.rs\")}); }",
        ] {
            let file = syn::parse_file(source).unwrap();
            assert!(
                !unsafe_source_diagnostics(&file).is_empty(),
                "accepted {source}"
            );
        }
        for source in [
            "mod features { include!(\"features/impl_encase.rs\"); }",
            "const DATA: &[u8] = include_bytes!(\"data/table.bin\");",
            "pub fn main() { let values = vec![1, 2]; let _ = values[0]; }",
            "fn main() { unsafe { operation(); } }",
            "unsafe fn operation() {}",
            "unsafe trait Marker {}",
            "unsafe impl Send for User {}",
            "struct User; impl User { unsafe fn operation() {} }",
            "#![allow(unsafe_code)] fn main() { unsafe { operation(); } }",
            "macro_rules! hidden { () => { unsafe { operation(); } } }",
            // Registry crates gate nightly features behind `cfg_attr`; the
            // compiler refuses an active gate (`-Zallow-features=`).
            "#![cfg_attr(any(), feature(core_intrinsics))] fn main() {}",
            "#![cfg_attr(docsrs, feature(doc_cfg))] pub fn main() {}",
            "#![cfg_attr(feature = \"specialization\", feature(specialization))] pub fn main() {}",
            "#![cfg_attr(feature = \"specialization\", allow(incomplete_features))] pub fn main() {}",
            "#![cfg_attr(all(), cfg_attr(all(), feature(core_intrinsics)))] pub fn main() {}",
            "#[cfg_attr(docsrs, doc(cfg(feature = \"const_new\")))] pub fn new() {}",
            "macro_rules! smallvec { ($($x:expr),*) => { { let mut v = Vec::new(); $(v.push($x);)* v } } }",
        ] {
            let file = syn::parse_file(source).unwrap();
            assert!(
                unsafe_source_diagnostics(&file).is_empty(),
                "refused {source}"
            );
        }
    }
}
