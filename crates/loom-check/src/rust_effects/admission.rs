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

/// The name of an identifier without a raw prefix: `r#env` is `env`. `proc_macro2::Ident`
/// compares equal to `"env"` only when it is not raw, so every comparison goes through here.
fn name(ident: &proc_macro2::Ident) -> String {
    ident.unraw().to_string()
}

/// Whether the host-reading macro `macro_name` given `arguments` reads only package data or
/// source the scanners read. The only argument must be a relative string literal with no `..`
/// (`crate::safety::relative_literal`: a leading `./` and a trailing comma are fine): the path is
/// relative to the including file, so it stays under that file's directory. `include_str!` and
/// `include_bytes!` read data and accept any extension. `include!` compiles the text, so it accepts
/// only a `.rs` file (the scanners read `*.rs` and nothing else). `env!`, `option_env!`, an
/// absolute path, a parent escape and a computed path (`concat!(env!(..), ..)`) are refused.
///
/// Where the included file lies is a fact about the package, not about the text: a root package's
/// top-level `vendor/`, `loom-crates/` and `.cargo/` are skipped by the package scan, so
/// `safety::untrusted_package_diagnostics` resolves every `include!` against its including file
/// and refuses one that lands there.
fn reads_only_package(macro_name: &str, arguments: &TokenStream) -> bool {
    let Some(path) = crate::safety::relative_literal(arguments) else {
        return false;
    };
    match macro_name {
        "include_str" | "include_bytes" => true,
        "include" => path.extension().is_some_and(|extension| extension == "rs"),
        _ => false,
    }
}

/// Attributes that name a symbol, section, import or allocator of the module, or reach the
/// compiler. `ABI_ATTRIBUTES` are refused as a bare word anywhere in macro tokens (no ordinary
/// code uses those words); the rest only where they head an attribute.
const ABI_ATTRIBUTES: [&str; 7] = [
    "no_mangle",
    "export_name",
    "link_section",
    "link_name",
    "global_allocator",
    "panic_handler",
    "alloc_error_handler",
];

/// `conditional`: nested inside `cfg_attr`. A feature gate there is dormant until its condition
/// holds, and an active one is a compile error because dependency and guest units build with
/// `-Zallow-features=` (`rustc/capture.sh`); registry crates carry
/// `#![cfg_attr(feature = "specialization", feature(specialization))]`.
fn forbidden_attribute(attribute: &str, conditional: bool) -> bool {
    attribute.starts_with("rustc_")
        || (attribute == "feature" && !conditional)
        || ABI_ATTRIBUTES.contains(&attribute)
        || [
            "allow_internal_unsafe",
            "allow_internal_unstable",
            "no_core",
            "lang",
            "prelude_import",
            "link",
            "path",
            "proc_macro",
            "proc_macro_attribute",
            "proc_macro_derive",
        ]
        .contains(&attribute)
}

fn is_punct(token: Option<&TokenTree>, character: char) -> bool {
    matches!(token, Some(TokenTree::Punct(punct)) if punct.as_char() == character)
}

/// `! (..)`, `! [..]` or `! {..}`: what follows a macro's name (not `!=` or `!x`).
fn calls_macro(after: &[TokenTree]) -> bool {
    is_punct(after.first(), '!') && matches!(after.get(1), Some(TokenTree::Group(_)))
}

/// `=` at `index` that is an assignment: not part of `==`, `<=`, `=>`, ...
fn assigns(tokens: &[TokenTree], index: usize) -> bool {
    let joined_before = index > 0
        && matches!(&tokens[index - 1], TokenTree::Punct(punct) if punct.spacing() == Spacing::Joint);
    matches!(tokens.get(index), Some(TokenTree::Punct(punct))
        if punct.as_char() == '=' && punct.spacing() == Spacing::Alone)
        && !joined_before
}

fn split_commas(stream: TokenStream) -> Vec<Vec<TokenTree>> {
    let mut segments = vec![Vec::new()];
    for token in stream {
        if is_punct(Some(&token), ',') {
            segments.push(Vec::new());
        } else if let Some(segment) = segments.last_mut() {
            segment.push(token);
        }
    }
    segments
}

/// The last segment of the path an attribute starts with (`core::arch::foo` is `foo`) and the
/// index after it.
fn attribute_name(tokens: &[TokenTree]) -> Option<(String, usize)> {
    let mut last = None;
    let mut index = 0;
    while let Some(TokenTree::Ident(ident)) = tokens.get(index) {
        last = Some(name(ident));
        index += 1;
        if is_punct(tokens.get(index), ':') && is_punct(tokens.get(index + 1), ':') {
            index += 2;
        } else {
            break;
        }
    }
    last.map(|last| (last, index))
}

