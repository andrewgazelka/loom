use super::*;
use rustc_hir::{
    self as hir,
    intravisit::{self, Visitor},
};

pub(super) fn scan<'tcx>(
    analysis: &Analysis<'tcx>,
    instance: Instance<'tcx>,
    body: &'tcx hir::Body<'tcx>,
    node: &mut Node<'tcx>,
) {
    let typeck = analysis.tcx.typeck(instance.def_id().expect_local());
    Scan {
        analysis,
        instance,
        typeck,
        node,
        bindings: HashMap::new(),
    }
    .visit_expr(body.value);
}
struct Scan<'a, 'tcx> {
    analysis: &'a Analysis<'tcx>,
    instance: Instance<'tcx>,
    typeck: &'tcx ty::TypeckResults<'tcx>,
    node: &'a mut Node<'tcx>,
    bindings: HashMap<hir::HirId, Vec<&'tcx hir::Expr<'tcx>>>,
}
fn literal(expr: &hir::Expr<'_>) -> Option<String> {
    if let hir::ExprKind::Lit(lit) = expr.kind
        && let rustc_ast::LitKind::Str(value, _) = lit.node
    {
        Some(value.to_string())
    } else {
        None
    }
}
fn labels(expr: &hir::Expr<'_>) -> Option<BTreeSet<String>> {
    let expr = if let hir::ExprKind::AddrOf(_, _, expr) = expr.kind {
        expr
    } else {
        expr
    };
    if let hir::ExprKind::Array(items) = expr.kind {
        items.iter().map(literal).collect()
    } else {
        None
    }
}
impl<'tcx> Scan<'_, 'tcx> {
    fn constant_label(&self, expr: &hir::Expr<'tcx>) -> Option<String> {
        if let Some(label) = literal(expr) {
            return Some(label);
        }
        let hir::ExprKind::Path(path) = expr.kind else {
            return None;
        };
        let hir::def::Res::Def(
            hir::def::DefKind::Const { .. } | hir::def::DefKind::AssocConst { .. },
            id,
        ) = self.typeck.qpath_res(&path, expr.hir_id)
        else {
            return None;
        };
        let args = self.instantiate(self.typeck.node_args(expr.hir_id));
        let value = self
            .analysis
            .tcx
            .const_eval_resolve(
                ty::TypingEnv::fully_monomorphized(),
                rustc_middle::mir::UnevaluatedConst::new(id, args),
                expr.span,
            )
            .ok()?;
        let bytes = value.try_get_slice_bytes_for_diagnostics(self.analysis.tcx)?;
        std::str::from_utf8(bytes).ok().map(str::to_owned)
    }

    fn instantiate<T: ty::TypeFoldable<TyCtxt<'tcx>>>(&self, value: T) -> T {
        self.instance.instantiate_mir_and_normalize_erasing_regions(
            self.analysis.tcx,
            ty::TypingEnv::fully_monomorphized(),
            ty::EarlyBinder::bind(self.analysis.tcx, value),
        )
    }
    fn callable(&mut self, expr: &'tcx hir::Expr<'tcx>, handled: BTreeSet<String>) {
        let ty = self.instantiate(self.typeck.expr_ty(expr));
        let instance = match *ty.peel_refs().kind() {
            ty::Closure(id, args) => Some(Instance::new_raw(id, args)),
            ty::FnDef(id, args) => self
                .analysis
                .resolve(id, args.no_bound_vars().expect("effect callable arguments")),
            _ => None,
        };
        if let Some(callee) = instance {
            self.node.edges.push(Edge { callee, handled });
            return;
        }
        match expr.kind {
            hir::ExprKind::Path(path) => {
                if let hir::def::Res::Local(binding) = self.typeck.qpath_res(&path, expr.hir_id)
                    && let Some(values) = self.bindings.get(&binding).cloned()
                {
                    for value in values {
                        self.callable(value, handled.clone());
                    }
                    return;
                }
            }
            hir::ExprKind::Cast(value, _) | hir::ExprKind::AddrOf(_, _, value) => {
                self.callable(value, handled);
                return;
            }
            _ => {}
        }
        if matches!(ty.peel_refs().kind(), ty::FnPtr(..)) {
            for callee in self.analysis.indirect(ty.peel_refs(), expr.span) {
                self.node.edges.push(Edge {
                    callee,
                    handled: handled.clone(),
                });
            }
            return;
        }
        self.analysis
            .tcx
            .dcx()
            .span_fatal(expr.span, "effect analysis cannot resolve this callable");
    }
    fn call(
        &mut self,
        expr: &'tcx hir::Expr<'tcx>,
        id: DefId,
        args: ty::GenericArgsRef<'tcx>,
        inputs: &[&'tcx hir::Expr<'tcx>],
    ) {
        let callee = self
            .analysis
            .resolve(id, self.instantiate(args))
            .unwrap_or_else(|| {
                self.analysis
                    .tcx
                    .dcx()
                    .span_fatal(expr.span, "effect analysis cannot resolve callee")
            });
        match sdk_effect(self.analysis.tcx, callee.def_id()) {
            Some("$perform") => {
                self.node.edges.push(Edge {
                    callee,
                    handled: BTreeSet::new(),
                });
                if let Some(label) = inputs.first().and_then(|expr| self.constant_label(expr)) {
                    self.node.row.labels.insert(label);
                } else {
                    let location = self
                        .analysis
                        .tcx
                        .sess
                        .source_map()
                        .lookup_char_pos(expr.span.source_callsite().lo());
                    self.node.row.unknown.insert(Unknown {
                        item: crate::graph::item_path(self.analysis.tcx, self.instance.def_id()),
                        span: format!(
                            "{}:{}:{}",
                            location.file.name.prefer_local_unconditionally(),
                            location.line,
                            location.col.0 + 1
                        ),
                    });
                }
            }
            Some("$handle") if inputs.len() == 3 => {
                self.callable(inputs[1], BTreeSet::new());
                self.callable(inputs[2], labels(inputs[0]).unwrap_or_default());
            }
            Some("$handle_any") if inputs.len() == 2 => {
                self.callable(inputs[0], BTreeSet::new());
                self.callable(inputs[1], BTreeSet::new());
            }
            Some("$handle_with" | "$handle_pinned") => {
                let hash = inputs
                    .first()
                    .and_then(|expr| literal(expr))
                    .unwrap_or_else(|| {
                        self.analysis
                            .tcx
                            .dcx()
                            .span_fatal(expr.span, "pinned handler requires a literal hash")
                    });
                let row = self.analysis.handlers.get(&hash).unwrap_or_else(|| {
                    self.analysis.tcx.dcx().span_fatal(
                        expr.span,
                        format!("missing LOOM_HANDLER_ROWS entry for {hash}"),
                    )
                });
                self.node.row.merge(&row.row, &BTreeSet::new());
                self.callable(inputs[inputs.len() - 1], row.handled.clone());
            }
            Some(label) if !label.starts_with('$') => {
                self.node.row.labels.insert(label.to_owned());
                self.node.edges.push(Edge {
                    callee,
                    handled: BTreeSet::new(),
                });
            }
            _ => self.node.edges.push(Edge {
                callee,
                handled: BTreeSet::new(),
            }),
        }
    }
}
impl<'tcx> Visitor<'tcx> for Scan<'_, 'tcx> {
    fn visit_local(&mut self, local: &'tcx hir::LetStmt<'tcx>) {
        if let hir::PatKind::Binding(_, binding, _, _) = local.pat.kind
            && let Some(value) = local.init
        {
            self.bindings.entry(binding).or_default().push(value);
        }
        intravisit::walk_local(self, local);
    }

