use rustc_hir::def::DefKind;
use rustc_hir::def_id::{CRATE_DEF_ID, LocalDefId};
use rustc_middle::ty::TyCtxt;

/// Guest exports are ordinary public free functions at the crate root.
pub fn is_entry(tcx: TyCtxt<'_>, id: LocalDefId) -> bool {
    tcx.def_kind(id) == DefKind::Fn
        && tcx.local_parent(id) == CRATE_DEF_ID
        && tcx.visibility(id).is_public()
}
