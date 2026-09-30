use super::*;

use proc_macro2::{Delimiter, Spacing, TokenStream, TokenTree};
use syn::ext::IdentExt;

/// Macros that read the host at compile time: files and environment variables end up in the
/// module, outside the runtime's per-tenant boundary.
const HOST_READING_MACROS: [&str; 5] = [
    "include",
    "include_str",
    "include_bytes",
    "env",
    "option_env",
];

/// Top-level names in a package that the dependency scan skips (`direct/admission.rs`,
/// `inspect_untrusted_source` with `generated_root`): the host places vendored sources, pinned
/// crates and cargo configuration there. `include!` must not name a file under one, or it could
/// reach source the scan never read.
const UNSCANNED_ROOTS: [&str; 3] = ["vendor", "loom-crates", ".cargo"];

/// The name of an identifier without a raw prefix: `r#env` is `env`. `proc_macro2::Ident`
/// compares equal to `"env"` only when it is not raw, so every comparison goes through here.
fn name(ident: &proc_macro2::Ident) -> String {
    ident.unraw().to_string()
}

/// Whether the host-reading macro `macro_name` given `arguments` reads only package data or
/// source the scanners read. The only argument must be a relative string literal with no `..`:
/// the path is relative to the including file, so it stays under the scanned source directory.
/// `include_str!` and `include_bytes!` read data and accept any extension. `include!` compiles
/// the text, so it accepts only a `.rs` file (the scanners read `*.rs` and nothing else) whose
/// first component is not a directory the dependency scan skips. `env!`, `option_env!`, an
/// absolute path, a parent escape and a computed path (`concat!(env!(..), ..)`) are refused.
fn reads_only_package(macro_name: &str, arguments: &TokenStream) -> bool {
    let Ok(literal) = syn::parse2::<syn::LitStr>(arguments.clone()) else {
        return false;
    };
    let value = literal.value();
    let path = std::path::Path::new(&value);
    let relative = !path.as_os_str().is_empty()
        && path
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)));
    match macro_name {
        "include_str" | "include_bytes" => relative,
        "include" => {
            relative
                && path.extension().is_some_and(|extension| extension == "rs")
                && path.components().next().is_some_and(|first| {
                    !UNSCANNED_ROOTS
                        .iter()
                        .any(|root| first.as_os_str() == *root)
                })
        }
        _ => false,
    }
}

/// Attributes that name a symbol, section, import or allocator of the module. They must not
/// appear in source, nor in any macro's tokens, where a `macro_rules!` body or a
/// macro argument could expand to one.
const ABI_ATTRIBUTES: [&str; 7] = [
    "no_mangle",
    "export_name",
    "link_section",
    "link_name",
    "global_allocator",
    "panic_handler",
    "alloc_error_handler",
];

struct UnsafeSource {
    messages: BTreeSet<String>,
}

impl UnsafeSource {
    fn reject(&mut self, kind: &str) {
        self.messages
            .insert(format!("{kind} is forbidden in definition source"));
    }