/// Whether the attribute whose tokens (between `#[` and `]`) are `tokens` is one this policy
/// refuses, looking through `unsafe(..)` and every `cfg_attr` consequent. With `metavariable`, a
/// `$` where the attribute's name would be (`#[$a = ".."]`, `#[$($t)*]`) also counts: a macro
/// transcriber must not assemble an attribute from its input.
fn attribute_refused(tokens: &[TokenTree], conditional: bool, metavariable: bool) -> bool {
    if metavariable && is_punct(tokens.first(), '$') {
        return true;
    }
    let Some((attribute, next)) = attribute_name(tokens) else {
        return false;
    };
    if forbidden_attribute(&attribute, conditional) {
        return true;
    }
    let Some(TokenTree::Group(group)) = tokens.get(next) else {
        return false;
    };
    if group.delimiter() != Delimiter::Parenthesis {
        return false;
    }
    match attribute.as_str() {
        "unsafe" => {
            let inner: Vec<TokenTree> = group.stream().into_iter().collect();
            attribute_refused(&inner, conditional, metavariable)
        }
        "cfg_attr" => split_commas(group.stream())
            .iter()
            .skip(1)
            .any(|segment| attribute_refused(segment, true, metavariable)),
        _ => false,
    }
}

/// The bracket group of the attribute that starts at `tokens[index]` (a `#`, optionally `#!`).
fn attribute_group(tokens: &[TokenTree], index: usize) -> Option<&proc_macro2::Group> {
    if !is_punct(tokens.get(index), '#') {
        return None;
    }
    let mut at = index + 1;
    if is_punct(tokens.get(at), '!') {
        at += 1;
    }
    match tokens.get(at) {
        Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Bracket => Some(group),
        _ => None,
    }
}

/// Whether the macro name before the `!` at `tokens[bang]` is (or goes through) a metavariable or a
/// repetition: `$name!(..)`, `$m::inner!(..)`, `$($t)*!(..)`, `$($t)sep*!(..)`. `$crate` is the
/// defining crate, not input.
fn macro_named_by_input(tokens: &[TokenTree], bang: usize) -> bool {
    let Some(mut at) = bang.checked_sub(1) else {
        return false;
    };
    if matches!(&tokens[at], TokenTree::Punct(punct) if matches!(punct.as_char(), '*' | '+' | '?'))
    {
        // `$( .. ) *` or `$( .. ) sep *`.
        let group = |index: usize| {
            index.checked_sub(1).is_some_and(|dollar| {
                matches!(&tokens[index], TokenTree::Group(_)) && is_punct(tokens.get(dollar), '$')
            })
        };
        return at.checked_sub(1).is_some_and(group) || at.checked_sub(2).is_some_and(group);
    }
    loop {
        let TokenTree::Ident(ident) = &tokens[at] else {
            return false;
        };
        if at > 0 && is_punct(tokens.get(at - 1), '$') {
            return name(ident) != "crate";
        }
        if at >= 3 && is_punct(tokens.get(at - 1), ':') && is_punct(tokens.get(at - 2), ':') {
            at -= 3;
        } else {
            return false;
        }
    }
}

struct UnsafeSource {
    messages: BTreeSet<String>,
}

impl UnsafeSource {
    fn reject(&mut self, kind: &str) {
        self.messages
            .insert(format!("{kind} is forbidden in definition source"));
    }

