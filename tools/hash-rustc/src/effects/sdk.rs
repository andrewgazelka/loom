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
        "call" => "call",
        "now" => "now",
        "random" => "random",
        "sleep" => "sleep",
        "exec" => "exec",
        "llm" => "llm",
        "fs::list" => "fs.list",
        "fs::stat" => "fs.stat",
        "fs::walk" => "fs.walk",
        "fs::snapshot" => "fs.snapshot",
        "fs::read" => "fs.read",
        "fs::read_optional" => "fs.read_optional",
        "fs::write" => "fs.write",
        "cas::get" => "cas.get",
        "cas::put" => "cas.put",
        "actor::send" => "actor.send",
        "actor::accept" => "actor.accept",
        "actor::spawn" => "actor.spawn",
        _ => return None,
    })
}
