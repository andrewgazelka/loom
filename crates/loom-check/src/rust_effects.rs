//! Conservative syntax analysis: unresolved dispatch is visible, never called pure.
use loom_proto::{EffectSet, TypeSig};
use std::collections::{BTreeMap, BTreeSet};
use syn::visit::{self, Visit};

#[derive(Clone, Default, PartialEq, Eq)]
struct Summary {
    labels: BTreeSet<String>,
    unknown: bool,
    calls: BTreeSet<String>,
}
impl Summary {
    fn merge(&mut self, other: &Self) {
        self.labels.extend(other.labels.clone());
        self.unknown |= other.unknown;
    }
    fn dependency(&mut self, sig: &TypeSig, export: Option<&str>) {
        let selected = match export {
            Some(name) => sig.exports.iter().find(|item| {
                item.name == name || format!("{}_DEF", item.name.to_uppercase()) == name
            }),
            None => sig.exports.first(),
        };
        if let Some(export) = selected {
            self.labels.extend(export.effects.labels.clone());
            self.unknown |= export.effects.unknown | sig.effects.unknown;
            self.labels.extend(sig.effects.labels.clone());
        } else {
            self.unknown = true;
        }
    }
}
fn imports(tree: &syn::UseTree, prefix: String, aliases: &mut BTreeMap<String, String>) {
    match tree {
        syn::UseTree::Path(path) => {
            imports(&path.tree, format!("{prefix}{}::", path.ident), aliases)
        }
        syn::UseTree::Name(name) => {
            aliases.insert(name.ident.to_string(), format!("{prefix}{}", name.ident));
        }
        syn::UseTree::Rename(name) => {
            aliases.insert(name.rename.to_string(), format!("{prefix}{}", name.ident));
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                imports(item, prefix.clone(), aliases);
            }
        }
        syn::UseTree::Glob(_) => {}
    }
}
struct Analysis<'a> {
    summary: Summary,
    aliases: &'a BTreeMap<String, String>,
    functions: &'a BTreeSet<String>,
    signatures: &'a BTreeMap<String, TypeSig>,
    shadowed: BTreeSet<String>,
}
impl Analysis<'_> {
    fn path(&self, path: &syn::Path) -> String {
        let mut segments = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string());
        let first = segments.next().unwrap_or_default();
        if self.shadowed.contains(&first) {
            return String::new();
        }
        let first = self.aliases.get(&first).cloned().unwrap_or(first);
        std::iter::once(first)
            .chain(segments)
            .collect::<Vec<_>>()
            .join("::")
    }
    fn target(&mut self, target: Option<&syn::Expr>) {
        let Some(syn::Expr::Path(path)) = target else {
            self.summary.unknown = true;
            return;
        };
        let name = self.path(&path.path);
        if let Some((alias, export)) = name.split_once("::")
            && let Some(sig) = self.signatures.get(alias)
        {
            self.summary.dependency(sig, Some(export));
            return;
        }
        if let Some(function) = self
            .functions
            .iter()
            .find(|function| format!("{}_DEF", function.to_uppercase()) == name)
        {
            self.summary.calls.insert(function.clone());
            return;
        }
        self.summary.unknown = true;
    }
    fn descriptor(&mut self, expression: &syn::Expr) {
        let syn::Expr::Call(call) = expression else {
            self.summary.unknown = true;
            return;
        };
        let syn::Expr::Path(path) = call.func.as_ref() else {
            self.summary.unknown = true;
            return;
        };
        let name = self.path(&path.path);
        if name == "loom::Desc::new" {
            if let Some(syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(label),
                ..
            })) = call.args.first()
            {
                self.summary.labels.insert(label.value());
                if ["call", "fork", "spawn", "all", "race"].contains(&label.value().as_str()) {
                    self.summary.unknown = true;
                }
            } else {
                self.summary.unknown = true;
            }
        } else if name == "loom::call_desc" || name == "loom::fork_desc" {
            self.summary.labels.insert(
                if name == "loom::call_desc" {
                    "call"
                } else {
                    "fork"
                }
                .into(),
            );
            self.target(call.args.first());
        } else {
            self.summary.unknown = true;
        }
    }
}
impl<'ast> Visit<'ast> for Analysis<'_> {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        let name = if let syn::Expr::Path(path) = call.func.as_ref() {
            self.path(&path.path)
        } else {
            String::new()
        };
        if self.functions.contains(&name) {
            self.summary.calls.insert(name.clone());
        } else if let Some(operation) = name.strip_prefix("loom::abilities::") {
            let label = operation.replace("::", ".");
            if [
                "now",
                "random",
                "sleep",
                "exec",
                "llm",
                "send",
                "fs.list",
                "fs.stat",
                "fs.read",
                "fs.snapshot",
            ]
            .contains(&label.as_str())
            {
                self.summary.labels.insert(label);
            } else {
                self.summary.unknown = true;
            }
        } else if ["loom::call", "loom::fork", "loom::spawn"].contains(&name.as_str()) {
            self.summary
                .labels
                .insert(name.trim_start_matches("loom::").into());
            self.target(call.args.first());
        } else if name == "loom::join" {
            self.summary.labels.insert("join".into());
        } else if name == "loom::perform" {
            if let Some(desc) = call.args.first() {
                self.descriptor(desc);
            } else {
                self.summary.unknown = true;
            }
        } else if name == "loom::all" || name == "loom::race" {
            self.summary
                .labels
                .insert(name.trim_start_matches("loom::").into());
            self.summary.unknown = true;
        } else if ["loom::Desc::new", "loom::call_desc", "loom::fork_desc"].contains(&name.as_str())
        {
        } else if let Some((alias, export)) = name.split_once("::")
            && let Some(sig) = self.signatures.get(alias)
        {
            self.summary.dependency(sig, Some(export));
        } else if !["Ok", "Err", "Some"].contains(&name.as_str()) {
            self.summary.unknown = true;
        }
        visit::visit_expr_call(self, call);
    }
    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        self.summary.unknown = true;
        visit::visit_expr_method_call(self, node);
    }
    fn visit_macro(&mut self, _: &'ast syn::Macro) {
        self.summary.unknown = true;
    }
    fn visit_item_fn(&mut self, _: &'ast syn::ItemFn) {
        self.summary.unknown = true;
    }
    fn visit_item_use(&mut self, _: &'ast syn::ItemUse) {
        self.summary.unknown = true;
    }
    fn visit_item_impl(&mut self, _: &'ast syn::ItemImpl) {
        self.summary.unknown = true;
    }
    fn visit_item_struct(&mut self, _: &'ast syn::ItemStruct) {
        self.summary.unknown = true;
    }
    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        self.summary.unknown = true;
        visit::visit_expr_struct(self, node);
    }
}
struct Bindings {
    names: BTreeSet<String>,
}
impl<'ast> Visit<'ast> for Bindings {
    fn visit_pat_ident(&mut self, pattern: &'ast syn::PatIdent) {
        self.names.insert(pattern.ident.to_string());
        visit::visit_pat_ident(self, pattern);
    }
}
pub(crate) fn infer(
    file: &syn::File,
    signatures: &BTreeMap<String, TypeSig>,
) -> BTreeMap<String, EffectSet> {
    let mut aliases = BTreeMap::new();
    for item in &file.items {
        if let syn::Item::Use(item) = item {
            imports(&item.tree, String::new(), &mut aliases);
        }
    }
    let functions: BTreeSet<_> = file
        .items
        .iter()
        .filter_map(|item| {
            if let syn::Item::Fn(function) = item {
                Some(function.sig.ident.to_string())
            } else {
                None
            }
        })
        .collect();
    let mut summaries = BTreeMap::new();
    for item in &file.items {
        let syn::Item::Fn(function) = item else {
            continue;
        };
        let mut bindings = Bindings {
            names: BTreeSet::new(),
        };
        bindings.visit_item_fn(function);
        let mut analysis = Analysis {
            summary: Summary::default(),
            aliases: &aliases,
            functions: &functions,
            signatures,
            shadowed: bindings.names,
        };
        // Custom types can run user serialization/drop/trait code outside this AST.
        let primitive = |ty: &syn::Type| matches!(ty,syn::Type::Path(path) if path.qself.is_none() && path.path.segments.len()==1 && ["bool","u8","u16","u32","u64","u128","usize","i8","i16","i32","i64","i128","isize","f32","f64"].contains(&path.path.segments[0].ident.to_string().as_str()));
        for argument in &function.sig.inputs {
            if let syn::FnArg::Typed(argument) = argument
                && !primitive(&argument.ty)
            {
                analysis.summary.unknown = true;
            }
        }
        if let syn::ReturnType::Type(_, ty) = &function.sig.output
            && !primitive(ty)
        {
            analysis.summary.unknown = true;
        }
        fn contains_glob(tree: &syn::UseTree) -> bool {
            match tree {
                syn::UseTree::Glob(_) => true,
                syn::UseTree::Path(path) => contains_glob(&path.tree),
                syn::UseTree::Group(group) => group.items.iter().any(contains_glob),
                _ => false,
            }
        }
        if file
            .items
            .iter()
            .any(|item| matches!(item, syn::Item::Use(item) if contains_glob(&item.tree)))
        {
            analysis.summary.unknown = true;
        }
        if file.items.iter().any(|item| {
            matches!(
                item,
                syn::Item::Impl(_)
                    | syn::Item::Struct(_)
                    | syn::Item::Enum(_)
                    | syn::Item::Type(_)
                    | syn::Item::Mod(_)
                    | syn::Item::Macro(_)
                    | syn::Item::Static(_)
            )
        }) {
            analysis.summary.unknown = true;
        }
        for attribute in &function.attrs {
            if attribute
                .path()
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>()
                .join("::")
                != "loom::def"
            {
                analysis.summary.unknown = true;
            }
        }
        analysis.visit_block(&function.block);
        summaries.insert(function.sig.ident.to_string(), analysis.summary);
    }
    for _ in 0..summaries.len() {
        let previous = summaries.clone();
        for summary in summaries.values_mut() {
            for call in summary.calls.clone() {
                if let Some(called) = previous.get(&call) {
                    summary.merge(called);
                } else {
                    summary.unknown = true;
                }
            }
        }
        if summaries == previous {
            break;
        }
    }
    let mut effects = BTreeMap::new();
    for (name, summary) in summaries {
        effects.insert(
            name,
            EffectSet {
                labels: summary.labels.into_iter().collect(),
                unknown: summary.unknown,
            },
        );
    }
    effects
}

