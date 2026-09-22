use super::*;
mod constants;
mod primitive;
pub(super) use primitive::scan_primitive;
use rustc_middle::mir::{Body, Operand, TerminatorKind};
use rustc_span::{Span, Spanned};
fn instantiate<'tcx, T: ty::TypeFoldable<TyCtxt<'tcx>>>(
    analysis: &Analysis<'tcx>,
    instance: Instance<'tcx>,
    value: T,
) -> T {
    instance.instantiate_mir_and_normalize_erasing_regions(
        analysis.tcx,
        ty::TypingEnv::fully_monomorphized(),
        ty::EarlyBinder::bind(analysis.tcx, value),
    )
}
fn sysroot_definition(tcx: TyCtxt<'_>, id: DefId) -> bool {
    !id.is_local()
        && matches!(
            tcx.crate_name(id.krate).as_str(),
            "std"
                | "core"
                | "alloc"
                | "compiler_builtins"
                | "panic_abort"
                | "panic_unwind"
                | "unwind"
                | "std_detect"
                | "libc"
                | "hashbrown"
                | "rustc_std_workspace_core"
                | "rustc_std_workspace_alloc"
        )
}
fn trusted_runtime<'tcx>(analysis: &Analysis<'tcx>, instance: Instance<'tcx>) -> bool {
    if !sysroot_definition(analysis.tcx, instance.def_id()) {
        return false;
    }
    // Rust's runtime cannot call guest SDK effects except through user-supplied
    // types/callbacks. Keep scanning monomorphizations that carry either one.
    instance.args.iter().all(|argument| {
        argument.walk().all(|argument| {
            let ty::GenericArgKind::Type(value) = argument.kind() else {
                return true;
            };
            match *value.kind() {
                ty::Adt(def, _) => sysroot_definition(analysis.tcx, def.did()),
                ty::Closure(id, _)
                | ty::Coroutine(id, _)
                | ty::CoroutineClosure(id, _)
                | ty::FnDef(id, _) => sysroot_definition(analysis.tcx, id),
                ty::FnPtr(..) | ty::Dynamic(..) => false,
                _ => true,
            }
        })
    })
}
fn body<'tcx>(analysis: &Analysis<'tcx>, instance: Instance<'tcx>) -> Option<&'tcx Body<'tcx>> {
    if trusted_runtime(analysis, instance) {
        return None;
    }
    match instance.def {
        ty::InstanceKind::Intrinsic(_) | ty::InstanceKind::LlvmIntrinsic(_) => None,
        ty::InstanceKind::Virtual(_, _) => analysis.tcx.dcx().fatal(format!(
            "cannot infer effects for unresolved virtual call {instance}"
        )),
        // Shims have generated MIR even when the underlying trait method does not.
        ty::InstanceKind::Shim(_) => Some(analysis.tcx.instance_mir(instance.def)),
        ty::InstanceKind::Item(id)
            if matches!(analysis.tcx.def_kind(id), rustc_hir::def::DefKind::Ctor(..)) =>
        {
            None
        }
        ty::InstanceKind::Item(id) if analysis.tcx.is_mir_available(id) => {
            Some(analysis.tcx.instance_mir(instance.def))
        }
        // Foreign functions cannot invoke the SDK through a Rust body.
        ty::InstanceKind::Item(id) if analysis.tcx.is_foreign_item(id) => None,
        ty::InstanceKind::Item(_) => analysis.tcx.dcx().fatal(format!(
            "cannot infer effects: MIR is unavailable for {instance}"
        )),
    }
}
fn drop_edge<'tcx>(
    analysis: &Analysis<'tcx>,
    instance: Instance<'tcx>,
    body: &Body<'tcx>,
    kind: &TerminatorKind<'tcx>,
    node: &mut Node<'tcx>,
) {
    if let TerminatorKind::Drop { place, .. } = kind {
        let dropped = instantiate(
            analysis,
            instance,
            place.ty(&body.local_decls, analysis.tcx).ty,
        );
        node.edges.push(Edge {
            callee: Instance::resolve_drop_glue(analysis.tcx, dropped),
            handled: BTreeSet::new(),
        });
    }
}

