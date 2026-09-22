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
            const AMBIENT_MACROS: [&str; 5] = [
                "include",
                "include_str",
                "include_bytes",
                "env",
                "option_env",
            ];
            const AMBIENT_MODULES: [&str; 5] = ["fs", "net", "time", "env", "process"];
            /// Macro bodies and inputs are tokens, not paths. An ambient-input
            /// macro is an identifier followed by `!`; an ambient module is the
            /// `std :: <module>` sequence. A local merely named `env` is neither.
            /// A fragment followed by `!` (`$m!`, `$($m)*!`) is refused outright:
            /// the call site supplies the macro name without a `!`, so neither
            /// side spells `include_str!` where this scan reads it.
            fn ambient_tokens(tokens: proc_macro2::TokenStream, violations: &mut Vec<String>) {
                use proc_macro2::{Delimiter, Spacing, TokenTree};
                let tokens: Vec<TokenTree> = tokens.into_iter().collect();
                // A `!` that is not the `!=` operator (a joint `!` before `=`).
                let bang = |position: usize| {
                    matches!(
                        tokens.get(position),
                        Some(TokenTree::Punct(punct))
                            if punct.as_char() == '!'
                                && !(punct.spacing() == Spacing::Joint
                                    && matches!(
                                        tokens.get(position + 1),
                                        Some(TokenTree::Punct(next)) if next.as_char() == '='
                                    ))
                    )
                };
                let repetition_operator = |position: usize| {
                    matches!(
                        tokens.get(position),
                        Some(TokenTree::Punct(punct)) if matches!(punct.as_char(), '*' | '+' | '?')
                    )
                };
                for (index, token) in tokens.iter().enumerate() {
                    match token {
                        TokenTree::Group(group) => ambient_tokens(group.stream(), violations),
                        TokenTree::Punct(dollar) if dollar.as_char() == '$' => {
                            match tokens.get(index + 1) {
                                // `$m!`
                                Some(TokenTree::Ident(name)) if bang(index + 2) => {
                                    violations.push(format!(
                                        "macro invocation through fragment `${name}`"
                                    ));
                                }
                                // `$( ... ) sep? op !`
                                Some(TokenTree::Group(group))
                                    if group.delimiter() == Delimiter::Parenthesis =>
                                {
                                    let operator = if repetition_operator(index + 2) {
                                        Some(index + 2)
                                    } else if !matches!(
                                        tokens.get(index + 2),
                                        Some(TokenTree::Group(_))
                                    ) && repetition_operator(index + 3)
                                    {
                                        Some(index + 3)
                                    } else {
                                        None
                                    };
                                    if let Some(operator) = operator
                                        && bang(operator + 1)
                                    {
                                        violations.push(
                                            "macro invocation through fragment `$(...)`".into(),
                                        );
                                    }
                                }
                                _ => {}
                            }
                        }
                        TokenTree::Ident(name) => {
                            let name = name.to_string();
                            let invoked = matches!(
                                tokens.get(index + 1),
                                Some(TokenTree::Punct(punct)) if punct.as_char() == '!'
                            );
                            if invoked && AMBIENT_MACROS.contains(&name.as_str()) {
                                violations.push("compile-time ambient input macro".into());
                            }
                            let colons = matches!(
                                (tokens.get(index + 1), tokens.get(index + 2)),
                                (Some(TokenTree::Punct(first)), Some(TokenTree::Punct(second)))
                                    if first.as_char() == ':' && second.as_char() == ':'
                            );
                            if name == "std"
                                && colons
                                && let Some(TokenTree::Ident(module)) = tokens.get(index + 3)
                                && AMBIENT_MODULES.contains(&module.to_string().as_str())
                            {
                                violations.push(format!("std::{module}"));
                            }
                        }
                        _ => {}
                    }
                }
            }
            struct IoVisitor {
                violations: Vec<String>,
            }
            impl<'ast> syn::visit::Visit<'ast> for IoVisitor {
                fn visit_macro(&mut self, mac: &'ast syn::Macro) {
                    if mac.path.segments.last().is_some_and(|segment| {
                        AMBIENT_MACROS.contains(&segment.ident.to_string().as_str())
                    }) {
                        self.violations
                            .push("compile-time ambient input macro".into());
                    }
                    ambient_tokens(mac.tokens.clone(), &mut self.violations);
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
                    if let syn::Meta::List(list) = &attribute.meta {
                        let before = self.violations.len();
                        ambient_tokens(list.tokens.clone(), &mut self.violations);
                        if self.violations.len() > before {
                            self.violations
                                .push("compile-time ambient input attribute".into());
                        }
                    }
                    syn::visit::visit_attribute(self, attribute);
                }
                fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
                    fn forbidden(prefix: &[String]) -> bool {
                        prefix
                            .last()
                            .is_some_and(|name| AMBIENT_MACROS.contains(&name.as_str()))
                            || (prefix.first().is_some_and(|name| name == "std")
                                && prefix
                                    .get(1)
                                    .is_some_and(|name| AMBIENT_MODULES.contains(&name.as_str())))
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
                        && segments
                            .get(1)
                            .is_some_and(|s| AMBIENT_MODULES.contains(&s.as_str()))
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
