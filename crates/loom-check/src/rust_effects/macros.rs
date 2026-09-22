//! Guest source may use built-in derives, the pinned toolchain's own macros, and
//! its own `macro_rules!`; nothing here executes user code on the build host.
//!
//! Expansion happens inside rustc, after this pass. `tools/hash-rustc` hashes and
//! infers effects from the expanded HIR, so no macro can hide a call from identity
//! or from the effect row (docs/content-addressed-code.md, "Macros in guest
//! source"). This pass refuses the two things expansion would otherwise let in:
//! code that runs at compile time (procedural macros, attribute macros, and
//! derives from crates) and conditional compilation that would give the source a
//! shape the compiled crate does not have. Everything not in the tables below is
//! refused, and the diagnostic names the item and prints the table.
//!
//! Macro bodies and macro inputs are token trees, not syntax, so the same tables
//! are applied to every `name!(...)` and `#[...]` shape found inside them. The
//! sibling passes (`LOOM_UNSAFE`, `LOOM_IO`) scan those tokens the same way.
use super::*;
use std::fmt::Write as _;

/// A derive implemented inside rustc. `module` is the `core`/`std` module that
/// re-exports the derive macro, so `#[derive(core::clone::Clone)]` is the same item.
struct BuiltinDerive {
    name: &'static str,
    module: &'static str,
}

const BUILTIN_DERIVES: &[BuiltinDerive] = &[
    BuiltinDerive {
        name: "Clone",
        module: "clone",
    },
    BuiltinDerive {
        name: "Copy",
        module: "marker",
    },
    BuiltinDerive {
        name: "Debug",
        module: "fmt",
    },
    BuiltinDerive {
        name: "Default",
        module: "default",
    },
    BuiltinDerive {
        name: "Eq",
        module: "cmp",
    },
    BuiltinDerive {
        name: "Hash",
        module: "hash",
    },
    BuiltinDerive {
        name: "Ord",
        module: "cmp",
    },
    BuiltinDerive {
        name: "PartialEq",
        module: "cmp",
    },
    BuiltinDerive {
        name: "PartialOrd",
        module: "cmp",
    },
];

/// A declarative or compiler-built-in macro whose expansion runs no user code.
/// `roots` are the crate paths under which the same macro is also spelled.
struct ToolchainMacro {
    name: &'static str,
    roots: &'static [&'static str],
}

const CORE: &[&str] = &["core", "std"];
const ALLOC: &[&str] = &["alloc", "std"];
const SERDE_JSON: &[&str] = &["serde_json", "loom::serde_json"];

/// Excluded on purpose: `print!`/`println!`/`eprint!`/`eprintln!`/`dbg!` (the
/// wasm32-unknown-unknown guest has no stdio; std silently discards the bytes,
/// a success-lookalike), `file!`/`line!`/`column!`/`module_path!` (they bind an
/// item's identity to source layout, which formatting must not move),
/// `include!`/`include_str!`/`include_bytes!`/`env!`/`option_env!` (compile-time
/// ambient input, also refused as `LOOM_IO`), `cfg!` (one build configuration
/// exists), `thread_local!` and `compile_error!` (no guest use).
const TOOLCHAIN_MACROS: &[ToolchainMacro] = &[
    ToolchainMacro {
        name: "assert",
        roots: CORE,
    },
    ToolchainMacro {
        name: "assert_eq",
        roots: CORE,
    },
    ToolchainMacro {
        name: "assert_ne",
        roots: CORE,
    },
    ToolchainMacro {
        name: "concat",
        roots: CORE,
    },
    ToolchainMacro {
        name: "debug_assert",
        roots: CORE,
    },
    ToolchainMacro {
        name: "debug_assert_eq",
        roots: CORE,
    },
    ToolchainMacro {
        name: "debug_assert_ne",
        roots: CORE,
    },
    ToolchainMacro {
        name: "format",
        roots: ALLOC,
    },
    ToolchainMacro {
        name: "format_args",
        roots: CORE,
    },
    ToolchainMacro {
        name: "json",
        roots: SERDE_JSON,
    },
    ToolchainMacro {
        name: "matches",
        roots: CORE,
    },
    ToolchainMacro {
        name: "panic",
        roots: CORE,
    },
    ToolchainMacro {
        name: "stringify",
        roots: CORE,
    },
    ToolchainMacro {
        name: "todo",
        roots: CORE,
    },
    ToolchainMacro {
        name: "unimplemented",
        roots: CORE,
    },
    ToolchainMacro {
        name: "unreachable",
        roots: CORE,
    },
    ToolchainMacro {
        name: "vec",
        roots: ALLOC,
    },
    ToolchainMacro {
        name: "write",
        roots: CORE,
    },
    ToolchainMacro {
        name: "writeln",
        roots: CORE,
    },
];

