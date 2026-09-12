use super::*;

fn sdk_definition(tcx: TyCtxt<'_>, id: DefId) -> bool {
    matches!(
        tcx.crate_name(id.krate).as_str(),
        "loom_guest_rs" | "loom_proto"
    )
}
fn user_arguments<'tcx>(analysis: &Analysis<'tcx>, instance: Instance<'tcx>) -> bool {
    instance.args.iter().any(|argument| {
        argument.walk().any(|argument| {
            let ty::GenericArgKind::Type(value) = argument.kind() else {
                return false;
            };
            let id = match *value.kind() {
                ty::Adt(def, _) => def.did(),
                ty::Closure(id, _)
                | ty::Coroutine(id, _)
                | ty::CoroutineClosure(id, _)
                | ty::FnDef(id, _) => id,
                ty::FnPtr(..) | ty::Dynamic(..) => return true,
                _ => return false,
            };
            !sysroot_definition(analysis.tcx, id)
                && !sdk_definition(analysis.tcx, id)
                && !matches!(
                    analysis.tcx.crate_name(id.krate).as_str(),
                    "serde" | "serde_core" | "serde_json"
                )
        })
    })
}

/// `perform` owns wire dispatch; its user serialization and deserialization
/// implementations remain ordinary reachable code and may perform effects.
pub(in super::super) fn scan_primitive<'tcx>(
    analysis: &Analysis<'tcx>,
    instance: Instance<'tcx>,
    node: &mut Node<'tcx>,
) {
    if !user_arguments(analysis, instance) {
        return;
    }
    let mut pending = vec![instance];
    let mut visited = std::collections::HashSet::new();
    while let Some(current) = pending.pop() {
        if !visited.insert(current) {
            continue;
        }
        let Some(body) = body(analysis, current) else {
            continue;
        };
        for block in body.basic_blocks.iter() {
            let terminator = block.terminator();
            let (TerminatorKind::Call { func, fn_span, .. }
            | TerminatorKind::TailCall { func, fn_span, .. }) = &terminator.kind
            else {
                let mut drops = Node::default();
                drop_edge(analysis, current, body, &terminator.kind, &mut drops);
                node.edges.extend(
                    drops
                        .edges
                        .into_iter()
                        .filter(|edge| user_arguments(analysis, edge.callee)),
                );
                continue;
            };
            let callable = instantiate(analysis, current, func.ty(&body.local_decls, analysis.tcx));
            let ty::FnDef(id, args) = *callable.kind() else {
                analysis
                    .tcx
                    .dcx()
                    .span_fatal(*fn_span, "cannot resolve SDK serialization callback");
            };
            let callee = analysis
                .resolve(id, args.no_bound_vars().expect("monomorphic SDK arguments"))
                .unwrap_or_else(|| {
                    analysis
                        .tcx
                        .dcx()
                        .span_fatal(*fn_span, "cannot resolve SDK serialization callee")
                });
            let infrastructure = sdk_definition(analysis.tcx, id)
                || sysroot_definition(analysis.tcx, id)
                || matches!(
                    analysis.tcx.crate_name(id.krate).as_str(),
                    "serde" | "serde_core" | "serde_json"
                );
            if !user_arguments(analysis, callee) && infrastructure {
                continue;
            }
            if sdk_definition(analysis.tcx, callee.def_id()) {
                pending.push(callee);
            } else {
                node.edges.push(Edge {
                    callee,
                    handled: BTreeSet::new(),
                });
            }
        }
    }
}
