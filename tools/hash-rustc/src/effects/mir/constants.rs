use super::*;

pub(super) fn static_string<'tcx>(
    analysis: &Analysis<'tcx>,
    instance: Instance<'tcx>,
    body: &Body<'tcx>,
    operand: &Operand<'tcx>,
) -> Option<String> {
    let mut operand = operand;
    let mut visited = std::collections::HashSet::new();
    loop {
        if matches!(operand, Operand::Constant(_)) {
            return string(analysis, instance, operand);
        }
        let (Operand::Copy(place) | Operand::Move(place)) = operand else {
            return None;
        };
        if !place.projection.is_empty() || !visited.insert(place.local) {
            return None;
        }
        for block in body.basic_blocks.iter() {
            if let TerminatorKind::Call { destination, .. } = &block.terminator().kind
                && destination.local == place.local
            {
                return None;
            }
        }
        let mut assignments = Vec::new();
        for statement in body.basic_blocks.iter().flat_map(|block| &block.statements) {
            let rustc_middle::mir::StatementKind::Assign(assignment) = &statement.kind else {
                continue;
            };
            if assignment.0.local == place.local {
                if assignment.0 != *place {
                    return None;
                }
                assignments.push(&assignment.1);
            }
            if let rustc_middle::mir::Rvalue::Ref(
                _,
                rustc_middle::mir::BorrowKind::Mut { .. },
                borrowed,
            ) = &assignment.1
                && borrowed.local == place.local
            {
                return None;
            }
        }
        let [rustc_middle::mir::Rvalue::Use(source, _)] = assignments.as_slice() else {
            return None;
        };
        operand = source;
    }
}

pub(super) fn string<'tcx>(
    analysis: &Analysis<'tcx>,
    instance: Instance<'tcx>,
    operand: &Operand<'tcx>,
) -> Option<String> {
    let Operand::Constant(constant) = operand else {
        return None;
    };
    let value = instantiate(analysis, instance, constant.const_);
    if !matches!(value.ty().kind(), ty::Ref(_, inner, _) if inner.is_str()) {
        return None;
    }
    let value = value
        .eval(
            analysis.tcx,
            ty::TypingEnv::fully_monomorphized(),
            constant.span,
        )
        .ok()?;
    let bytes = value.try_get_slice_bytes_for_diagnostics(analysis.tcx)?;
    std::str::from_utf8(bytes).ok().map(str::to_owned)
}

pub(in crate::effects) fn array_value<'tcx>(
    tcx: TyCtxt<'tcx>,
    value: rustc_middle::mir::ConstValue,
    value_ty: ty::Ty<'tcx>,
) -> Option<BTreeSet<String>> {
    use rustc_middle::mir::{
        ConstValue,
        interpret::{GlobalAlloc, alloc_range},
    };
    if let ty::Ref(_, target, _) = *value_ty.kind() {
        let pointer_size = tcx.data_layout.pointer_size();
        let zero = Default::default();
        let mut length = None;
        let pointee = match value {
            ConstValue::Slice { alloc_id, meta } => {
                length = Some(meta);
                ConstValue::Indirect {
                    alloc_id,
                    offset: zero,
                }
            }
            ConstValue::Scalar(pointer) => {
                let pointer = pointer.to_pointer(&tcx).into_pointer_or_addr().ok()?;
                let (provenance, offset) = pointer.prov_and_relative_offset();
                ConstValue::Indirect {
                    alloc_id: provenance.alloc_id(),
                    offset,
                }
            }
            ConstValue::Indirect { alloc_id, offset } => {
                let GlobalAlloc::Memory(allocation) = tcx.global_alloc(alloc_id) else {
                    return None;
                };
                let allocation = allocation.inner();
                let words = if matches!(target.kind(), ty::Slice(_)) {
                    2
                } else {
                    1
                };
                if offset + pointer_size * words > allocation.size() {
                    return None;
                }
                let pointer = allocation
                    .read_scalar(&tcx, alloc_range(offset, pointer_size), true)
                    .ok()?;
                let pointer = pointer.to_pointer(&tcx).into_pointer_or_addr().ok()?;
                if words == 2 {
                    length = Some(
                        allocation
                            .read_scalar(
                                &tcx,
                                alloc_range(offset + pointer_size, pointer_size),
                                false,
                            )
                            .ok()?
                            .to_target_usize(&tcx)
                            .discard_err()?,
                    );
                }
                let (provenance, offset) = pointer.prov_and_relative_offset();
                ConstValue::Indirect {
                    alloc_id: provenance.alloc_id(),
                    offset,
                }
            }
            ConstValue::ZeroSized => return None,
        };
        let target = if let ty::Slice(element) = *target.kind() {
            ty::Ty::new_array(tcx, element, length?)
        } else {
            target
        };
        return array_value(tcx, pointee, target);
    }
    let ty::Array(element, _) = *value_ty.kind() else {
        return None;
    };
    if !matches!(element.kind(), ty::Ref(_, inner, _) if inner.is_str()) {
        return None;
    }
    let fields = tcx.try_destructure_mir_constant_for_user_output(value, value_ty)?;
    fields
        .fields
        .iter()
        .map(|field| {
            let bytes = field.0.try_get_slice_bytes_for_diagnostics(tcx)?;
            std::str::from_utf8(bytes).ok().map(str::to_owned)
        })
        .collect()
}

