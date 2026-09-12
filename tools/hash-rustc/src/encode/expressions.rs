use rustc_hir as hir;
use rustc_hir::intravisit::{self, Visitor};

use super::Encoder;

impl<'tcx> Encoder<'tcx> {
    pub(super) fn expression(&mut self, expr: &'tcx hir::Expr<'tcx>) {
        self.tag(&expr.kind);
        self.dependent(
            expr.hir_id,
            matches!(expr.kind, hir::ExprKind::MethodCall(..)),
        );
        let binding_depth = self.bindings.len();
        let target_depth = self.targets.len();
        use hir::ExprKind::*;
        match expr.kind {
            Binary(op, ..) => self.scalar(op.node),
            AssignOp(op, ..) => self.scalar(op.node),
            Unary(op, _) => self.scalar(op),
            AddrOf(kind, mutability, _) => {
                self.scalar(kind);
                self.scalar(mutability);
            }
            Break(destination, value) => {
                self.destination(destination);
                self.scalar(value.is_some());
            }
            Continue(destination) => self.destination(destination),
            Loop(_, _, source, _) => {
                self.scalar(source);
                self.targets.push(expr.hir_id);
            }
            Block(block, _) => self.targets.push(block.hir_id),
            Closure(closure) => {
                self.scalar(closure.constness);
                self.tag(&closure.capture_clause);
                self.scalar(closure.kind);
                for capture in closure.explicit_captures {
                    self.resolution(hir::def::Res::Local(capture.var_hir_id));
                }
            }
            Match(_, arms, source) => {
                self.text(source.name());
                self.scalar(arms.len());
            }
            If(_, _, otherwise) => self.scalar(otherwise.is_some()),
            Ret(value) => self.scalar(value.is_some()),
            Array(values) | Tup(values) | Call(_, values) => self.scalar(values.len()),
            MethodCall(segment, receiver, args, _) => {
                self.scalar(args.len());
                if let Some(arguments) = segment.args {
                    self.visit_generic_args(arguments);
                }
                self.visit_expr(receiver);
                for argument in args {
                    self.visit_expr(argument);
                }
                self.end();
                return;
            }
            Struct(_, fields, tail) => {
                self.scalar(fields.len());
                self.tag(&tail);
            }
            UnsafeBinderCast(kind, _, ty) => {
                self.scalar(kind);
                self.scalar(ty.is_some());
            }
            Yield(_, source) => self.tag(&source),
            InlineAsm(_) => {}
            Err(_) => self.unsupported("error expression"),
            ConstBlock(_) | Use(..) | Lit(_) | Cast(..) | Type(..) | DropTemps(_) | Let(_)
            | Assign(..) | Field(..) | Index(..) | Path(_) | Become(_) | OffsetOf(..)
            | Repeat(..) => {}
        }
        intravisit::walk_expr(self, expr);
        // A let-chain exports its bindings to the subsequent condition/body.
        // Its surrounding if/match/block restores the lexical environment.
        if matches!(
            expr.kind,
            Closure(_) | If(..) | Match(..) | Loop(..) | Block(..)
        ) {
            self.bindings.truncate(binding_depth);
        }
        self.targets.truncate(target_depth);
        self.end();
    }

    pub(super) fn pattern(&mut self, pattern: &'tcx hir::Pat<'tcx>) {
        self.tag(&pattern.kind);
        use hir::PatKind::*;
        match pattern.kind {
            Binding(mode, id, _, subpattern) => {
                self.scalar(mode);
                if !self.bindings.contains(&id) {
                    self.bindings.push(id);
                }
                self.scalar(subpattern.is_some());
                if let Some(subpattern) = subpattern {
                    self.visit_pat(subpattern);
                }
                self.end();
                return;
            }
            Struct(_, fields, rest) => {
                self.scalar(fields.len());
                self.scalar(rest.is_some());
            }
            TupleStruct(_, patterns, rest) | Tuple(patterns, rest) => {
                self.scalar(patterns.len());
                self.scalar(rest.as_opt_usize());
            }
            Ref(_, pinned, mutability) => {
                self.scalar(pinned);
                self.scalar(mutability);
            }
            Range(start, end, limits) => {
                self.scalar(start.is_some());
                self.scalar(end.is_some());
                self.scalar(limits);
            }
            Slice(before, rest, after) => {
                self.scalar(before.len());
                self.scalar(rest.is_some());
                self.scalar(after.len());
            }
            Or(patterns) => self.scalar(patterns.len()),
            Missing | Wild | Never | Box(_) | Deref(_) | Expr(_) | Guard(..) => {}
            Err(_) => self.unsupported("error pattern"),
        }
        intravisit::walk_pat(self, pattern);
        self.end();
    }
}
