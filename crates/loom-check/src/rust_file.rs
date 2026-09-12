use super::*;

pub(super) fn check_rust_file(
    request: &DefineRequest,
    _signatures: &BTreeMap<String, TypeSig>,
    require_entry: bool,
) -> CheckedDef {
    let source = request.source.replace("\r\n", "\n");
    let mut diagnostics = Vec::new();
    let mut deps = request.deps.clone();
    let mut exports = Vec::new();
    let aggregate_effects = loom_proto::EffectSet {
        unknown: true,
        ..Default::default()
    };
    let source = match syn::parse_file(&source) {
        Ok(mut file) => {
            handler_references::lower(&mut file, &mut deps, &mut diagnostics);
            diagnostics.extend(rust_effects::unsafe_source_diagnostics(&file));
            diagnostics.extend(rust_effects::macro_diagnostics(&file));
            for item in &file.items {
                if let syn::Item::Fn(function) = item
                    && matches!(function.vis, syn::Visibility::Public(_))
                {
                    let params: Vec<ParamSig> = function
                        .sig
                        .inputs
                        .iter()
                        .filter_map(|argument| {
                            let syn::FnArg::Typed(argument) = argument else {
                                return None;
                            };
                            let name = if let syn::Pat::Ident(binding) = argument.pat.as_ref() {
                                binding.ident.to_string()
                            } else {
                                "argument".into()
                            };
                            Some(ParamSig {
                                name,
                                shape: rust_type_shape(&argument.ty),
                            })
                        })
                        .collect();
                    let returns = match &function.sig.output {
                        syn::ReturnType::Default => ValueShape::Null,
                        syn::ReturnType::Type(_, ty) => rust_type_shape(ty),
                    };
                    exports.push(ExportSig {
                        name: function.sig.ident.to_string(),
                        params,
                        returns,
                        effects: loom_proto::EffectSet {
                            labels: Vec::new(),
                            unknown: true,
                            declared: None,
                        },
                    });
                }
            }
            if require_entry && exports.is_empty() {
                diagnostics.push(diagnostic(
                    Lang::Rust,
                    "LOOM_ENTRYPOINT",
                    "A definition must export at least one crate-root pub fn; private functions and nested pub fn items are not entries.",
                ));
            }
            fn ambient_macro(tokens: proc_macro2::TokenStream) -> bool {
                tokens.into_iter().any(|token| match token {
                    proc_macro2::TokenTree::Ident(name) => [
                        "include",
                        "include_str",
                        "include_bytes",
                        "env",
                        "option_env",
                    ]
                    .contains(&name.to_string().as_str()),
                    proc_macro2::TokenTree::Group(group) => ambient_macro(group.stream()),
                    _ => false,
                })
            }
            struct IoVisitor {
                violations: Vec<String>,
            }
            impl<'ast> syn::visit::Visit<'ast> for IoVisitor {
                fn visit_macro(&mut self, mac: &'ast syn::Macro) {
                    if mac.path.segments.last().is_some_and(|segment| {
                        [
                            "include",
                            "include_str",
                            "include_bytes",
                            "env",
                            "option_env",
                        ]
                        .contains(&segment.ident.to_string().as_str())
                    }) || ambient_macro(mac.tokens.clone())
                    {
                        self.violations
                            .push("compile-time ambient input macro".into());
                    }
                    syn::visit::visit_macro(self, mac);
                }
                fn visit_attribute(&mut self, attribute: &'ast syn::Attribute) {
                    if attribute.path().is_ident("path") {
                        self.violations.push("external module path".into());
                    }
                    fn conditional_path(meta: &syn::Meta) -> bool {
                        if meta.path().is_ident("path") {
                            return true;
                        }
                        if let syn::Meta::List(list) = meta
                            && list.path.is_ident("cfg_attr")
                            && let Ok(attributes)=list.parse_args_with(syn::punctuated::Punctuated::<syn::Meta,syn::Token![,]>::parse_terminated){return attributes.iter().skip(1).any(conditional_path);}
                        false
                    }
                    if conditional_path(&attribute.meta) {
                        self.violations.push("external module path".into());
                    }
                    if let syn::Meta::List(list) = &attribute.meta
                        && ambient_macro(list.tokens.clone())
                    {
                        self.violations
                            .push("compile-time ambient input attribute".into());
                    }
                    syn::visit::visit_attribute(self, attribute);
                }
                fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
                    fn forbidden(prefix: &[String]) -> bool {
                        prefix.last().is_some_and(|name| {
                            [
                                "include",
                                "include_str",
                                "include_bytes",
                                "env",
                                "option_env",
                            ]
                            .contains(&name.as_str())
                        }) || (prefix.first().is_some_and(|name| name == "std")
                            && prefix.get(1).is_some_and(|name| {
                                ["fs", "net", "time", "env", "process"].contains(&name.as_str())
                            }))
                    }
                    fn inspect(tree: &syn::UseTree, mut prefix: Vec<String>) -> bool {
                        match tree {
                            syn::UseTree::Path(path) => {
                                prefix.push(path.ident.to_string());
                                inspect(&path.tree, prefix)
                            }
                            syn::UseTree::Group(group) => {
                                group.items.iter().any(|item| inspect(item, prefix.clone()))
                            }
                            syn::UseTree::Name(name) => {
                                prefix.push(name.ident.to_string());
                                forbidden(&prefix)
                            }
                            syn::UseTree::Rename(rename) => {
                                prefix.push(rename.ident.to_string());
                                forbidden(&prefix)
                            }
                            syn::UseTree::Glob(_) => {
                                prefix.first().is_some_and(|name| name == "std")
                            }
                        }
                    }
                    if inspect(&item.tree, Vec::new()) {
                        self.violations.push("ambient import".into());
                    }
                    syn::visit::visit_item_use(self, item);
                }
                fn visit_path(&mut self, path: &'ast syn::Path) {
                    let segments: Vec<String> = path
                        .segments
                        .iter()
                        .map(|segment| segment.ident.to_string())
                        .collect();
                    if segments.first().is_some_and(|s| s == "std")
                        && segments.get(1).is_some_and(|s| {
                            ["fs", "net", "time", "env", "process"].contains(&s.as_str())
                        })
                    {
                        self.violations.push(segments.join("::"));
                    }
                    syn::visit::visit_path(self, path);
                }
            }
            let mut visitor = IoVisitor {
                violations: Vec::new(),
            };
            syn::visit::Visit::visit_file(&mut visitor, &file);
            for path in visitor.violations {
                diagnostics.push(diagnostic(
                    Lang::Rust,
                    "LOOM_IO",
                    &format!("{path} is unavailable; use loom effects."),
                ));
            }
            prettyplease::unparse(&file)
        }
        Err(error) => {
            diagnostics.push(diagnostic(Lang::Rust, "RUST_PARSE", &error.to_string()));
            source
        }
    };
    CheckedDef {
        hash: String::new(),
        lang: Lang::Rust,
        name: request.name.clone(),
        source,
        deps,
        sig: TypeSig {
            exports,
            effects: aggregate_effects,
        },
        diagnostics,
    }
}