    /// Scan the tokens of a macro invocation, a `macro_rules!` body or a `cfg_attr` argument list
    /// recursively. Tokens are opaque to `syn`, and a macro can expand them into anything, so what
    /// the compiler would refuse or read from the host is refused by shape:
    /// - `include*` in any position unless it is `include*!("literal")` reading only package data
    ///   (a bare `include_str` handed to `$name!(..)` would otherwise read anything);
    /// - `env!` and `option_env!`;
    /// - the ABI attribute names, `path = <literal>`, `link(name = ..)`, and `extern` followed by a
    ///   block (a foreign block, which also forges imports);
    /// - `$name!`: a macro whose name comes from a metavariable could be handed any of the above.
    fn scan_tokens(&mut self, tokens: TokenStream) {
        fn is_punct(token: Option<&TokenTree>, character: char) -> bool {
            matches!(token, Some(TokenTree::Punct(punct)) if punct.as_char() == character)
        }
        // `! (..)`, `! [..]` or `! {..}`: what follows a macro's name (not `!=` or `!x`).
        fn calls_macro(after: &[TokenTree]) -> bool {
            is_punct(after.first(), '!') && matches!(after.get(1), Some(TokenTree::Group(_)))
        }
        // `=` at `index` that is an assignment: not part of `==`, `<=`, `=>`, ...
        fn assigns(tokens: &[TokenTree], index: usize) -> bool {
            let joined_before = index > 0
                && matches!(&tokens[index - 1], TokenTree::Punct(punct) if punct.spacing() == Spacing::Joint);
            matches!(tokens.get(index), Some(TokenTree::Punct(punct))
                if punct.as_char() == '=' && punct.spacing() == Spacing::Alone)
                && !joined_before
        }
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            let after = &tokens[index + 1..];
            match token {
                TokenTree::Group(group) => self.scan_tokens(group.stream()),
                TokenTree::Ident(ident) => {
                    let ident = name(ident);
                    if ABI_ATTRIBUTES.contains(&ident.as_str()) {
                        self.reject(
                            "symbol, section, import or allocator attribute (no_mangle, export_name, link_section, link_name, global_allocator) in macro tokens",
                        );
                    }
                    match ident.as_str() {
                        "include" | "include_str" | "include_bytes" => {
                            let conforming = calls_macro(after)
                                && matches!(after.get(1), Some(TokenTree::Group(group))
                                    if reads_only_package(&ident, &group.stream()));
                            if !conforming {
                                self.reject("source inclusion in macro input or definition");
                            }
                        }
                        "env" | "option_env" if calls_macro(after) => {
                            self.reject("compile-time host access (include, env)");
                        }
                        "path"
                            if assigns(&tokens, index + 1)
                                && (matches!(after.get(1), Some(TokenTree::Literal(_)))
                                    || is_punct(after.get(1), '$')) =>
                        {
                            self.reject("#[path] in macro tokens");
                        }
                        "link" => {
                            if let Some(TokenTree::Group(group)) = after.first()
                                && group.delimiter() == Delimiter::Parenthesis
                                && {
                                    let inner: Vec<TokenTree> =
                                        group.stream().into_iter().collect();
                                    (0..inner.len()).any(|at| assigns(&inner, at))
                                }
                            {
                                self.reject("#[link] in macro tokens");
                            }
                        }
                        "extern" => {
                            let block = match after.first() {
                                Some(TokenTree::Literal(_)) => after.get(1),
                                other => other,
                            };
                            if matches!(block, Some(TokenTree::Group(group))
                                if group.delimiter() == Delimiter::Brace)
                            {
                                self.reject("foreign extern block");
                            }
                        }
                        _ => {}
                    }
                    if index > 0 && is_punct(tokens.get(index - 1), '$') && calls_macro(after) {
                        self.reject("a macro named by a metavariable");
                    }
                }
                _ => {}
            }
        }
    }
}

