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
mod inference;
pub(crate) use inference::{aggregate, infer};
mod declarations;
pub(crate) use declarations::{actor_declaration_diagnostics, declaration_diagnostics};
use declarations::{declared_attributes, declared_labels};
mod admission;
pub(crate) use admission::{unsafe_source_diagnostics, unsupported_mode_diagnostics};
#[cfg(test)]
mod tests;