    fn visit_expr(&mut self, expr: &'tcx hir::Expr<'tcx>) {
        match expr.kind {
            hir::ExprKind::Closure(_) => return, // Creation does not execute the body.
            hir::ExprKind::Call(func, inputs) => {
                if let ty::FnDef(id, args) = *self.instantiate(self.typeck.expr_ty(func)).kind() {
                    self.call(
                        expr,
                        id,
                        args.no_bound_vars().expect("effect call arguments"),
                        &inputs.iter().collect::<Vec<_>>(),
                    );
                } else {
                    let start = self.node.edges.len();
                    self.callable(func, BTreeSet::new());
                    let targets = self.node.edges.split_off(start);
                    for edge in targets {
                        if sdk_effect(self.analysis.tcx, edge.callee.def_id()).is_some() {
                            self.call(
                                expr,
                                edge.callee.def_id(),
                                edge.callee.args,
                                &inputs.iter().collect::<Vec<_>>(),
                            );
                        } else {
                            self.node.edges.push(edge);
                        }
                    }
                }
            }
            hir::ExprKind::MethodCall(_, receiver, inputs, _) => {
                if let Some(id) = self.typeck.type_dependent_def_id(expr.hir_id) {
                    let arguments = std::iter::once(receiver)
                        .chain(inputs.iter())
                        .collect::<Vec<_>>();
                    self.call(expr, id, self.typeck.node_args(expr.hir_id), &arguments);
                }
            }
            _ => {
                if let Some(id) = self.typeck.type_dependent_def_id(expr.hir_id) {
                    self.call(expr, id, self.typeck.node_args(expr.hir_id), &[]);
                }
            }
        }
        intravisit::walk_expr(self, expr);
    }
}
