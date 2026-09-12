//! Evaluate exported guest constants with rustc rather than interpreting syntax.
use super::*;
use rustc_hir::def::DefKind;
use rustc_hir::def_id::{CRATE_DEF_ID, LocalDefId};

fn constant(tcx: TyCtxt<'_>, name: &str) -> Option<LocalDefId> {
    tcx.iter_local_def_id().find(|id| {
        matches!(tcx.def_kind(*id), DefKind::Const { .. })
            && tcx.local_parent(*id) == CRATE_DEF_ID
            && tcx.item_name(*id).as_str() == name
            && tcx.visibility(*id).is_public()
    })
}

pub(crate) fn schema(tcx: TyCtxt<'_>) -> Option<String> {
    let id = constant(tcx, "LOOM_SCHEMA")?;
    let value_ty = tcx.type_of(id).instantiate_identity().skip_norm_wip();
    if !matches!(value_ty.kind(), ty::Ref(_, inner, rustc_ast::Mutability::Not) if inner.is_str()) {
        tcx.dcx()
            .span_fatal(tcx.def_span(id), "LOOM_SCHEMA must have type &str");
    }
    let value = tcx.const_eval_poly(id.to_def_id()).unwrap_or_else(|_| {
        tcx.dcx()
            .span_fatal(tcx.def_span(id), "cannot evaluate LOOM_SCHEMA")
    });
    let bytes = value
        .try_get_slice_bytes_for_diagnostics(tcx)
        .unwrap_or_else(|| {
            tcx.dcx()
                .span_fatal(tcx.def_span(id), "cannot decode LOOM_SCHEMA")
        });
    Some(
        std::str::from_utf8(bytes)
            .expect("rustc string constant is UTF-8")
            .to_owned(),
    )
}

/// The exported schema is part of every entry's execution contract.
pub(crate) fn append_contract(tcx: TyCtxt<'_>, parts: &mut Vec<crate::encode::Part>) {
    if let Some(id) = constant(tcx, "LOOM_SCHEMA") {
        parts.push(crate::encode::Part::Bytes(
            b"loom-entry-contract:LOOM_SCHEMA".to_vec(),
        ));
        parts.push(crate::encode::Part::Reference(id.to_def_id()));
    }
}
