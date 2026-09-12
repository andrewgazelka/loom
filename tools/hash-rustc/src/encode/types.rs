use rustc_hir as hir;
use rustc_hir::intravisit;
use rustc_hir::intravisit::VisitorExt;

use super::Encoder;

impl<'tcx> Encoder<'tcx> {
    pub(super) fn ty(&mut self, ty: &'tcx hir::Ty<'tcx, hir::AmbigArg>) {
        self.tag(&ty.kind);
        use hir::TyKind::*;
        match ty.kind {
            Ptr(ty) | Ref(_, ty) => self.scalar(ty.mutbl),
            FnPtr(signature) => {
                self.scalar(signature.safety);
                self.scalar(signature.abi);
            }
            Tup(types) => self.scalar(types.len()),
            InferDelegation(_) => self.unsupported("delegated type inference"),
            FieldOf(_, fields) => self.scalar(fields.variant.is_some()),
            View(_, fields) => self.scalar(fields.len()),
            Err(_) => self.unsupported("error type"),
            Slice(_) | Array(..) | UnsafeBinder(_) | Never | Path(_) | OpaqueDef(_)
            | TraitAscription(_) | TraitObject(..) | Pat(..) | Infer(_) => {}
        }
        intravisit::walk_ty(self, ty);
        self.end();
    }

    pub(super) fn generic(&mut self, param: &'tcx hir::GenericParam<'tcx>) {
        let next = self.parameters.len();
        self.parameters.entry(param.def_id).or_insert(next);
        self.tag(&param.kind);
        self.scalar(param.pure_wrt_drop);
        match param.kind {
            hir::GenericParamKind::Lifetime { .. } => {}
            hir::GenericParamKind::Type { default, .. } => {
                self.scalar(default.is_some());
                if let Some(default) = default {
                    self.visit_ty_unambig(default);
                }
            }
            hir::GenericParamKind::Const { ty, default } => {
                self.visit_ty_unambig(ty);
                self.scalar(default.is_some());
                if let Some(default) = default {
                    self.visit_const_arg_unambig(default);
                }
            }
        }
        self.end();
    }
}