pub(crate) fn aggregate(
    file: &syn::File,
    signatures: &BTreeMap<String, TypeSig>,
    inferred: &BTreeMap<String, EffectSet>,
    exports: &[loom_proto::ExportSig],
) -> EffectSet {
    let mut summary = Summary::default();
    for export in exports {
        summary.labels.extend(export.effects.labels.clone());
        summary.unknown |= export.effects.unknown;
    }
    if exports.is_empty() {
        summary.unknown = true;
    }
    let mut aliases = BTreeMap::new();
    for item in &file.items {
        if let syn::Item::Use(item) = item {
            imports(&item.tree, String::new(), &mut aliases);
        }
    }
    let functions = inferred.keys().cloned().collect();
    for item in &file.items {
        let syn::Item::Impl(implementation) = item else {
            continue;
        };
        for item in &implementation.items {
            let syn::ImplItem::Fn(method) = item else {
                continue;
            };
            let mut bindings = Bindings {
                names: BTreeSet::new(),
            };
            bindings.visit_impl_item_fn(method);
            let mut analysis = Analysis {
                summary: Summary::default(),
                aliases: &aliases,
                functions: &functions,
                signatures,
                shadowed: bindings.names,
            };
            analysis.visit_block(&method.block);
            for called in analysis.summary.calls.clone() {
                if let Some(effects) = inferred.get(&called) {
                    analysis.summary.labels.extend(effects.labels.clone());
                    analysis.summary.unknown |= effects.unknown;
                } else {
                    analysis.summary.unknown = true;
                }
            }
            summary.merge(&analysis.summary);
            summary.unknown = true;
        }
    }
    EffectSet {
        labels: summary.labels.into_iter().collect(),
        unknown: summary.unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn infer_source(source: &str) -> EffectSet {
        infer(&syn::parse_file(source).unwrap(), &BTreeMap::new())
            .remove("main")
            .unwrap()
    }
    #[test]
    fn primitive_arithmetic_is_known_and_helpers_propagate() {
        assert_eq!(
            infer_source("fn main(a:i64)->i64 {a+1}"),
            EffectSet {
                labels: vec![],
                unknown: false
            }
        );
        let effects = infer_source(
            "use loom::abilities::now as clock; fn helper(){clock();} fn main(){helper();}",
        );
        assert_eq!(
            effects,
            EffectSet {
                labels: vec!["now".into()],
                unknown: false
            }
        );
    }
    #[test]
    fn descriptors_are_inert_until_performed_and_dynamic_stays_visible() {
        let effects = infer_source("fn main(){loom::Desc::new(\"exec\", 0);}");
        assert!(effects.labels.is_empty());
        let effects = infer_source("fn main(){loom::perform(loom::Desc::new(\"exec\", 0));}");
        assert_eq!(effects.labels, vec!["exec"]);
        assert!(!effects.unknown);
        assert!(infer_source("fn main(){loom::perform(desc);} ").unknown);
        assert!(infer_source("fn main(){callback();}").unknown);
    }
    #[test]
    fn shadowed_names_custom_traits_and_macros_are_not_claimed_pure() {
        let effects =
            infer_source("use loom::abilities::now as clock; fn main(clock:fn()){clock();}");
        assert!(effects.unknown);
        assert!(effects.labels.is_empty());
        assert!(infer_source("struct S; impl Drop for S {fn drop(&mut self){loom::abilities::random();}} fn main(){let _x=S;}").unknown);
        assert!(infer_source("fn main(){custom!();}").unknown);
        assert!(infer_source("fn main(){fn Ok(){loom::abilities::now();} Ok();}").unknown);
        assert!(infer_source("use external::*; fn main(){Ok();}").unknown);
    }
    #[test]
    fn known_dependency_effects_propagate_through_call_and_fork() {
        let sig:TypeSig=serde_json::from_value(serde_json::json!({"effects":{"labels":["llm"],"unknown":false},"exports":[{"name":"work","params":[],"returns":{"type":"null"},"effects":{"labels":["llm"],"unknown":false}}]})).unwrap();
        let mut signatures = BTreeMap::new();
        signatures.insert("worker".into(), sig);
        let effects = infer(
            &syn::parse_file("fn main(){loom::fork(worker::WORK_DEF,0);}").unwrap(),
            &signatures,
        )
        .remove("main")
        .unwrap();
        assert_eq!(
            effects,
            EffectSet {
                labels: vec!["fork".into(), "llm".into()],
                unknown: false
            }
        );
    }
    #[test]
    fn actor_summary_keeps_handler_effects_without_inventing_free_exports() {
        let file = syn::parse_file(
            "struct Counter; impl Counter {fn handle(){loom::abilities::send(0,0);}} ",
        )
        .unwrap();
        let inferred = infer(&file, &BTreeMap::new());
        assert!(inferred.is_empty());
        let effects = aggregate(&file, &BTreeMap::new(), &inferred, &[]);
        assert_eq!(effects.labels, vec!["send"]);
        assert!(effects.unknown);
    }
}

/// Friendly correctness diagnostics, complemented by compiler `forbid(unsafe_code)`.
/// These checks are not a security boundary: external macro expansions can be exempt.
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
                    if name == "allow" || name == "expect" {
                        if list.tokens.clone().into_iter().any(|token|matches!(token,proc_macro2::TokenTree::Ident(name) if name=="unsafe_code" || name=="unsafe_op_in_unsafe_fn")) {
                            checker.reject("unsafe lint override");
                        }
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
        .map(|message| super::diagnostic(loom_proto::Lang::Rust, "LOOM_UNSAFE", &message))
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
        vec![super::diagnostic(
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