/// What definition source may not do. `unsafe` code is allowed: a guest runs in its own
/// wasm linear memory in a per-tenant runtime, so memory unsafety reaches only itself, and
/// the host checks every import against the call's declared effects and records what runs.
/// What stays refused is what reaches the HOST or the COMPILER, outside that boundary:
/// source inclusion and `#[path]` (compile-time host file reads), foreign `extern` blocks
/// and symbol or section attributes (forged imports and exports), compiler-internal
/// attributes and unconditional feature gates (a gate inside `cfg_attr` is dormant, or a
/// compile error under `-Zallow-features=`), and macro tokens that name a symbol, section
/// or import attribute or read the host. `include!` is admitted only for a relative `.rs`
/// file (see `reads_only_package`): the dependency scan reads exactly the `*.rs` files under a
/// compiled target's source directory, so an included file is scanned as source and a file in
/// any other form is refused. Build scripts, proc macros and symlinks are refused by
/// `safety::untrusted_package_diagnostics`.
pub(crate) fn unsafe_source_diagnostics(file: &syn::File) -> Vec<loom_proto::Diagnostic> {
    impl<'ast> Visit<'ast> for UnsafeSource {
        fn visit_macro(&mut self, node: &'ast syn::Macro) {
            if let Some(segment) = node.path.segments.last() {
                let macro_name = name(&segment.ident);
                if HOST_READING_MACROS.contains(&macro_name.as_str())
                    && !reads_only_package(&macro_name, &node.tokens)
                {
                    self.reject("compile-time host access (include, env)");
                }
            }
            self.scan_tokens(node.tokens.clone());
            visit::visit_macro(self, node);
        }
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            fn imports_host_macro(tree: &syn::UseTree) -> bool {
                let host = |ident: &proc_macro2::Ident| {
                    HOST_READING_MACROS.contains(&name(ident).as_str())
                };
                match tree {
                    syn::UseTree::Name(leaf) => host(&leaf.ident),
                    syn::UseTree::Rename(leaf) => host(&leaf.ident),
                    syn::UseTree::Path(path) => imports_host_macro(&path.tree),
                    syn::UseTree::Group(group) => group.items.iter().any(imports_host_macro),
                    syn::UseTree::Glob(_) => false,
                }
            }
            if imports_host_macro(&item.tree) {
                self.reject("host-reading macro import (include, env)");
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
                        // The argument expressions (`doc = include_str!(..)`) are opaque
                        // tokens here; scan them like macro input.
                        checker.scan_tokens(list.tokens.clone());
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
            "fn main() { generate!({include!(\"/outside.rs\")}); }",
            // `include!` compiles text the scanners read only when it is a `.rs` file.
            "include!(\"payload.txt\");",
            "include!(\"payload\");",
            "include!(\"vendor/x.rs\");",
            "include!(\"loom-crates/x.rs\");",
            "include!(\".cargo/x.rs\");",
            "include!(\"./x.rs\");",
            "include!(\"\");",
            "include!();",
            // Raw identifiers name the same macros.
            "const S: &str = r#include_str!(\"/etc/passwd\");",
            "const H: &str = r#env!(\"HOME\");",
            "const H: &str = core::r#env!(\"HOME\");",
            "r#include!(\"../outside.rs\");",
            "use core::r#include_str as x;",
            "use core::{r#env, r#option_env as e};",
            "macro_rules! m { () => { r#env!(\"X\") } }",
            // Host reads and ABI attributes inside opaque token positions.
            "const H: &str = concat!(env!(\"HOME\"), \"\");",
            "macro_rules! leak { () => { env!(\"X\") } }",
            "macro_rules! leak { () => { std::option_env!(\"X\") } }",
            "macro_rules! leak { () => { include_bytes!(\"../secret\") } }",
            "macro_rules! leak { ($p:literal) => { include!($p) } }",
            "macro_rules! leak { ($($t:tt)*) => { include_str!($($t)*) } } leak!(\"/etc/passwd\");",
            "macro_rules! leak { ($name:ident) => { $name!(\"HOME\") } } leak!(env);",
            "#![cfg_attr(all(), doc = include_str!(\"/etc/passwd\"))] fn main() {}",
            "#![cfg_attr(all(), doc = env!(\"HOME\"))] fn main() {}",
            "#[doc = include_str!(\"/etc/passwd\")] struct S;",
            "macro_rules! m { () => { #[path = \"/abs.rs\"] mod x; } }",
            "macro_rules! m { ($p:literal) => { #[path = $p] mod x; } }",
            "macro_rules! m { () => { #[r#path = \"/abs.rs\"] mod x; } }",
            "macro_rules! m { () => { #[link(wasm_import_module = \"loom\")] extern \"C\" { fn f(); } } }",
            "macro_rules! m { () => { #[link(name = \"c\")] unsafe extern {} } }",
            "macro_rules! m { () => { extern \"C\" { fn f(); } } }",
            "macro_rules! m { () => { #[global_allocator] static A: System = System; } }",
            "macro_rules! m { () => { #[panic_handler] fn f() -> ! { loop {} } } }",
            "consume!(path = \"/abs.rs\");",
        ] {
            let file = syn::parse_file(source).unwrap();
            assert!(
                !unsafe_source_diagnostics(&file).is_empty(),
                "accepted {source}"
            );
        }
        for source in [
            "mod features { include!(\"features/impl_encase.rs\"); }",
            "include!(\"x.rs\");",
            "const DATA: &[u8] = include_bytes!(\"data/table.bin\");",
            "const TEXT: &str = include_str!(\"x.txt\");",
            "const TEXT: &str = include_str!(\"vendor/notes.md\");",
            "fn main() { generate!({include!(\"inside.rs\")}); }",
            "const S: &str = concat!(\"a\", \"b\");",
            // Names that overlap the refused ones, used as plain identifiers or other shapes.
            "fn main() { let path = 1; let env = 2; println!(\"{} {}\", path, env); }",
            "fn main() { let link = 1; let _ = vec![link, link == 1, link != 2]; }",
            "macro_rules! m { ($path:expr, $a:expr, $b:expr) => { ($path.len(), $a != $b, $a == $b) } }",
            "macro_rules! m { () => { extern crate alloc; } }",
            "macro_rules! m { () => { extern \"C\" fn f() {} } }",
            "macro_rules! m { () => { link(a == b) } }",
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