    /// Scan the tokens of a macro invocation, for what the compiler would refuse or read from the
    /// host. Tokens are opaque to `syn`, and a macro can expand them into anything, so this is by
    /// shape. Everywhere in the tokens, recursively:
    /// - `include*` unless it is `include*!("literal")` reading only package data;
    /// - `env!` and `option_env!`, and `env`/`option_env` followed directly by a group;
    /// - the ABI attribute names as bare words;
    /// - attributes that are refused in source (`#[path ..]`, `#[link ..]`, `#![feature ..]`, ..),
    ///   through `unsafe(..)` and `cfg_attr`;
    /// - `path = <literal>`, `link(name = ..)`, and `extern` followed by a block.
    ///
    /// Words like `path`, `link` and `env` are NOT refused as plain arguments (`format!("{}", path)`,
    /// `vec![link]`): what makes them dangerous is a transcriber gluing them into an attribute or a
    /// macro name, and `scan_transcriber` refuses that where the macro is defined.
    fn scan_tokens(&mut self, tokens: TokenStream) {
        let tokens: Vec<TokenTree> = tokens.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            let after = &tokens[index + 1..];
            if let Some(group) = attribute_group(&tokens, index) {
                let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                if attribute_refused(&inner, false, false) {
                    self.reject("compiler or ABI attribute in macro tokens");
                }
            }
            match token {
                TokenTree::Group(group) => {
                    self.scan_tokens(group.stream());
                }
                TokenTree::Ident(ident) => {
                    let ident = name(ident);
                    if ABI_ATTRIBUTES.contains(&ident.as_str()) {
                        self.reject(
                            "symbol, section, import or allocator attribute (no_mangle, export_name, link_section, link_name, global_allocator) in macro tokens",
                        );
                    }
                    if ident == "macro_rules"
                        && is_punct(after.first(), '!')
                        && let Some(TokenTree::Group(rules)) = after.get(2)
                    {
                        self.scan_rules(rules.stream());
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
                        "env" | "option_env"
                            if calls_macro(after)
                                || matches!(after.first(), Some(TokenTree::Group(_))) =>
                        {
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
                }
                _ => {}
            }
        }
    }

    /// The body of a `macro_rules!` definition: `(matcher) => { transcriber };` rules. The
    /// matcher and the transcriber are both scanned like macro tokens; the transcriber, which
    /// builds code from what the call gives it, must also not build the security-relevant parts of
    /// an item from input (`scan_transcriber`).
    fn scan_rules(&mut self, rules: TokenStream) {
        let tokens: Vec<TokenTree> = rules.into_iter().collect();
        for index in 0..tokens.len() {
            if is_punct(tokens.get(index), '=')
                && is_punct(tokens.get(index + 1), '>')
                && let Some(TokenTree::Group(transcriber)) = tokens.get(index + 2)
            {
                self.scan_transcriber(transcriber.stream());
            }
        }
    }

    /// What a transcriber may not do, recursively through its groups. Input (a metavariable `$x`
    /// or a repetition `$(..)*`) may not be:
    /// - where an attribute's name goes (`#[$a = ".."]`, `#![$($t)*]`, also inside `unsafe(..)` and
    ///   `cfg_attr`): the call would pick the attribute, and the review found `m!(path)` and
    ///   `m!(= "/abs.rs")` splitting `path` from its value;
    /// - a macro's name or a segment of its path (`$m!(..)`, `$m::inner!(..)`, `$($t)*!(..)`):
    ///   the call would pick `env!` or `include_str!`;
    /// - after `extern` (`extern "C" $block`): the call would supply a foreign block.
    fn scan_transcriber(&mut self, stream: TokenStream) {
        let tokens: Vec<TokenTree> = stream.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            let after = &tokens[index + 1..];
            if let Some(group) = attribute_group(&tokens, index) {
                let inner: Vec<TokenTree> = group.stream().into_iter().collect();
                if attribute_refused(&inner, false, true) {
                    self.reject("an attribute built from macro input or refused in source");
                }
            }
            match token {
                TokenTree::Group(group) => self.scan_transcriber(group.stream()),
                TokenTree::Ident(ident) if name(ident) == "extern" => {
                    let next = match after.first() {
                        Some(TokenTree::Literal(_)) => after.get(1),
                        other => other,
                    };
                    if is_punct(next, '$') {
                        self.reject("a foreign block or ABI built from macro input");
                    }
                }
                TokenTree::Punct(punct)
                    if punct.as_char() == '!'
                        && matches!(after.first(), Some(TokenTree::Group(_))) =>
                {
                    if macro_named_by_input(&tokens, index) {
                        self.reject("a macro named by macro input");
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
/// or import attribute or read the host. A `macro_rules!` transcriber may not build an
/// attribute, a macro name or an `extern` block out of its input (`scan_transcriber`), so the
/// refused forms cannot be assembled from pieces that are each harmless. `include!` is admitted
/// only for a relative `.rs` file (see `reads_only_package`). Build scripts, proc macros and
/// symlinks are refused by `safety::untrusted_package_diagnostics`, which also checks where an
/// included file lies.
pub(crate) fn unsafe_source_diagnostics(file: &syn::File) -> Vec<loom_proto::Diagnostic> {
    impl<'ast> Visit<'ast> for UnsafeSource {
        fn visit_macro(&mut self, node: &'ast syn::Macro) {
            let mut is_definition = false;
            if let Some(segment) = node.path.segments.last() {
                let macro_name = name(&segment.ident);
                if HOST_READING_MACROS.contains(&macro_name.as_str())
                    && !reads_only_package(&macro_name, &node.tokens)
                {
                    self.reject("compile-time host access (include, env)");
                }
                is_definition = macro_name == "macro_rules";
            }
            if is_definition {
                self.scan_rules(node.tokens.clone());
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
            fn inspect(meta: &syn::Meta, checker: &mut UnsafeSource, conditional: bool) {
                let path = meta.path();
                let name = path
                    .segments
                    .last()
                    .map(|segment| segment.ident.unraw().to_string())
                    .unwrap_or_default();
                if forbidden_attribute(&name, conditional) {
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
            "include!(\"\");",
            "include!(\"./\");",
            "include!();",
            "include!(\"a.rs\", \"b.rs\");",
            "include_str!(\"../README.md\");",
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
            // Split forms: no single token tree is refused on its own, the pieces are assembled by
            // the macro (macro input may not build an attribute, a macro name or an extern block).
            "macro_rules! m { ($a:ident) => { #[$a = \"/etc/passwd\"] mod x; } } m!(path);",
            "macro_rules! m { ($($t:tt)*) => { #[path $($t)*] mod x; } } m!(= \"/abs.rs\");",
            "macro_rules! m { ($($t:tt)*) => { #![$($t)*] } }",
            "macro_rules! m { ($a:ident) => { #[unsafe($a)] fn f() {} } }",
            "macro_rules! m { ($a:meta) => { #[cfg_attr(all(), $a)] fn f() {} } }",
            "macro_rules! m { ($($a:tt)*) => { #[cfg_attr(all(), $($a)*)] fn f() {} } }",
            "macro_rules! m { ($m:meta) => { $(#[$m])* fn f() {} } }",
            "macro_rules! m { ($($t:tt)*) => { $($t)*!(\"HOME\") } } const X: &str = m!(env);",
            "macro_rules! m { ($($t:tt),*) => { $($t),*!(\"HOME\") } }",
            "macro_rules! m { ($p:path) => { $p::helper!(1) } }",
            "macro_rules! m { ($b:tt) => { extern \"C\" $b } } m!({ fn f(); });",
            "macro_rules! m { ($b:tt) => { unsafe extern $b } }",
            "macro_rules! m { ($abi:literal, $b:tt) => { extern $abi $b } }",
            "macro_rules! m { ($($t:tt)*) => { #[link($($t)*)] fn f() {} } } m!(name = \"c\");",
            "macro_rules! m { ($($t:tt)*) => { #[no_mangle] $($t)* } }",
            "macro_rules! m { ($($t:tt)*) => { #[path = \"../x.rs\"] $($t)* } }",
            // The call sites of those macros, for macros whose definition is not in view.
            "m!(path = \"/abs.rs\");",
            "m!(link(name = \"c\"));",
            "m!(#[path = \"/abs.rs\"] mod x;);",
            "m!(#![feature(core_intrinsics)]);",
            "m!(#[cfg_attr(all(), path = \"/abs.rs\")] mod x;);",
            "const X: &str = m!(env(\"HOME\"));",
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
            "include!(\"./x.rs\");",
            "const TEXT: &str = include_str!(\"x.txt\",);",
            "include!(\"sub/vendor/x.rs\");",
            "fn main() { generate!({include!(\"inside.rs\")}); }",
            "const S: &str = concat!(\"a\", \"b\");",
            // Names that overlap the refused ones, used as plain identifiers or other shapes.
            "fn main() { let path = 1; let env = 2; let link = path + env; let _ = link; }",
            "fn f(path: &str, env: &str) -> usize { path.len() + env.len() }",
            "fn f(path: &str) -> String { format!(\"{path}/{}\", path.len()) }",
            "fn f(path: &str) -> usize { assert_eq!(path.len(), 1); path.len() }",
            "macro_rules! len { ($path:expr) => { $path.len() } } fn f(path: &str) -> usize { len!(path.trim()) }",
            // A host-sounding word is an ordinary identifier as an argument: what is dangerous is a
            // transcriber gluing it into an attribute or a macro name, refused where it is defined.
            "m!(path);",
            "m!(link);",
            "m!(env);",
            "m!(a, option_env);",
            "outer!(inner!(path));",
            "fn f(path: &str, link: u32, env: &str) { let _ = format!(\"{} {} {}\", path, link, env); }",
            "fn f(path: &str) { let _ = format!(\"{path}\", path = path); }",
            "fn f(link: u32) { let _ = vec![link, link + 1]; assert_eq!(link, 1); }",
            "macro_rules! twice { ($e:expr) => { ($e, $e) } } fn f(a: &str) -> (usize, usize) { twice!(a.len()) }",
            // Attribute passthrough that names nothing: derive lists and docs from input.
            "macro_rules! doc { ($d:expr) => { #[doc = $d] pub struct S; } }",
            "macro_rules! derive { ($($d:ident),*) => { #[derive($($d),*)] pub struct S; } }",
            "macro_rules! ctor { ($name:ident) => { pub fn $name() -> u32 { 1 } } }",
            "macro_rules! call { ($f:ident) => { $f() } }",
            "macro_rules! with { ($($x:expr),*) => { vec![$($x),*] } } fn f() { let _ = with!(1, 2); }",
            "macro_rules! nested { ($($x:tt)*) => { std::format!($($x)*) } }",
            "macro_rules! not { ($a:expr, $b:expr) => { !($a == $b) && $a != $b } }",
            "macro_rules! flags { ($($n:ident = $v:expr;)*) => { $(pub const $n: u32 = $v;)* } }",
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