pub(super) fn labels<'tcx>(
    analysis: &Analysis<'tcx>,
    instance: Instance<'tcx>,
    body: &Body<'tcx>,
    operand: &Operand<'tcx>,
) -> Option<BTreeSet<String>> {
    Labels {
        analysis,
        instance,
        body,
        visited: std::collections::HashSet::new(),
    }
    .operand(operand)
}

struct Labels<'a, 'tcx> {
    analysis: &'a Analysis<'tcx>,
    instance: Instance<'tcx>,
    body: &'a Body<'tcx>,
    visited: std::collections::HashSet<rustc_middle::mir::Local>,
}
impl<'tcx> Labels<'_, 'tcx> {
    fn operand(&mut self, operand: &Operand<'tcx>) -> Option<BTreeSet<String>> {
        match operand {
            Operand::Constant(constant) => {
                let value = instantiate(self.analysis, self.instance, constant.const_);
                let evaluated = value
                    .eval(
                        self.analysis.tcx,
                        ty::TypingEnv::fully_monomorphized(),
                        constant.span,
                    )
                    .ok()?;
                array_value(self.analysis.tcx, evaluated, value.ty())
            }
            Operand::Copy(place) | Operand::Move(place) => self.place(*place),
            _ => None,
        }
    }
    fn place(&mut self, place: rustc_middle::mir::Place<'tcx>) -> Option<BTreeSet<String>> {
        use rustc_middle::mir::{AggregateKind, Rvalue, StatementKind};
        if !place.projection.is_empty() || !self.visited.insert(place.local) {
            return None;
        }
        for statement in self
            .body
            .basic_blocks
            .iter()
            .flat_map(|block| &block.statements)
        {
            if let StatementKind::Assign(assignment) = &statement.kind {
                if assignment.0.local == place.local && assignment.0 != place {
                    return None;
                }
                if let Rvalue::Ref(_, rustc_middle::mir::BorrowKind::Mut { .. }, borrowed) =
                    &assignment.1
                    && borrowed.local == place.local
                {
                    return None;
                }
            }
        }
        // Accept immutable temporaries only. Multiple assignments require actual
        // data-flow reasoning; choosing an arbitrary assignment would be unsound.
        let mut assignments = self
            .body
            .basic_blocks
            .iter()
            .flat_map(|block| &block.statements)
            .filter_map(|statement| match &statement.kind {
                StatementKind::Assign(assignment) if assignment.0 == place => Some(&assignment.1),
                _ => None,
            });
        let value = assignments.next()?;
        if assignments.next().is_some() {
            return None;
        }
        match value {
            Rvalue::Use(operand, _) | Rvalue::Cast(_, operand, _) => self.operand(operand),
            Rvalue::Ref(_, _, place) => self.place(*place),
            Rvalue::Aggregate(kind, operands) if matches!(**kind, AggregateKind::Array(_)) => {
                operands
                    .iter()
                    .map(|operand| string(self.analysis, self.instance, operand))
                    .collect()
            }
            _ => None,
        }
    }
}