fn rust_type_shape(ty: &syn::Type) -> ValueShape {
    match ty {
        syn::Type::Path(path) => {
            let Some(segment) = path.path.segments.last() else {
                return ValueShape::Value;
            };
            let name = segment.ident.to_string();
            match name.as_str() {
                "u8" | "u16" | "u32" | "u64" | "usize" | "i8" | "i16" | "i32" | "i64" | "isize"
                | "f32" | "f64" => ValueShape::Number,
                "bool" => ValueShape::Boolean,
                "String" | "str" => ValueShape::String,
                "Vec" | "Ref" | "Result" => {
                    let syn::PathArguments::AngleBracketed(arguments) = &segment.arguments else {
                        return ValueShape::Value;
                    };
                    let inner = arguments
                        .args
                        .iter()
                        .find_map(|argument| {
                            if let syn::GenericArgument::Type(ty) = argument {
                                Some(rust_type_shape(ty))
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    match name.as_str() {
                        "Vec" => ValueShape::Array {
                            items: Box::new(inner),
                        },
                        "Ref" => ValueShape::Ref {
                            target: Box::new(inner),
                        },
                        _ => inner,
                    }
                }
                _ => ValueShape::Value,
            }
        }
        syn::Type::Reference(reference) => rust_type_shape(&reference.elem),
        syn::Type::Tuple(tuple) if tuple.elems.is_empty() => ValueShape::Null,
        _ => ValueShape::Value,
    }
}
