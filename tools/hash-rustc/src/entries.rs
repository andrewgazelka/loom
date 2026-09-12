use rustc_hir::def::DefKind;
use rustc_hir::def_id::{CRATE_DEF_ID, LocalDefId};
use rustc_middle::ty::TyCtxt;
use rustc_span::MacroKind;
use rustc_span::hygiene::ExpnKind;

pub fn is_entry(tcx: TyCtxt<'_>, id: LocalDefId) -> bool {
    let kind = tcx.def_kind(id);
    if kind == DefKind::Fn && tcx.local_parent(id) == CRATE_DEF_ID && tcx.visibility(id).is_public()
    {
        return true;
    }
    let mut expansion = tcx.expn_that_defined(id.to_def_id());
    while expansion != rustc_span::ExpnId::root() {
        let data = expansion.expn_data();
        if let ExpnKind::Macro(MacroKind::Attr, _) = data.kind
            && let Some(macro_id) = data.macro_def_id
            && let name = tcx.item_name(macro_id)
            && matches!(
                tcx.crate_name(macro_id.krate).as_str(),
                "loom_guest_macros" | "loom"
            )
            && ((name.as_str() == "def" && kind == DefKind::Fn)
                || (name.as_str() == "actor" && kind == DefKind::Struct))
            && let Some(span) = tcx.def_ident_span(id)
            && tcx
                .sess
                .source_map()
                .span_to_snippet(span)
                .is_ok_and(|source| source.trim_start_matches("r#") == tcx.item_name(id).as_str())
        {
            // The macros retain the annotated item's tokens. Generated wrappers
            // have new identifiers, even when format_ident copies their span.
            return true;
        }
        expansion = data.parent;
    }
    false
}
