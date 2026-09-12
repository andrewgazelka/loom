//! Closed-world targets for function pointers, taken from rustc's concrete MIR.
use super::*;
use rustc_middle::mir::{self, visit::Visitor};
use rustc_span::Span;
use std::collections::HashSet;

impl<'tcx> Analysis<'tcx> {
    pub(super) fn indirect(&self, callable: ty::Ty<'tcx>, span: Span) -> Vec<Instance<'tcx>> {
        if !matches!(callable.kind(), ty::FnPtr(..)) {
            self.tcx
                .dcx()
                .span_fatal(span, "cannot resolve non-function indirect effect call");
        }
        let mut scan = Targets {
            analysis: self,
            callable: signature(self.tcx, callable),
            owner: None,
            body: None,
            targets: HashSet::new(),
            pending: self
                .instances
                .iter()
                .copied()
                .chain(self.nodes.keys().copied())
                .collect(),
        };
        // Only rustc-collected monomorphizations participate. Casts retain the
        // concrete source type, including noncapturing closure coercions.
        let mut visited = HashSet::new();
        while let Some(instance) = scan.pending.pop() {
            if !visited.insert(instance) {
                continue;
            }
            match instance.def {
                ty::InstanceKind::Item(id) if !self.tcx.is_mir_available(id) => continue,
                ty::InstanceKind::Intrinsic(_)
                | ty::InstanceKind::LlvmIntrinsic(_)
                | ty::InstanceKind::Virtual(..) => continue,
                _ => {}
            }
            let body = self.tcx.instance_mir(instance.def);
            scan.owner = Some(instance);
            scan.body = Some(body);
            scan.visit_body(body);
        }
        if scan.targets.is_empty() {
            self.tcx.dcx().span_fatal(
                span,
                format!("cannot infer effects: no address-taken target for {callable}"),
            );
        }
        scan.targets.into_iter().collect()
    }
}

fn signature<'tcx>(tcx: TyCtxt<'tcx>, callable: ty::Ty<'tcx>) -> ty::FnSig<'tcx> {
    // Pointer coercions can introduce late-bound lifetimes where generic bodies
    // carried early-bound lifetimes. Lifetimes do not change executable targets.
    tcx.erase_and_anonymize_regions(tcx.instantiate_bound_regions_with_erased(callable.fn_sig(tcx)))
}

struct Targets<'a, 'tcx> {
    analysis: &'a Analysis<'tcx>,
    callable: ty::FnSig<'tcx>,
    owner: Option<Instance<'tcx>>,
    body: Option<&'tcx mir::Body<'tcx>>,
    targets: HashSet<Instance<'tcx>>,
    pending: Vec<Instance<'tcx>>,
}
impl<'tcx> Targets<'_, 'tcx> {
    fn instantiate<T: ty::TypeFoldable<TyCtxt<'tcx>>>(&self, value: T) -> T {
        self.owner
            .expect("MIR owner")
            .instantiate_mir_and_normalize_erasing_regions(
                self.analysis.tcx,
                ty::TypingEnv::fully_monomorphized(),
                ty::EarlyBinder::bind(self.analysis.tcx, value),
            )
    }
    fn matches(&self, value: ty::Ty<'tcx>) -> bool {
        matches!(value.kind(), ty::FnPtr(..))
            && signature(self.analysis.tcx, self.instantiate(value)) == self.callable
    }
}
impl<'tcx> Visitor<'tcx> for Targets<'_, 'tcx> {
    fn visit_terminator(&mut self, terminator: &mir::Terminator<'tcx>, location: mir::Location) {
        match &terminator.kind {
            mir::TerminatorKind::Call { func, .. } | mir::TerminatorKind::TailCall { func, .. } => {
                let callable = self.instantiate(
                    func.ty(&self.body.expect("MIR body").local_decls, self.analysis.tcx),
                );
                if let ty::FnDef(id, args) = *callable.kind()
                    && let Some(callee) = self.analysis.resolve(
                        id,
                        args.no_bound_vars()
                            .expect("monomorphic candidate arguments"),
                    )
                {
                    self.pending.push(callee);
                }
            }
            mir::TerminatorKind::Drop { place, .. } => {
                let dropped = self.instantiate(
                    place
                        .ty(&self.body.expect("MIR body").local_decls, self.analysis.tcx)
                        .ty,
                );
                self.pending
                    .push(Instance::resolve_drop_glue(self.analysis.tcx, dropped));
            }
            _ => {}
        }
        self.super_terminator(terminator, location);
    }

    fn visit_rvalue(&mut self, value: &mir::Rvalue<'tcx>, location: mir::Location) {
        if let mir::Rvalue::Cast(mir::CastKind::PointerCoercion(_, _), operand, target) = value
            && self.matches(*target)
        {
            let source = self.instantiate(
                operand.ty(&self.body.expect("MIR body").local_decls, self.analysis.tcx),
            );
            let callee = match *source.kind() {
                ty::FnDef(id, args) => Instance::resolve_for_fn_ptr(
                    self.analysis.tcx,
                    ty::TypingEnv::fully_monomorphized(),
                    id,
                    args.no_bound_vars()
                        .expect("monomorphic function pointer arguments"),
                ),
                ty::Closure(id, args) => Some(Instance::resolve_closure(
                    self.analysis.tcx,
                    id,
                    args,
                    ty::ClosureKind::FnOnce,
                )),
                _ => None,
            };
            if let Some(callee) = callee {
                self.targets.insert(callee);
                self.pending.push(callee);
            }
        }
        self.super_rvalue(value, location);
    }
    fn visit_const_operand(&mut self, constant: &mir::ConstOperand<'tcx>, location: mir::Location) {
        if self.matches(constant.const_.ty()) {
            let constant_value = self.instantiate(constant.const_);
            if let Ok(value) = constant_value.eval(
                self.analysis.tcx,
                ty::TypingEnv::fully_monomorphized(),
                constant.span,
            ) && let Some(pointer) = value.try_to_scalar()
                && let Ok(pointer) = pointer
                    .to_pointer(&self.analysis.tcx)
                    .into_pointer_or_addr()
            {
                let (provenance, _) = pointer.prov_and_relative_offset();
                if let mir::interpret::GlobalAlloc::Function { instance } =
                    self.analysis.tcx.global_alloc(provenance.alloc_id())
                {
                    self.targets.insert(instance);
                }
            }
        }
        self.super_const_operand(constant, location);
    }
}