/// Add destructors which HIR does not represent as explicit calls. Handler callback
/// bodies are separate instances, so their destructor rows retain the handler mask.
pub(super) fn scan_implicit<'tcx>(
    analysis: &Analysis<'tcx>,
    instance: Instance<'tcx>,
    node: &mut Node<'tcx>,
) {
    let Some(body) = body(analysis, instance) else {
        return;
    };
    for block in body.basic_blocks.iter() {
        drop_edge(analysis, instance, body, &block.terminator().kind, node);
    }
}
fn callback<'tcx>(
    analysis: &Analysis<'tcx>,
    instance: Instance<'tcx>,
    body: &Body<'tcx>,
    argument: &Spanned<Operand<'tcx>>,
    handled: BTreeSet<String>,
    node: &mut Node<'tcx>,
) {
    let callable = instantiate(
        analysis,
        instance,
        argument.node.ty(&body.local_decls, analysis.tcx),
    );
    let callee = match *callable.peel_refs().kind() {
        ty::Closure(id, args) => Instance::new_raw(id, args),
        ty::FnDef(id, args) => analysis
            .resolve(
                id,
                args.no_bound_vars()
                    .expect("monomorphic callback arguments"),
            )
            .unwrap_or_else(|| {
                analysis
                    .tcx
                    .dcx()
                    .span_fatal(argument.span, "cannot resolve effect callback")
            }),
        _ => analysis.tcx.dcx().span_fatal(
            argument.span,
            "cannot infer effects for unresolved handler callback",
        ),
    };
    node.edges.push(Edge { callee, handled });
}
fn call<'tcx>(
    analysis: &Analysis<'tcx>,
    instance: Instance<'tcx>,
    body: &Body<'tcx>,
    func: &Operand<'tcx>,
    args: &[Spanned<Operand<'tcx>>],
    span: Span,
    node: &mut Node<'tcx>,
) {
    let callable = instantiate(analysis, instance, func.ty(&body.local_decls, analysis.tcx));
    let ty::FnDef(id, arguments) = *callable.kind() else {
        for callee in analysis.indirect(callable, span) {
            selected_call(analysis, instance, body, callee, args, span, node);
        }
        return;
    };
    let arguments = arguments
        .no_bound_vars()
        .expect("monomorphic function arguments");
    let callee = analysis.resolve(id, arguments).unwrap_or_else(|| {
        analysis
            .tcx
            .dcx()
            .span_fatal(span, "cannot resolve effect callee")
    });
    selected_call(analysis, instance, body, callee, args, span, node);
}
fn selected_call<'tcx>(
    analysis: &Analysis<'tcx>,
    instance: Instance<'tcx>,
    body: &Body<'tcx>,
    callee: Instance<'tcx>,
    args: &[Spanned<Operand<'tcx>>],
    span: Span,
    node: &mut Node<'tcx>,
) {
    if matches!(callee.def, ty::InstanceKind::Virtual(..)) {
        analysis
            .tcx
            .dcx()
            .span_fatal(span, "cannot infer effects for unresolved virtual call");
    }
    match sdk_effect(analysis.tcx, callee.def_id()) {
        Some("$perform") => {
            node.edges.push(Edge {
                callee,
                handled: BTreeSet::new(),
            });
            if let Some(label) = args
                .first()
                .and_then(|arg| constants::static_string(analysis, instance, body, &arg.node))
            {
                node.row.labels.insert(label);
            } else {
                dynamic_label(analysis.tcx, span);
            }
        }
        Some("$isolated_call") => {
            node.edges.push(Edge {
                callee,
                handled: BTreeSet::new(),
            });
            node.row.labels.insert("call".into());
        }
        Some("$handle") if args.len() == 3 => {
            // Dynamic selection discharges no statically proven label; retain
            // the callback's complete residual row.
            let labels =
                constants::labels(analysis, instance, body, &args[0].node).unwrap_or_default();
            callback(analysis, instance, body, &args[1], BTreeSet::new(), node);
            callback(analysis, instance, body, &args[2], labels, node);
        }
        Some("$handle_any") if args.len() == 2 => {
            callback(analysis, instance, body, &args[0], BTreeSet::new(), node);
            callback(analysis, instance, body, &args[1], BTreeSet::new(), node);
        }
        Some("$handle_with" | "$handle_pinned") => {
            let hash = args
                .first()
                .and_then(|arg| constants::static_string(analysis, instance, body, &arg.node))
                .unwrap_or_else(|| {
                    analysis
                        .tcx
                        .dcx()
                        .span_fatal(span, "pinned handler requires a literal hash")
                });
            let row = analysis.handlers.get(&hash).unwrap_or_else(|| {
                analysis
                    .tcx
                    .dcx()
                    .span_fatal(span, format!("missing LOOM_HANDLER_ROWS entry for {hash}"))
            });
            node.row.merge(&row.row, &BTreeSet::new());
            let argument = args.last().unwrap_or_else(|| {
                analysis
                    .tcx
                    .dcx()
                    .span_fatal(span, "missing handler callback")
            });
            callback(
                analysis,
                instance,
                body,
                argument,
                row.handled.clone(),
                node,
            );
        }
        Some(_) => analysis.tcx.dcx().span_fatal(
            span,
            "cannot infer external handler effects without literal handler metadata",
        ),
        None => node.edges.push(Edge {
            callee,
            handled: BTreeSet::new(),
        }),
    }
}
pub(super) fn scan<'tcx>(
    analysis: &Analysis<'tcx>,
    instance: Instance<'tcx>,
    node: &mut Node<'tcx>,
) {
    let Some(body) = body(analysis, instance) else {
        return;
    };
    for block in body.basic_blocks.iter() {
        let terminator = block.terminator();
        drop_edge(analysis, instance, body, &terminator.kind, node);
        match &terminator.kind {
            TerminatorKind::Call {
                func,
                args,
                fn_span,
                ..
            }
            | TerminatorKind::TailCall {
                func,
                args,
                fn_span,
            } => {
                call(analysis, instance, body, func, args, *fn_span, node);
            }
            _ => {}
        }
    }
}