/// The form that defines a local macro. Its name is recorded so the file's own
/// invocations resolve; the body is scanned like any other macro tokens.
const MACRO_DEFINITION: &str = "macro_rules";

/// An attribute the guest may write. `arguments` pins the exact argument tokens
/// when only one spelling is admitted; `None` accepts any arguments.
struct GuestAttribute {
    name: &'static str,
    arguments: Option<&'static str>,
}

/// `derive` is admitted structurally (each path must be a built-in derive) and
/// is not listed here. `cfg(test)` is admitted because the guest build never
/// runs tests: the module is absent from the compiled crate, so it has no
/// identity and no effects, and the source passes still scan it. Any other
/// `cfg` predicate would give the source a shape the one guest build lacks.
/// `default` is the `derive(Default)` variant marker; `test` is the harness
/// attribute rustc strips outside `--test`.
const GUEST_ATTRIBUTES: &[GuestAttribute] = &[
    GuestAttribute {
        name: "allow",
        arguments: None,
    },
    GuestAttribute {
        name: "cfg",
        arguments: Some("test"),
    },
    GuestAttribute {
        name: "cold",
        arguments: None,
    },
    GuestAttribute {
        name: "default",
        arguments: None,
    },
    GuestAttribute {
        name: "deny",
        arguments: None,
    },
    GuestAttribute {
        name: "deprecated",
        arguments: None,
    },
    GuestAttribute {
        name: "doc",
        arguments: None,
    },
    GuestAttribute {
        name: "expect",
        arguments: None,
    },
    GuestAttribute {
        name: "forbid",
        arguments: None,
    },
    GuestAttribute {
        name: "inline",
        arguments: None,
    },
    GuestAttribute {
        name: "must_use",
        arguments: None,
    },
    GuestAttribute {
        name: "non_exhaustive",
        arguments: None,
    },
    GuestAttribute {
        name: "repr",
        arguments: None,
    },
    GuestAttribute {
        name: "test",
        arguments: None,
    },
    GuestAttribute {
        name: "track_caller",
        arguments: None,
    },
    GuestAttribute {
        name: "warn",
        arguments: None,
    },
];

