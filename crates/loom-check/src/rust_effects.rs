//! Conservative syntax analysis: unresolved dispatch is visible, never called pure.
use loom_proto::{EffectSet, TypeSig};
use std::collections::{BTreeMap, BTreeSet};
use syn::visit::{self, Visit};

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Call {
    name: String,
    handled: BTreeSet<String>,
}

#[derive(Clone, Default, PartialEq, Eq)]
struct Summary {
    labels: BTreeSet<String>,
    unknown: bool,
    calls: BTreeSet<Call>,
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
    fn callable(&mut self, expression: &syn::Expr) {
        if let syn::Expr::Path(path) = expression {
            let name = self.path(&path.path);
            if self.functions.contains(&name) {
                self.summary.calls.insert(Call {
                    name,
                    handled: BTreeSet::new(),
                });
            } else if let Some((alias, export)) = name.split_once("::")
                && let Some(signature) = self.signatures.get(alias)
            {
                self.summary.dependency(signature, Some(export));
            } else {
                self.summary.unknown = true;
            }
        } else {
            self.visit_expr(expression);
        }
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
            self.summary.calls.insert(Call {
                name: function.clone(),
                handled: BTreeSet::new(),
            });
            return;
        }
        self.summary.unknown = true;
    }
}
impl<'ast> Visit<'ast> for Analysis<'_> {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        let name = if let syn::Expr::Path(path) = call.func.as_ref() {
            self.path(&path.path)
        } else {
            String::new()
        };
        if name == "loom::handle" && call.args.len() == 3 {
            // Labels are a runtime-enforced total-handling contract. Forwarding a
            // discharged label must fail rather than silently reaching the root.
            if let Some(labels) = literal_labels(&call.args[0]) {
                self.visit_expr(&call.args[0]);
                self.callable(&call.args[1]);
                let outer = std::mem::take(&mut self.summary);
                self.callable(&call.args[2]);
                let mut body = std::mem::take(&mut self.summary);
                body.labels.retain(|label| !labels.contains(label));
                body.calls = body
                    .calls
                    .into_iter()
                    .map(|mut call| {
                        call.handled.extend(labels.iter().cloned());
                        call
                    })
                    .collect();
                self.summary = outer;
                self.summary.merge(&body);
                self.summary.calls.extend(body.calls);
                return;
            }
            self.summary.unknown = true;
        } else if name == "loom::handle_any" && call.args.len() == 2 {
            // A wildcard handler may Forward any effect.
            self.callable(&call.args[0]);
            self.callable(&call.args[1]);
            return;
        } else if matches!(name.as_str(), "loom::scope" | "loom::spawn") && call.args.len() == 1 {
            self.callable(&call.args[0]);
            return;
        } else if self.functions.contains(&name) {
            self.summary.calls.insert(Call {
                name: name.clone(),
                handled: BTreeSet::new(),
            });
        } else if [
            "now",
            "random",
            "sleep",
            "exec",
            "llm",
            "actor.send",
            "fs.list",
            "fs.stat",
            "fs.read",
            "fs.read_optional",
            "fs.write",
            "fs.walk",
            "fs.snapshot",
            "cas.get",
            "cas.put",
        ]
        .iter()
        .any(|effect| name == format!("loom::{}", effect.replace(".", "::")))
        {
            self.summary
                .labels
                .insert(name.trim_start_matches("loom::").replace("::", "."));
        } else if matches!(name.as_str(), "loom::call" | "loom::actor::spawn") {
            self.summary
                .labels
                .insert(name.trim_start_matches("loom::").replace("::", "."));
            self.target(call.args.first());
        } else if name == "loom::perform" {
            if let Some(syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(label),
                ..
            })) = call.args.first()
            {
                let label = label.value();
                self.summary.unknown |= matches!(label.as_str(), "call" | "actor.spawn");
                self.summary.labels.insert(label);
            } else {
                self.summary.unknown = true;
            }
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

