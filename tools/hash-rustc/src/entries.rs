use rustc_hir::def::DefKind;
use rustc_hir::def_id::{CRATE_DEF_ID, LocalDefId};
use rustc_middle::ty::TyCtxt;

/// Guest exports are ordinary public free functions at the crate root.
pub fn is_entry(tcx: TyCtxt<'_>, id: LocalDefId) -> bool {
    tcx.def_kind(id) == DefKind::Fn
        && tcx.local_parent(id) == CRATE_DEF_ID
        && tcx.visibility(id).is_public()
}

/// Items the host generated: expansions of the `loom-guest-rs` entry macros it
/// appends after the checked guest source (`loom-build` `entry_abi`). They
/// enter neither the item document nor the effect roots, so a definition's
/// identity is the guest's own items only, exactly as when the wrappers were a
/// second compile. Nested items (the wrapper's closure) count through their
/// owner.
pub fn is_generated(tcx: TyCtxt<'_>, id: LocalDefId) -> bool {
    let mut id = id;
    loop {
        let expansion = tcx.def_span(id).ctxt().outer_expn_data();
        // Another crate's macro: a guest's own `macro_rules!` never counts, whatever
        // its name and whatever the guest calls its crate.
        if let Some(macro_def) = expansion.macro_def_id
            && macro_def.krate != rustc_hir::def_id::LOCAL_CRATE
            && tcx.crate_name(macro_def.krate).as_str() == "loom_guest_rs"
            && tcx.item_name(macro_def).as_str().starts_with("__loom_export_")
        {
            return true;
        }
        match tcx.opt_local_parent(id) {
            Some(parent) if parent != CRATE_DEF_ID => id = parent,
            _ => return false,
        }
    }
}