fn path_text(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

fn builtin_derive(path: &syn::Path) -> bool {
    let spelled = path_text(path);
    BUILTIN_DERIVES.iter().any(|derive| {
        spelled == derive.name
            || CORE
                .iter()
                .any(|root| spelled == format!("{root}::{}::{}", derive.module, derive.name))
    })
}

fn toolchain_macro(spelled: &str) -> bool {
    TOOLCHAIN_MACROS.iter().any(|entry| {
        spelled == entry.name
            || entry
                .roots
                .iter()
                .any(|root| spelled == format!("{root}::{}", entry.name))
    })
}

fn guest_attribute(name: &str, arguments: Option<&proc_macro2::TokenStream>) -> bool {
    GUEST_ATTRIBUTES.iter().any(|attribute| {
        attribute.name == name
            && match attribute.arguments {
                None => true,
                Some(exact) => arguments.is_some_and(|tokens| tokens.to_string() == exact),
            }
    })
}

fn derive_list() -> String {
    BUILTIN_DERIVES
        .iter()
        .map(|derive| derive.name)
        .collect::<Vec<_>>()
        .join(", ")
}

fn macro_list() -> String {
    let mut text = String::new();
    for (index, entry) in TOOLCHAIN_MACROS.iter().enumerate() {
        if index > 0 {
            text.push_str(", ");
        }
        let _ = write!(text, "{}! ({})", entry.name, entry.roots.join(", "));
    }
    text
}

fn attribute_list() -> String {
    GUEST_ATTRIBUTES
        .iter()
        .map(|attribute| match attribute.arguments {
            Some(exact) => format!("{}({exact})", attribute.name),
            None => attribute.name.to_owned(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn macro_message(spelled: &str) -> String {
    format!(
        "Macro `{spelled}!` is not available in guest Rust. Available: `macro_rules!` macros defined in this file, invoked by their bare name, and the toolchain macros {}. Procedural macros run code at compile time and are not admitted.",
        macro_list()
    )
}

fn derive_message(spelled: &str) -> String {
    format!(
        "Derive `{spelled}` is not a built-in derive. Available: {} (also spelled through core:: or std::). Derives from crates such as serde are procedural macros, run code at compile time, and are not admitted.",
        derive_list()
    )
}

fn attribute_message(spelled: &str) -> String {
    format!(
        "Attribute `#[{spelled}]` is not available in guest Rust. Available: `derive(...)` with the built-in derives {} and the attributes {}. Attribute macros run code at compile time and are not admitted; export ordinary crate-root pub fn items.",
        derive_list(),
        attribute_list()
    )
}

struct Macros {
    local: BTreeSet<String>,
    messages: BTreeSet<String>,
}

impl Macros {
    fn invocation(&mut self, path: &syn::Path) {
        let spelled = path_text(path);
        let local = path.segments.len() == 1 && self.local.contains(&spelled);
        if spelled != MACRO_DEFINITION && !local && !toolchain_macro(&spelled) {
            self.messages.insert(macro_message(&spelled));
        }
    }

    fn attribute(&mut self, meta: &syn::Meta) {
        let path = meta.path();
        let spelled = path_text(path);
        if path.is_ident("derive") {
            let derives = match meta {
                syn::Meta::List(list) => list
                    .parse_args_with(
                        syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
                    )
                    .ok(),
                _ => None,
            };
            match derives {
                Some(derives) => {
                    for derive in &derives {
                        if !builtin_derive(derive) {
                            self.messages.insert(derive_message(&path_text(derive)));
                        }
                    }
                }
                None => {
                    self.messages
                        .insert(derive_message("<unparseable derive list>"));
                }
            }
            return;
        }
        let arguments = match meta {
            syn::Meta::List(list) => Some(&list.tokens),
            _ => None,
        };
        let inert = path.segments.len() == 1 && guest_attribute(&spelled, arguments);
        if !inert {
            let shown = match meta {
                syn::Meta::List(list) => format!("{spelled}({})", list.tokens),
                _ => spelled,
            };
            self.messages.insert(attribute_message(&shown));
        }
    }

    /// Macro bodies and inputs are token trees. Every `path!(...)` and `#[...]`
    /// shape inside them meets the same tables as syntax does.
    fn tokens(&mut self, stream: proc_macro2::TokenStream) {
        use proc_macro2::{Delimiter, TokenTree};
        let tokens: Vec<TokenTree> = stream.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Group(group) => self.tokens(group.stream()),
                TokenTree::Punct(punct) if punct.as_char() == '!' => {
                    let Some(TokenTree::Group(_)) = tokens.get(index + 1) else {
                        continue;
                    };
                    if let Some(path) = invocation_path(&tokens[..index]) {
                        self.invocation(&path);
                    }
                }
                TokenTree::Punct(punct) if punct.as_char() == '#' => {
                    let mut next = index + 1;
                    if let Some(TokenTree::Punct(inner)) = tokens.get(next)
                        && inner.as_char() == '!'
                    {
                        next += 1;
                    }
                    let Some(TokenTree::Group(group)) = tokens.get(next) else {
                        continue;
                    };
                    if group.delimiter() != Delimiter::Bracket {
                        continue;
                    }
                    // `#[$attr]` forwards an attribute spelled at the call site,
                    // where this scan sees it in full.
                    let fragment = matches!(
                        group.stream().into_iter().next(),
                        Some(TokenTree::Punct(punct)) if punct.as_char() == '$'
                    );
                    if fragment {
                        continue;
                    }
                    match syn::parse2::<syn::Meta>(group.stream()) {
                        Ok(meta) => self.attribute(&meta),
                        Err(_) => {
                            self.messages
                                .insert(attribute_message(&group.stream().to_string()));
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// The `a::b::name` path ending right before a `!`, read backwards from the
/// token stream. A keyword such as `return` before a unary `!` is not a macro
/// name; `self`, `super`, `crate` and `Self` are path segments. A leading `::`
/// is dropped, as `path_text` drops it for syntax.
fn invocation_path(tokens: &[proc_macro2::TokenTree]) -> Option<syn::Path> {
    use proc_macro2::TokenTree;
    let mut segments: Vec<proc_macro2::Ident> = Vec::new();
    let mut end = tokens.len();
    while let Some(TokenTree::Ident(ident)) = end.checked_sub(1).and_then(|last| tokens.get(last)) {
        let path_keyword = matches!(
            ident.to_string().as_str(),
            "self" | "super" | "crate" | "Self"
        );
        let stream = proc_macro2::TokenStream::from(TokenTree::Ident(ident.clone()));
        if !path_keyword && syn::parse2::<syn::Ident>(stream).is_err() {
            break;
        }
        segments.push(ident.clone());
        end -= 1;
        let colons = end >= 2
            && matches!(&tokens[end - 1], TokenTree::Punct(p) if p.as_char() == ':')
            && matches!(&tokens[end - 2], TokenTree::Punct(p) if p.as_char() == ':');
        if !colons {
            break;
        }
        end -= 2;
    }
    if segments.is_empty() {
        return None;
    }
    segments.reverse();
    let mut path = syn::Path {
        leading_colon: None,
        segments: syn::punctuated::Punctuated::new(),
    };
    for ident in segments {
        path.segments.push(syn::PathSegment::from(ident));
    }
    Some(path)
}

impl<'ast> Visit<'ast> for Macros {
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        self.invocation(&node.path);
        self.tokens(node.tokens.clone());
        visit::visit_macro(self, node);
    }
    fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
        self.attribute(&attribute.meta);
        visit::visit_attribute(self, attribute);
    }
}

fn local_definitions(file: &syn::File) -> BTreeSet<String> {
    struct Definitions(BTreeSet<String>);
    impl<'ast> Visit<'ast> for Definitions {
        fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
            if item.mac.path.is_ident(MACRO_DEFINITION)
                && let Some(name) = &item.ident
            {
                self.0.insert(name.to_string());
            }
            visit::visit_item_macro(self, item);
        }
    }
    let mut definitions = Definitions(BTreeSet::new());
    definitions.visit_file(file);
    definitions.0
}

pub(crate) fn macro_diagnostics(file: &syn::File) -> Vec<loom_proto::Diagnostic> {
    let mut visitor = Macros {
        local: local_definitions(file),
        messages: BTreeSet::new(),
    };
    visitor.visit_file(file);
    visitor
        .messages
        .into_iter()
        .map(|message| crate::diagnostic(loom_proto::Lang::Rust, "LOOM_MACRO", &message))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(source: &str) -> Vec<String> {
        macro_diagnostics(&syn::parse_file(source).unwrap())
            .into_iter()
            .map(|error| {
                assert_eq!(error.code, "LOOM_MACRO");
                error.message
            })
            .collect()
    }

    #[test]
    fn builtin_derives_and_toolchain_macros_are_accepted() {
        for source in [
            "#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)] struct Value(u8); pub fn main() {}",
            "#[derive(core::clone::Clone, ::std::fmt::Debug, std::cmp::PartialEq)] struct Value; pub fn main() {}",
            "#[derive(Default)] enum Kind { #[default] Alpha, Beta } pub fn main() {}",
            "pub fn main() -> Vec<u8> { vec![1, 2] }",
            "pub fn main() -> Vec<u8> { std::vec![1; 4] }",
            "pub fn main() -> String { format!(\"{} and {}\", 1, 2) }",
            "pub fn main() -> String { ::alloc::format!(\"{}\", 1) }",
            "pub fn main(value: Option<u8>) -> bool { matches!(value, Some(_)) }",
            "pub fn main() { assert!(true); assert_eq!(1, 1); ::core::assert_ne!(1, 2); debug_assert!(true); }",
            "use std::fmt::Write; pub fn main() -> String { let mut out = String::new(); write!(out, \"{}\", 1).unwrap(); writeln!(out).unwrap(); out }",
            "pub fn main() -> ! { todo!() }",
            "pub fn main() -> ! { unreachable!(\"{}\", 7) }",
            "pub fn main() -> ! { panic!(\"stop\") }",
            "pub fn main() -> &'static str { concat!(\"a\", stringify!(b)) }",
            "pub fn main() -> loom::Value { loom::serde_json::json!({\"a\": [1, 2]}) }",
            "use loom::serde_json::json; pub fn main() -> loom::Value { json!({\"a\": vec![1]}) }",
            "macro_rules! twice { ($e:expr) => { $e * 2 } } pub fn main() -> i32 { twice!(21) }",
            "pub fn main() -> i32 { macro_rules! inner { () => { 7 } } inner!() }",
            "macro_rules! forward { ($(#[$attr:meta])* $name:ident) => { $(#[$attr])* struct $name; } } forward!(#[derive(Clone)] Value); pub fn main() {}",
            "macro_rules! nested { () => { vec![format!(\"{}\", 1)] } } pub fn main() -> Vec<String> { nested!() }",
            "macro_rules! negate { ($flag:expr) => { return !($flag && false) } } pub fn main() -> bool { negate!(true) }",
            "macro_rules! differs { ($a:expr, $b:expr) => { $a !=($b) } } pub fn main() -> bool { differs!(1, 2) }",
            "#[cfg(test)] mod tests { #[test] fn adds() { assert_eq!(1 + 1, 2); } } pub fn main() {}",
            "#[inline] #[must_use] #[doc = \"documented\"] pub fn main() -> u8 { 1 }",
        ] {
            assert_eq!(messages(source), Vec::<String>::new(), "{source}");
        }
    }

    #[test]
    fn rejections_name_the_item_and_print_the_table() {
        for (source, named) in [
            (
                "#[derive(Serialize)] struct Value; pub fn main() {}",
                "Derive `Serialize`",
            ),
            (
                "#[derive(serde::Serialize, Clone)] struct Value; pub fn main() {}",
                "Derive `serde::Serialize`",
            ),
            (
                "#[derive(std::fmt::Display)] struct Value; pub fn main() {}",
                "Derive `std::fmt::Display`",
            ),
            ("pub fn main() { foo!(); }", "Macro `foo!`"),
            ("pub fn main() { println!(\"hello\"); }", "Macro `println!`"),
            ("pub fn main() { dbg!(1); }", "Macro `dbg!`"),
            ("pub fn main() -> u32 { line!() }", "Macro `line!`"),
            (
                "pub fn main() -> &'static str { include_str!(\"x\") }",
                "Macro `include_str!`",
            ),
            (
                "#[tokio::main] pub async fn main() {}",
                "Attribute `#[tokio::main]`",
            ),
            (
                "#[custom::expand] pub fn main() {}",
                "Attribute `#[custom::expand]`",
            ),
            (
                "#[cfg_attr(all(), derive(Clone))] struct Value; pub fn main() {}",
                "Attribute `#[cfg_attr(",
            ),
            (
                "#[cfg(feature = \"x\")] pub fn main() {}",
                "Attribute `#[cfg(feature",
            ),
            ("#[cfg] pub fn main() {}", "Attribute `#[cfg]`"),
            (
                "#[macro_export] macro_rules! helper { () => {} } pub fn main() {}",
                "Attribute `#[macro_export]`",
            ),
            (
                "macro_rules! helper { () => {} } pub fn main() { self::helper!(); }",
                "Macro `self::helper!`",
            ),
            (
                "pub fn main() { other!(); } macro_rules! helper { () => {} }",
                "Macro `other!`",
            ),
            (
                "macro_rules! hidden { () => { println!(\"x\") } } pub fn main() { hidden!(); }",
                "Macro `println!`",
            ),
            (
                "macro_rules! hidden { () => { #[derive(Serialize)] struct S; } } hidden!(); pub fn main() {}",
                "Derive `Serialize`",
            ),
            (
                "macro_rules! hidden { () => { #[tokio::main] fn f() {} } } hidden!(); pub fn main() {}",
                "Attribute `#[tokio::main]`",
            ),
            ("pub fn main() { vec![dbg!(1)]; }", "Macro `dbg!`"),
            (
                "macro_rules! call { ($m:ident) => { $m!() } } pub fn main() { call!(vec); }",
                "Macro `m!`",
            ),
        ] {
            let messages = messages(source);
            assert!(
                messages.iter().any(|message| message.contains(named)),
                "{source}: {messages:?}"
            );
            for message in &messages {
                assert!(
                    message.contains("Available:"),
                    "diagnostic omits the table: {message}"
                );
            }
        }
    }

    #[test]
    fn tables_render_into_diagnostics() {
        let [message] = messages("pub fn main() { foo!(); }").try_into().unwrap();
        for entry in TOOLCHAIN_MACROS {
            assert!(message.contains(&format!("{}!", entry.name)), "{message}");
        }
        let [message] = messages("#[derive(Serialize)] struct V; pub fn main() {}")
            .try_into()
            .unwrap();
        for derive in BUILTIN_DERIVES {
            assert!(message.contains(derive.name), "{message}");
        }
        let [message] = messages("#[tokio::main] pub fn main() {}")
            .try_into()
            .unwrap();
        for attribute in GUEST_ATTRIBUTES {
            assert!(message.contains(attribute.name), "{message}");
        }
        assert!(message.contains("cfg(test)"), "{message}");
    }
}