/// Literal labels are deliberately required: arbitrary expressions cannot establish
/// a statically known handler contract.
fn literal_labels(expression: &syn::Expr) -> Option<BTreeSet<String>> {
    let expression = match expression {
        syn::Expr::Reference(reference) => reference.expr.as_ref(),
        other => other,
    };
    let syn::Expr::Array(array) = expression else {
        return None;
    };
    array
        .elems
        .iter()
        .map(|element| match element {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(label),
                ..
            }) if !label.value().is_empty() && label.value() != "*" => Some(label.value()),
            _ => None,
        })
        .collect()
}

/// Declared rows constrain the effects the outer host must supply. They are
/// not rustc effect types: unknown Rust dispatch remains marked unknown.
pub(crate) fn declaration_diagnostics(
    file: &syn::File,
    inferred: &BTreeMap<String, EffectSet>,
) -> Vec<loom_proto::Diagnostic> {
    let mut diagnostics = Vec::new();
    for item in &file.items {
        let syn::Item::Fn(function) = item else {
            continue;
        };
        for attribute in &function.attrs {
            if !attribute
                .path()
                .segments
                .last()
                .is_some_and(|segment| segment.ident == "def")
            {
                continue;
            }
            let mut declared = None;
            let parsed = if matches!(attribute.meta, syn::Meta::Path(_)) {
                Ok(())
            } else {
                attribute.parse_nested_meta(|meta| {
                    if meta.path.is_ident("effects") {
                        if declared.is_some() { return Err(meta.error("duplicate effects declaration")); }
                        let value: syn::Expr = meta.value()?.parse()?;
                        declared = Some(literal_labels(&value).ok_or_else(|| meta.error("effects must be an array of nonempty literal labels, without wildcard"))?);
                    } else if meta.path.is_ident("hash") {
                        let _: syn::LitStr = meta.value()?.parse()?;
                    } else { return Err(meta.error("expected effects or hash")); }
                    Ok(())
                })
            };
            if let Err(error) = parsed {
                diagnostics.push(super::diagnostic(
                    loom_proto::Lang::Rust,
                    "LOOM_EFFECT_ROW",
                    &error.to_string(),
                ));
                continue;
            }
            let Some(row) = inferred.get(&function.sig.ident.to_string()) else {
                continue;
            };
            if let Some(declared) = declared {
                let residual: Vec<_> = row
                    .labels
                    .iter()
                    .filter(|label| !declared.contains(*label))
                    .cloned()
                    .collect();
                if !residual.is_empty() {
                    diagnostics.push(super::diagnostic(
                        loom_proto::Lang::Rust,
                        "LOOM_EFFECT_ROW",
                        &format!(
                            "{} requires residual host effects [{}] outside its declared row",
                            function.sig.ident,
                            residual.join(", ")
                        ),
                    ));
                }
            } else if row.unknown {
                diagnostics.push(super::diagnostic(loom_proto::Lang::Rust, "LOOM_EFFECT_ROW", &format!("{} has unknown effect dispatch; declare the residual host row with #[loom::def(effects = [\"label\"])]", function.sig.ident)));
            }
        }
    }
    diagnostics
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
                if let Some(called) = previous.get(&call.name) {
                    summary
                        .labels
                        .extend(called.labels.difference(&call.handled).cloned());
                    summary.unknown |= called.unknown;
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
                declared: None,
            },
        );
    }
    for item in &file.items {
        if let syn::Item::Fn(function) = item
            && let Some(row) = effects.get_mut(&function.sig.ident.to_string())
        {
            row.declared = declared_labels(function);
        }
    }
    effects
}

fn declared_labels(function: &syn::ItemFn) -> Option<Vec<String>> {
    declared_attributes(&function.attrs)
}

