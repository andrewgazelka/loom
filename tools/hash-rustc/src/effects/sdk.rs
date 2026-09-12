//! Paths are owned by the SDK pinned alongside this compiler driver.
use super::*;

pub(super) fn effect(tcx: TyCtxt<'_>, id: DefId) -> Option<&'static str> {
    if tcx.crate_name(id.krate).as_str() != "loom_guest_rs" {
        return None;
    }
    let path = tcx.def_path(id).to_string_no_crate_verbose();
    Some(match path.trim_start_matches("::") {
        "perform" => "$perform",
        "handlers::handle" => "$handle",
        "handlers::handle_any" => "$handle_any",
        "handlers::handle_pinned" => "$handle_pinned",
        _ => return None,
    })
}
