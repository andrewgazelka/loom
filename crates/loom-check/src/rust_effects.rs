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
