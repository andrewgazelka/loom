use super::*;

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
