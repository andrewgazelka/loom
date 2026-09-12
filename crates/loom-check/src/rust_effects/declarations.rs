use super::*;

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
                diagnostics.push(crate::diagnostic(
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
                    diagnostics.push(crate::diagnostic(
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
                diagnostics.push(crate::diagnostic(loom_proto::Lang::Rust, "LOOM_EFFECT_ROW", &format!("{} has unknown effect dispatch; declare the residual host row with #[loom::def(effects = [\"label\"])]", function.sig.ident)));
            }
        }
    }
    diagnostics
}

pub(super) fn declared_labels(function: &syn::ItemFn) -> Option<Vec<String>> {
    declared_attributes(&function.attrs)
}

pub(super) fn declared_attributes(attributes: &[syn::Attribute]) -> Option<Vec<String>> {
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
                diagnostics.push(crate::diagnostic(
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
                    diagnostics.push(crate::diagnostic(
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
                diagnostics.push(crate::diagnostic(loom_proto::Lang::Rust, "LOOM_EFFECT_ROW", "actor has unknown effect dispatch; declare #[loom::actor(effects = [\"label\"])]"));
            }
        }
    }
    diagnostics
}
