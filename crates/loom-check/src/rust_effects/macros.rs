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
//!
//! A `macro_rules!` transcriber can also assemble a shape out of its fragments
//! that neither the body nor the call site spells in full: `$m!(...)` names the
//! macro at the call site without a `!` there, `#[$a]` names the attribute at
//! the call site without a `#` there, and a `tt` fragment can carry a bare `#`,
//! `!` or `[...]` into any position. Every such shape is refused: the macro
//! name, the attribute and the fragment kind must be written where this pass
//! reads them, and an import may not rename an item to a name in the tables.
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
/// `roots` are the crate paths under which the same macro is also spelled; a
/// macro is admitted under its own roots only (`alloc::assert!` does not exist
/// and is refused, `alloc::vec!` and `std::vec!` are the same macro).
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
/// invocations resolve; its matchers are checked against `FRAGMENT_SPECIFIERS`
/// and its transcribers are scanned like any other macro tokens. It is an item
/// form only: a definition inside a macro body is refused, because its matchers
/// would never be read as matchers.
const MACRO_DEFINITION: &str = "macro_rules";

/// Fragment specifiers a local `macro_rules!` matcher may use. Every one binds
/// syntax that the call site spells in full (an expression, an item, a path, a
/// visibility, ...), so the call-site scan sees whatever the fragment carries.
/// `tt` is excluded on purpose: a token-tree fragment binds a bare `#`, `!` or
/// `[...]`, which the transcriber can place next to other tokens to assemble an
/// attribute or a macro invocation that neither the body nor the call site
/// shows to this pass.
const FRAGMENT_SPECIFIERS: &[&str] = &[
    "block",
    "expr",
    "expr_2021",
    "ident",
    "item",
    "lifetime",
    "literal",
    "meta",
    "pat",
    "pat_param",
    "path",
    "stmt",
    "ty",
    "vis",
];

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
/// `default` is the `derive(Default)` variant marker; it is admitted wherever
/// it is written, and rustc refuses it anywhere but on a unit variant of an
/// enum deriving `Default`. `test` is the harness attribute rustc strips
/// outside `--test`.
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