fn declared_attributes(attributes: &[syn::Attribute]) -> Option<Vec<String>> {
    let mut declared = None;
    for attribute in attributes {
        if !attribute
            .path()
            .segments
            .last()
            .is_some_and(|part| part.ident == "def" || part.ident == "actor")
        {
            continue;
        }
        let _ = attribute.parse_nested_meta(|meta| {
            if meta.path.is_ident("effects") {
                let expression: syn::Expr = meta.value()?.parse()?;
                declared = literal_labels(&expression).map(|labels| labels.into_iter().collect());
            } else if meta.path.is_ident("hash") {
                let _: syn::LitStr = meta.value()?.parse()?;
            }
            Ok(())
        });
    }
    declared
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
                if let Some(effects) = inferred.get(&called.name) {
                    analysis.summary.labels.extend(
                        effects
                            .labels
                            .iter()
                            .filter(|label| !called.handled.contains(*label))
                            .cloned(),
                    );
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
        declared: if exports.len() == 1 {
            exports[0].effects.declared.clone()
        } else {
            file.items.iter().find_map(|item| {
                if let syn::Item::Struct(item) = item {
                    declared_attributes(&item.attrs)
                } else {
                    None
                }
            })
        },
    }
}

/// Actors use the same root-row contract; fold remains pure at runtime even if
/// handle/init declare root effects. Guest-handled effects never reach that root.
pub(crate) fn actor_declaration_diagnostics(
    file: &syn::File,
    row: &EffectSet,
) -> Vec<loom_proto::Diagnostic> {
    let mut diagnostics = Vec::new();
    for item in &file.items {
        let syn::Item::Struct(item) = item else {
            continue;
        };
        for attribute in &item.attrs {
            if !attribute
                .path()
                .segments
                .last()
                .is_some_and(|part| part.ident == "actor")
            {
                continue;
            }
            let mut labels = None;
            let parsed = if matches!(attribute.meta, syn::Meta::Path(_)) {
                Ok(())
            } else {
                attribute.parse_nested_meta(|meta| {
                    if !meta.path.is_ident("effects") || labels.is_some() {
                        return Err(meta.error("expected one effects = [\"label\"] declaration"));
                    }
                    let expression: syn::Expr = meta.value()?.parse()?;
                    labels = Some(literal_labels(&expression).ok_or_else(|| {
                        meta.error("effects must be literal labels without wildcard")
                    })?);
                    Ok(())
                })
            };
            if let Err(error) = parsed {
                diagnostics.push(super::diagnostic(
                    loom_proto::Lang::Rust,
                    "LOOM_EFFECT_ROW",
                    &error.to_string(),
                ));
            } else if let Some(labels) = labels {
                let residual: Vec<_> = row
                    .labels
                    .iter()
                    .filter(|label| !labels.contains(*label))
                    .cloned()
                    .collect();
                if !residual.is_empty() {
                    diagnostics.push(super::diagnostic(
                        loom_proto::Lang::Rust,
                        "LOOM_EFFECT_ROW",
                        &format!(
                            "{} requires residual host effects [{}] outside its declared row",
                            item.ident,
                            residual.join(", ")
                        ),
                    ));
                }
            } else if row.unknown {
                diagnostics.push(super::diagnostic(loom_proto::Lang::Rust, "LOOM_EFFECT_ROW", "actor has unknown effect dispatch; declare #[loom::actor(effects = [\"label\"])]"));
            }
        }
    }
    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn detached_spawn_closure_effects_flow_into_caller() {
        let row = infer_source(
            "#[loom::def] fn main() { loom::spawn(|| { loom::sleep(1); loom::now(); }); }",
        );
        assert_eq!(row.labels, vec!["now", "sleep"]);
        assert!(!row.unknown);
        let alias = infer_source(
            "use loom::spawn as start; #[loom::def] fn main() { start(|| loom::sleep(1)); }",
        );
        assert_eq!(alias.labels, vec!["sleep"]);
        assert!(!alias.unknown);
    }
    fn infer_source(source: &str) -> EffectSet {
        infer(&syn::parse_file(source).unwrap(), &BTreeMap::new())
            .remove("main")
            .unwrap()
    }
    #[test]
    fn labeled_handlers_discharge_body_helpers_but_not_handler_effects() {
        let row = infer_source(
            r#"
            fn read() { loom::fs::read("local", "."); }
            fn main() {
                loom::sleep(1);
                loom::handle(["fs.read"], |op, k| { loom::now(); }, || read());
            }
        "#,
        );
        assert_eq!(row.labels, vec!["now", "sleep"]);
        assert!(!row.unknown);
        let row = infer_source(
            r#"fn main() {
            loom::handle_any(|op,k| {}, || loom::fs::read("local", "."));
        }"#,
        );
        assert_eq!(row.labels, vec!["fs.read"]);
    }

    #[test]
    fn handler_function_values_contribute_outer_effects() {
        let row = infer_source(
            r#"
            fn handler() { loom::now(); }
            fn body() { loom::sleep(1); }
            fn main() { loom::handle(["sleep"], handler, body); }
        "#,
        );
        assert_eq!(row.labels, vec!["now"]);
        assert!(!row.unknown);
        assert!(infer_source("fn main() { loom::handle_any(external::handler, || 1); }").unknown);
    }

    #[test]
    fn residual_declaration_names_unhandled_labels() {
        let bad = syn::parse_file(
            r#"#[loom::def(effects=["sleep"])] fn main() {
            loom::sleep(1); loom::fs::read("local", ".");
        }"#,
        )
        .unwrap();
        let diagnostics = declaration_diagnostics(&bad, &infer(&bad, &BTreeMap::new()));
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "LOOM_EFFECT_ROW");
        let good = syn::parse_file(
            r#"#[loom::def(effects=["sleep"])] fn main() {
            loom::sleep(1);
            loom::handle(["fs.read"], |op,k| {}, || loom::fs::read("local", "."));
        }"#,
        )
        .unwrap();
        let rows = infer(&good, &BTreeMap::new());
        assert!(declaration_diagnostics(&good, &rows).is_empty());
        assert_eq!(rows["main"].declared, Some(vec!["sleep".into()]));
        let dynamic = syn::parse_file("#[loom::def] fn main() { callback(); }").unwrap();
        assert_eq!(
            declaration_diagnostics(&dynamic, &infer(&dynamic, &BTreeMap::new())).len(),
            1
        );
        let explicit =
            syn::parse_file("#[loom::def(effects=[])] fn main() { callback(); }").unwrap();
        let rows = infer(&explicit, &BTreeMap::new());
        assert!(rows["main"].unknown);
        assert!(declaration_diagnostics(&explicit, &rows).is_empty());
    }

    #[test]
    fn nested_handler_rows_and_unknown_dispatch_stay_conservative() {
        let row = infer_source(
            r#"fn work() { loom::fs::read("local", "."); }
            fn main() {
                work();
                loom::handle(["fs.read"], |op,k| {}, || work());
            }"#,
        );
        assert_eq!(row.labels, vec!["fs.read"]);
        let row = infer_source(
            r#"fn main() {
            loom::handle(["fs.read"], |op,k| {}, || callback());
        }"#,
        );
        assert!(row.unknown);
        let row = infer_source(
            r#"fn main() {
            loom::handle(labels, |op,k| {}, || loom::fs::read("local", "."));
        }"#,
        );
        assert_eq!(row.labels, vec!["fs.read"]);
        assert!(row.unknown);
    }

    #[test]
    fn primitive_arithmetic_is_known_and_helpers_propagate() {
        assert_eq!(
            infer_source("fn main(a:i64)->i64 {a+1}"),
            EffectSet {
                labels: vec![],
                unknown: false,
                declared: None
            }
        );
        let effects =
            infer_source("use loom::now as clock; fn helper(){clock();} fn main(){helper();}");
        assert_eq!(
            effects,
            EffectSet {
                labels: vec!["now".into()],
                unknown: false,
                declared: None
            }
        );
    }
    #[test]
    fn perform_literal_labels_are_effects_and_dynamic_labels_stay_unknown() {
        let effects = infer_source(r#"fn main(){loom::perform::<u64>("exec", 0);}"#);
        assert_eq!(effects.labels, vec!["exec"]);
        assert!(!effects.unknown);
        for label in ["call", "actor.spawn"] {
            let effects = infer_source(&format!("fn main(){{loom::perform({label:?}, 0);}}"));
            assert_eq!(effects.labels, vec![label]);
            assert!(effects.unknown);
        }
        assert!(infer_source("fn main(){loom::perform(label, 0);}").unknown);
        assert!(infer_source("fn main(){callback();}").unknown);
    }
    #[test]
    fn shadowed_names_custom_traits_and_macros_are_not_claimed_pure() {
        let effects = infer_source("use loom::now as clock; fn main(clock:fn()){clock();}");
        assert!(effects.unknown);
        assert!(effects.labels.is_empty());
        assert!(infer_source("struct S; impl Drop for S {fn drop(&mut self){loom::random();}} fn main(){let _x=S;}").unknown);
        assert!(infer_source("fn main(){custom!();}").unknown);
        assert!(infer_source("fn main(){fn Ok(){loom::now();} Ok();}").unknown);
        assert!(infer_source("use external::*; fn main(){Ok();}").unknown);
    }
    #[test]
    fn known_dependency_effects_propagate_through_call_and_actor_spawn() {
        let sig:TypeSig=serde_json::from_value(serde_json::json!({"effects":{"labels":["llm"],"unknown":false},"exports":[{"name":"work","params":[],"returns":{"type":"null"},"effects":{"labels":["llm"],"unknown":false}}]})).unwrap();
        let mut signatures = BTreeMap::new();
        signatures.insert("worker".into(), sig);
        for effect in ["call", "actor.spawn"] {
            let function = effect.replace(".", "::");
            let effects = infer(
                &syn::parse_file(&format!(
                    "fn main(){{loom::{function}(worker::WORK_DEF,0);}}"
                ))
                .unwrap(),
                &signatures,
            )
            .remove("main")
            .unwrap();
            assert_eq!(
                effects,
                EffectSet {
                    labels: vec![effect.into(), "llm".into()],
                    unknown: false,
                    declared: None
                }
            );
        }
    }
    #[test]
    fn actor_declaration_tracks_root_requirements_without_claiming_fold_permission() {
        let file = syn::parse_file(
            r#"
            #[loom::actor(effects=[])] struct Counter;
            impl Counter { fn fold() {
                loom::handle(["sleep"], |op,k| {}, || loom::sleep(1));
            } }
        "#,
        )
        .unwrap();
        let inferred = infer(&file, &BTreeMap::new());
        let row = aggregate(&file, &BTreeMap::new(), &inferred, &[]);
        assert_eq!(row.declared, Some(vec![]));
        assert!(row.labels.is_empty());
        assert!(row.unknown);
        assert!(actor_declaration_diagnostics(&file, &row).is_empty());
        let file = syn::parse_file(
            r#"
            #[loom::actor(effects=[])] struct Counter;
            impl Counter { fn fold() { loom::sleep(1); } }
        "#,
        )
        .unwrap();
        let row = aggregate(
            &file,
            &BTreeMap::new(),
            &infer(&file, &BTreeMap::new()),
            &[],
        );
        assert_eq!(actor_declaration_diagnostics(&file, &row).len(), 1);
    }

    #[test]
    fn actor_summary_keeps_actor_send_without_inventing_free_exports() {
        let file =
            syn::parse_file("struct Counter; impl Counter {fn handle(){loom::actor::send(0,0);}} ")
                .unwrap();
        let inferred = infer(&file, &BTreeMap::new());
        assert!(inferred.is_empty());
        let effects = aggregate(&file, &BTreeMap::new(), &inferred, &[]);
        assert_eq!(effects.labels, vec!["actor.send"]);
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