/// Every bare name the tables admit. An import that renames another item to
/// one of these would satisfy the bare-name arm of `builtin_derive`,
/// `toolchain_macro` or `guest_attribute` by spelling alone.
fn table_name(name: &str) -> bool {
    BUILTIN_DERIVES.iter().any(|derive| derive.name == name)
        || TOOLCHAIN_MACROS.iter().any(|entry| entry.name == name)
        || GUEST_ATTRIBUTES
            .iter()
            .any(|attribute| attribute.name == name)
        || name == MACRO_DEFINITION
        || name == "derive"
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

/// `$m!(...)`: the macro is named by the call site, where it appears without
/// a `!` and so meets no table.
fn fragment_macro_message(fragment: &str) -> String {
    format!(
        "Fragment `{fragment}` stands where a macro name goes (`{fragment}!`) inside a macro body, so the invoked macro is chosen by the call site and is not checked. Write the macro name in the body. Available: `macro_rules!` macros defined in this file, invoked by their bare name, and the toolchain macros {}.",
        macro_list()
    )
}

/// `#[$a]`, `#[derive($d)]`: the attribute is named by the call site, where it
/// appears without a `#` and so meets no table.
fn fragment_attribute_message(fragment: &str) -> String {
    format!(
        "Fragment `{fragment}` appears inside an attribute (`#[...]`) in a macro body, so the attribute is chosen by the call site and is not checked. Write the attribute in the body. Available: `derive(...)` with the built-in derives {} and the attributes {}.",
        derive_list(),
        attribute_list()
    )
}

/// A `#` that is not immediately followed by `[...]` is not an attribute this
/// pass can read; the bracket it pairs with after expansion would be one the
/// pass never saw as an attribute.
fn attribute_shape_message(following: &str) -> String {
    format!(
        "`#` followed by `{following}` inside a macro body or macro input is not an attribute this pass can read; `#` must be followed by `[...]`. Available: `derive(...)` with the built-in derives {} and the attributes {}.",
        derive_list(),
        attribute_list()
    )
}

fn fragment_specifier_message(name: &str, specifier: &str) -> String {
    format!(
        "Fragment `${name}:{specifier}` is not available in guest `macro_rules!`: a token-tree fragment can carry `#`, `!` or `[...]` and assemble an attribute or macro invocation that neither the call site nor the body shows. Available fragment specifiers: {}.",
        FRAGMENT_SPECIFIERS.join(", ")
    )
}

fn definition_shape_message(found: &str) -> String {
    format!(
        "`macro_rules!` body is not a sequence of `(matcher) => {{transcriber}}` arms (found `{found}`); the matchers must be readable so their fragment specifiers can be checked. Available fragment specifiers: {}.",
        FRAGMENT_SPECIFIERS.join(", ")
    )
}

fn nested_definition_message() -> String {
    format!(
        "`macro_rules!` inside a macro body is not available in guest Rust; define macros as items, where their matchers are checked. Available fragment specifiers: {}.",
        FRAGMENT_SPECIFIERS.join(", ")
    )
}

/// `use path as Name` (or the same tokens inside a macro body) where `Name` is
/// in a table: the renamed item would pass the bare-name arm by spelling.
fn alias_message(spelled: &str, alias: &str) -> String {
    format!(
        "Import `{spelled}` renames an item to `{alias}`, a name in the guest tables, so the renamed item would pass by spelling. Import it under its own name. Available: the built-in derives {}, the toolchain macros {}, and the attributes {}.",
        derive_list(),
        macro_list(),
        attribute_list()
    )
}

/// What stands before a `!` in a token stream.
enum Head {
    /// `a::b::name`, read backwards from the `!`.
    Path(syn::Path),
    /// A fragment (`$m`, `$m::name`, `$(...)*`) supplies the macro name.
    Fragment(String),
    /// A keyword, operator or nothing: a unary `!`, not an invocation.
    Other,
}

struct Macros {
    local: BTreeSet<String>,
    messages: BTreeSet<String>,
}

impl Macros {
    /// A macro invoked by path, in syntax or in tokens. `macro_rules!` never
    /// reaches here: `visit_macro` and `tokens` route it to `definition` or to
    /// the nested-definition refusal first.
    fn invocation(&mut self, path: &syn::Path) {
        let spelled = path_text(path);
        let local = path.segments.len() == 1 && self.local.contains(&spelled);
        if !local && !toolchain_macro(&spelled) {
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

    /// `use tree` renaming an item to a table name; the alias is refused
    /// whatever the item is, because the tables match spelling.
    fn use_tree(&mut self, tree: &syn::UseTree, prefix: &mut Vec<String>) {
        match tree {
            syn::UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                self.use_tree(&path.tree, prefix);
                prefix.pop();
            }
            syn::UseTree::Group(group) => {
                for item in &group.items {
                    self.use_tree(item, prefix);
                }
            }
            syn::UseTree::Rename(rename) => {
                let alias = rename.rename.to_string();
                if table_name(&alias) {
                    let mut spelled = prefix.clone();
                    spelled.push(rename.ident.to_string());
                    let spelled = format!("{} as {alias}", spelled.join("::"));
                    self.messages.insert(alias_message(&spelled, &alias));
                }
            }
            syn::UseTree::Name(_) | syn::UseTree::Glob(_) => {}
        }
    }

    /// A `macro_rules!` body: `(matcher) => {transcriber}` arms, `;`-separated.
    /// Matchers are checked for their fragment specifiers, transcribers are
    /// scanned as macro tokens.
    fn definition(&mut self, stream: proc_macro2::TokenStream) {
        use proc_macro2::TokenTree;
        let tokens: Vec<TokenTree> = stream.into_iter().collect();
        let mut index = 0;
        while index < tokens.len() {
            let arm = (
                tokens.get(index),
                tokens.get(index + 1),
                tokens.get(index + 2),
                tokens.get(index + 3),
            );
            let (
                Some(TokenTree::Group(matcher)),
                Some(TokenTree::Punct(equals)),
                Some(TokenTree::Punct(greater)),
                Some(TokenTree::Group(transcriber)),
            ) = arm
            else {
                let found: Vec<String> = tokens[index..].iter().map(ToString::to_string).collect();
                self.messages
                    .insert(definition_shape_message(&found.join(" ")));
                return;
            };
            if equals.as_char() != '=' || greater.as_char() != '>' {
                self.messages
                    .insert(definition_shape_message(&format!("{equals}{greater}")));
                return;
            }
            self.matcher(matcher.stream());
            self.tokens(transcriber.stream());
            index += 4;
            if let Some(TokenTree::Punct(separator)) = tokens.get(index)
                && separator.as_char() == ';'
            {
                index += 1;
            }
        }
    }

    /// Every `$name:specifier` in a matcher, at any depth (delimited matchers
    /// and `$(...)` repetitions are groups), must use an admitted specifier.
    fn matcher(&mut self, stream: proc_macro2::TokenStream) {
        use proc_macro2::TokenTree;
        let tokens: Vec<TokenTree> = stream.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Group(group) => self.matcher(group.stream()),
                TokenTree::Punct(punct) if punct.as_char() == '$' => {
                    if let (
                        Some(TokenTree::Ident(name)),
                        Some(TokenTree::Punct(colon)),
                        Some(TokenTree::Ident(specifier)),
                    ) = (
                        tokens.get(index + 1),
                        tokens.get(index + 2),
                        tokens.get(index + 3),
                    ) && colon.as_char() == ':'
                    {
                        let specifier = specifier.to_string();
                        if !FRAGMENT_SPECIFIERS.contains(&specifier.as_str()) {
                            self.messages
                                .insert(fragment_specifier_message(&name.to_string(), &specifier));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Macro bodies and inputs are token trees. Every `path!(...)` and `#[...]`
    /// shape inside them meets the same tables as syntax does; every shape that
    /// would become one of those only after expansion (a fragment before `!`,
    /// a fragment inside `#[...]`, a `#` not followed by `[...]`, a nested
    /// `macro_rules!`, an `as Name` alias to a table name) is refused.
    fn tokens(&mut self, stream: proc_macro2::TokenStream) {
        use proc_macro2::{Delimiter, Spacing, TokenTree};
        let tokens: Vec<TokenTree> = stream.into_iter().collect();
        for (index, token) in tokens.iter().enumerate() {
            match token {
                TokenTree::Group(group) => self.tokens(group.stream()),
                TokenTree::Ident(word) if word == "as" => {
                    if let Some(TokenTree::Ident(alias)) = tokens.get(index + 1)
                        && table_name(&alias.to_string())
                    {
                        let alias = alias.to_string();
                        self.messages
                            .insert(alias_message(&format!("as {alias}"), &alias));
                    }
                }
                TokenTree::Punct(punct) if punct.as_char() == '!' => {
                    // `!=` lexes as a joint `!` before `=`.
                    let operator = punct.spacing() == Spacing::Joint
                        && matches!(
                            tokens.get(index + 1),
                            Some(TokenTree::Punct(next)) if next.as_char() == '='
                        );
                    if operator {
                        continue;
                    }
                    let invoked = match tokens.get(index + 1) {
                        Some(TokenTree::Group(_)) => true,
                        Some(TokenTree::Punct(next)) => next.as_char() == '$',
                        _ => false,
                    };
                    match invocation_head(&tokens[..index]) {
                        Head::Fragment(fragment) => {
                            self.messages.insert(fragment_macro_message(&fragment));
                        }
                        Head::Path(path) if path.is_ident(MACRO_DEFINITION) => {
                            // `macro_rules! name { ... }`: the `!` precedes the
                            // name, not the body, so `invoked` does not apply.
                            self.messages.insert(nested_definition_message());
                        }
                        Head::Path(path) if invoked => self.invocation(&path),
                        Head::Path(_) | Head::Other => {}
                    }
                }
                TokenTree::Punct(punct) if punct.as_char() == '#' => {
                    let mut next = index + 1;
                    if let Some(TokenTree::Punct(inner)) = tokens.get(next)
                        && inner.as_char() == '!'
                    {
                        next += 1;
                    }
                    let group = match tokens.get(next) {
                        Some(TokenTree::Group(group))
                            if group.delimiter() == Delimiter::Bracket =>
                        {
                            group
                        }
                        Some(other) => {
                            self.messages
                                .insert(attribute_shape_message(&other.to_string()));
                            continue;
                        }
                        None => {
                            self.messages
                                .insert(attribute_shape_message("end of input"));
                            continue;
                        }
                    };
                    let fragments = fragments_in(group.stream());
                    if !fragments.is_empty() {
                        for fragment in fragments {
                            self.messages.insert(fragment_attribute_message(&fragment));
                        }
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

/// Every `$name` and `$(...)` fragment in a stream, at any depth, as spelled.
fn fragments_in(stream: proc_macro2::TokenStream) -> Vec<String> {
    use proc_macro2::TokenTree;
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let mut fragments = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Group(group) => fragments.extend(fragments_in(group.stream())),
            TokenTree::Punct(punct) if punct.as_char() == '$' => match tokens.get(index + 1) {
                Some(TokenTree::Ident(name)) => fragments.push(format!("${name}")),
                Some(TokenTree::Group(_)) => fragments.push("$(...)".into()),
                _ => fragments.push("$".into()),
            },
            _ => {}
        }
    }
    fragments
}

/// What stands right before a `!`, read backwards from the token stream. A
/// keyword such as `return` before a unary `!` is not a macro name; `self`,
/// `super`, `crate` and `Self` are path segments. A leading `::` is dropped,
/// as `path_text` drops it for syntax. A `$` before the first segment
/// (`$m!`, `$m::name!`, `$crate::name!`) or a repetition `$(...) sep? op`
/// before the `!` means a fragment supplies the name.
fn invocation_head(tokens: &[proc_macro2::TokenTree]) -> Head {
    use proc_macro2::{Delimiter, TokenTree};
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
    // `end` now indexes the first segment; the token before it may be a `$`.
    let dollar = |position: usize| {
        matches!(
            position.checked_sub(1).and_then(|before| tokens.get(before)),
            Some(TokenTree::Punct(punct)) if punct.as_char() == '$'
        )
    };
    if let Some(first) = segments.last()
        && dollar(end)
    {
        return Head::Fragment(format!("${first}"));
    }
    if segments.is_empty() {
        // `$( ... ) sep? op !`: a repetition transcribes the macro name.
        let repetition = |last: usize| {
            matches!(
                tokens.get(last),
                Some(TokenTree::Punct(punct)) if matches!(punct.as_char(), '*' | '+' | '?')
            )
        };
        let paren = |position: usize| {
            matches!(
                tokens.get(position),
                Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis
            ) && dollar(position)
        };
        if let Some(last) = tokens.len().checked_sub(1)
            && repetition(last)
        {
            let group = if last >= 1 && paren(last - 1) {
                Some(last - 1)
            } else if last >= 2
                && !matches!(tokens[last - 1], TokenTree::Group(_))
                && paren(last - 2)
            {
                Some(last - 2)
            } else {
                None
            };
            if let Some(group) = group {
                let separator: String = tokens[group + 1..last]
                    .iter()
                    .map(ToString::to_string)
                    .collect();
                let operator = tokens[last].to_string();
                return Head::Fragment(format!("$(...){separator}{operator}"));
            }
        }
        return Head::Other;
    }
    segments.reverse();
    let mut path = syn::Path {
        leading_colon: None,
        segments: syn::punctuated::Punctuated::new(),
    };
    for ident in segments {
        path.segments.push(syn::PathSegment::from(ident));
    }
    Head::Path(path)
}

impl<'ast> Visit<'ast> for Macros {
    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        if node.path.is_ident(MACRO_DEFINITION) {
            self.definition(node.tokens.clone());
        } else {
            self.invocation(&node.path);
            self.tokens(node.tokens.clone());
        }
        visit::visit_macro(self, node);
    }
    fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
        self.attribute(&attribute.meta);
        visit::visit_attribute(self, attribute);
    }
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.use_tree(&item.tree, &mut Vec::new());
        visit::visit_item_use(self, item);
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
            "use std::fmt::Write as FmtWrite; pub fn main() -> String { let mut out = String::new(); write!(out, \"{}\", 1).unwrap(); out }",
            "use std::fmt::Write as _; pub fn main() {}",
            "use std::collections::{BTreeMap as Map, BTreeSet}; pub fn main() -> Map<u8, BTreeSet<u8>> { Map::new() }",
            "pub fn main() -> ! { todo!() }",
            "pub fn main() -> ! { unreachable!(\"{}\", 7) }",
            "pub fn main() -> ! { panic!(\"stop\") }",
            "pub fn main() -> &'static str { concat!(\"a\", stringify!(b)) }",
            "pub fn main() -> loom::Value { loom::serde_json::json!({\"a\": [1, 2]}) }",
            "use loom::serde_json::json; pub fn main() -> loom::Value { json!({\"a\": vec![1]}) }",
            "macro_rules! twice { ($e:expr) => { $e * 2 } } pub fn main() -> i32 { twice!(21) }",
            "macro_rules! pick { ($e:expr) => ( $e ); ($e:expr, $f:expr) => [ $f ] } pub fn main() -> i32 { pick!(1, 2) }",
            "pub fn main() -> i32 { macro_rules! inner { () => { 7 } } inner!() }",
            "macro_rules! nested { () => { vec![format!(\"{}\", 1)] } } pub fn main() -> Vec<String> { nested!() }",
            "macro_rules! items { ($($e:expr),* $(,)?) => { vec![$($e),*] } } pub fn main() -> Vec<u8> { items!(1, 2,) }",
            "macro_rules! negate { ($flag:expr) => { return !($flag && false) } } pub fn main() -> bool { negate!(true) }",
            "macro_rules! differs { ($a:expr, $b:expr) => { $a !=($b) } } pub fn main() -> bool { differs!(1, 2) }",
            "macro_rules! not { ($a:expr) => { !$a } } pub fn main() -> bool { not!(false) }",
            "macro_rules! scaled { ($a:expr) => { $a * !(false) as i32 } } pub fn main() -> i32 { scaled!(2) }",
            "macro_rules! typed { ($v:ident) => { let $v: u32 = 1; } } pub fn main() -> u32 { typed!(x); x }",
            "macro_rules! define { ($name:ident) => { #[derive(Clone, Debug)] struct $name; } } define!(Value); pub fn main() {}",
            "macro_rules! call { ($f:ident) => { $f(1) } } fn double(x: u8) -> u8 { x * 2 } pub fn main() -> u8 { call!(double) }",
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
                "pub fn main() { ::alloc::assert!(true); }",
                "Macro `alloc::assert!`",
            ),
            (
                "pub fn main() -> Vec<u8> { core::vec![1] }",
                "Macro `core::vec!`",
            ),
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
        ] {
            let messages = messages(source);
            assert!(
                messages.iter().any(|message| message.contains(named)),
                "{source}: {messages:?}"
            );
        }
    }

    /// B1: a fragment in macro-name position lets the call site choose the
    /// macro without ever writing `name!`; every fragment shape is refused by
    /// the fragment's own spelling, whatever the fragment is called.
    #[test]
    fn fragments_in_invocation_position_are_refused() {
        for (source, named) in [
            (
                "macro_rules! call { ($m:ident) => { $m!(\"/etc/passwd\") } } pub fn main() -> &'static str { call!(include_str) }",
                "Fragment `$m` stands where a macro name goes (`$m!`)",
            ),
            (
                "macro_rules! call { ($vec:ident) => { $vec!(\"LOOM_TOKEN\") } } pub fn main() -> &'static str { call!(env) }",
                "Fragment `$vec` stands where a macro name goes (`$vec!`)",
            ),
            (
                "macro_rules! call { ($m:path) => { $m!() } } pub fn main() -> u32 { call!(core::line) }",
                "Fragment `$m`",
            ),
            (
                "macro_rules! call { ($m:ident) => { $m!() } } pub fn main() { call!(vec); }",
                "Fragment `$m`",
            ),
            (
                "macro_rules! call { ($root:ident) => { $root::vec!() } } pub fn main() { call!(std); }",
                "Fragment `$root`",
            ),
            (
                "macro_rules! call { () => { $crate::helper!() } } pub fn main() { call!(); }",
                "Fragment `$crate`",
            ),
            (
                "macro_rules! call { ($m:ident, $g:expr) => { $m ! $g } } pub fn main() { call!(dbg, (1)); }",
                "Fragment `$m`",
            ),
            (
                "macro_rules! call { ($($m:ident)*) => { $($m)*!(\"x\") } } pub fn main() { call!(include_str); }",
                "Fragment `$(...)*`",
            ),
            (
                "macro_rules! call { ($($m:ident),*) => { $($m),*!(\"x\") } } pub fn main() { call!(include_str); }",
                "Fragment `$(...),*`",
            ),
        ] {
            let messages = messages(source);
            assert!(
                messages.iter().any(|message| message.contains(named)),
                "{source}: {messages:?}"
            );
        }
    }

    /// B2: `#[$a]` lets a `meta` call site spell `cfg(not(test))` or
    /// `derive(loom::serde::Serialize)` with neither `#` nor `!`; the same for
    /// a `#` whose bracket arrives through a fragment.
    #[test]
    fn fragments_in_attribute_position_are_refused() {
        for (source, named) in [
            (
                "macro_rules! tag { ($a:meta, $i:item) => { #[$a] $i } } tag!(cfg(not(test)), pub fn hidden() {}); pub fn main() {}",
                "Fragment `$a` appears inside an attribute (`#[...]`)",
            ),
            (
                "macro_rules! tag { ($a:meta, $i:item) => { #![$a] $i } } tag!(no_mangle, pub fn hidden() {}); pub fn main() {}",
                "Fragment `$a` appears inside an attribute (`#[...]`)",
            ),
            (
                "macro_rules! forward { ($(#[$attr:meta])* $name:ident) => { $(#[$attr])* struct $name; } } forward!(#[derive(Clone)] Value); pub fn main() {}",
                "Fragment `$attr`",
            ),
            (
                "macro_rules! tag { ($d:path, $i:item) => { #[derive($d)] $i } } tag!(loom::serde::Serialize, struct S;); pub fn main() {}",
                "Fragment `$d`",
            ),
            (
                "macro_rules! tag { ($a:meta) => { # $a struct S; } } tag!(cfg(not(test))); pub fn main() {}",
                "`#` followed by `$`",
            ),
            (
                "macro_rules! tag { () => { # } } pub fn main() {}",
                "`#` followed by `end of input`",
            ),
            ("pub fn main() { vec![# 1]; }", "`#` followed by `1`"),
        ] {
            let messages = messages(source);
            assert!(
                messages.iter().any(|message| message.contains(named)),
                "{source}: {messages:?}"
            );
        }
    }

    /// `tt` is the fragment kind that binds a bare `#`, `!` or `[...]`; it is
    /// refused by specifier so no transcriber can reassemble those tokens.
    #[test]
    fn token_tree_fragments_and_nested_definitions_are_refused() {
        for (source, named) in [
            (
                "macro_rules! tag { ($h:tt $g:tt $i:item) => { $h $g $i } } tag!(# [cfg(test)] [cfg(not(test))] pub fn hidden() {}); pub fn main() {}",
                "Fragment `$h:tt`",
            ),
            (
                "macro_rules! call { ($bang:tt) => { include_str $bang (\"/etc/passwd\") } } pub fn main() { call!(!); }",
                "Fragment `$bang:tt`",
            ),
            (
                "macro_rules! forward { ($($rest:tt)*) => { vec![$($rest)*] } } pub fn main() { forward!(1); }",
                "Fragment `$rest:tt`",
            ),
            (
                "macro_rules! forward { ([$inner:tt]) => { $inner } } pub fn main() { forward!([1]); }",
                "Fragment `$inner:tt`",
            ),
            (
                "macro_rules! bad { ($x:frag) => { $x } } pub fn main() {}",
                "Fragment `$x:frag`",
            ),
            (
                "macro_rules! outer { () => { macro_rules! inner { ($t:tt) => { $t } } } } pub fn main() {}",
                "`macro_rules!` inside a macro body",
            ),
            (
                "macro_rules! broken { ($e:expr) { $e } } pub fn main() {}",
                "not a sequence of `(matcher) => {transcriber}` arms",
            ),
            (
                "macro_rules! broken { ($e:expr) -> { $e } } pub fn main() {}",
                "not a sequence of `(matcher) => {transcriber}` arms",
            ),
        ] {
            let messages = messages(source);
            assert!(
                messages.iter().any(|message| message.contains(named)),
                "{source}: {messages:?}"
            );
        }
    }

    /// B3: the tables match spelling, so an import that renames another item
    /// to a table name would pass; the alias is refused wherever it is written.
    #[test]
    fn aliases_to_table_names_are_refused() {
        for (source, named) in [
            (
                "use loom::serde::Serialize as Clone; #[derive(Clone)] struct S; pub fn main() {}",
                "Import `loom::serde::Serialize as Clone` renames an item to `Clone`",
            ),
            (
                "use std::println as format; pub fn main() { format!(\"x\"); }",
                "Import `std::println as format` renames an item to `format`",
            ),
            (
                "use loom::serde::{Deserialize, Serialize as Debug}; pub fn main() {}",
                "Import `loom::serde::Serialize as Debug` renames an item to `Debug`",
            ),
            (
                "mod core { pub mod clone { pub use loom::serde::Serialize as Clone; } } #[derive(core::clone::Clone)] struct S; pub fn main() {}",
                "Import `loom::serde::Serialize as Clone`",
            ),
            (
                "use some_crate::attribute as inline; #[inline] pub fn main() {}",
                "Import `some_crate::attribute as inline` renames an item to `inline`",
            ),
            (
                "use some_crate::attribute as derive; pub fn main() {}",
                "renames an item to `derive`",
            ),
            (
                "use some_crate::rules as macro_rules; pub fn main() {}",
                "renames an item to `macro_rules`",
            ),
            (
                "macro_rules! import { ($p:path) => { use $p as Clone; } } import!(loom::serde::Serialize); pub fn main() {}",
                "Import `as Clone` renames an item to `Clone`",
            ),
            (
                "pub fn main() { helper!(use std::println as format); } macro_rules! helper { ($i:item) => { $i } }",
                "Import `as format`",
            ),
        ] {
            let messages = messages(source);
            assert!(
                messages.iter().any(|message| message.contains(named)),
                "{source}: {messages:?}"
            );
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
        let [message] = messages("macro_rules! bad { ($x:tt) => { $x } } pub fn main() {}")
            .try_into()
            .unwrap();
        for specifier in FRAGMENT_SPECIFIERS {
            assert!(message.contains(specifier), "{message}");
        }
        assert!(!FRAGMENT_SPECIFIERS.contains(&"tt"));
    }

    /// Every table name is refused as an import alias: the check reads the
    /// tables, not a second list.
    #[test]
    fn every_table_name_is_a_refused_alias() {
        let names = BUILTIN_DERIVES
            .iter()
            .map(|derive| derive.name)
            .chain(TOOLCHAIN_MACROS.iter().map(|entry| entry.name))
            .chain(GUEST_ATTRIBUTES.iter().map(|attribute| attribute.name));
        for name in names {
            let source = format!("use other::item as {name}; pub fn main() {{}}");
            let messages = messages(&source);
            assert!(
                messages
                    .iter()
                    .any(|message| message.contains(&format!("renames an item to `{name}`"))),
                "{source}: {messages:?}"
            );
        }
        assert!(!table_name("FmtWrite"));
        assert!(!table_name("_"));
    }
}
