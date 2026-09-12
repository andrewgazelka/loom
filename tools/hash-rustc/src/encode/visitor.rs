use rustc_hir as hir;
use rustc_hir::intravisit::VisitorExt;
use rustc_hir::intravisit::{self, Visitor};
use rustc_middle::hir::nested_filter;
use rustc_middle::ty::TyCtxt;
use rustc_span::{Span, Symbol};

use super::Encoder;

macro_rules! framed {
    ($method:ident, $ty:ty, $walk:ident) => {
        fn $method(&mut self, value: &'tcx $ty) {
            self.text(stringify!($method));
            intravisit::$walk(self, value);
            self.end();
        }
    };
}

impl<'tcx> Visitor<'tcx> for Encoder<'tcx> {
    type NestedFilter = nested_filter::OnlyBodies;
    fn maybe_tcx(&mut self) -> TyCtxt<'tcx> {
        self.tcx
    }

    fn visit_name(&mut self, name: Symbol) {
        self.text(name.as_str());
    }
    fn visit_label(&mut self, _: &'tcx rustc_ast::Label) {}
    fn visit_expr(&mut self, expr: &'tcx hir::Expr<'tcx>) {
        self.expression(expr);
    }
    fn visit_pat(&mut self, pat: &'tcx hir::Pat<'tcx>) {
        self.pattern(pat);
    }

    fn visit_nested_body(&mut self, id: hir::BodyId) {
        let owner = self.tcx.hir_body_owner_def_id(id);
        let previous = self.typeck.replace(self.tcx.typeck(owner));
        let depth = self.bindings.len();
        self.text("body");
        intravisit::walk_body(self, self.tcx.hir_body(id));
        self.end();
        self.bindings.truncate(depth);
        self.typeck = previous;
    }

    fn visit_block(&mut self, block: &'tcx hir::Block<'tcx>) {
        self.text("block");
        self.scalar(block.rules);
        let depth = self.bindings.len();
        // Declarations have their own identity; merely placing an unused item
        // inside this block must not make it a dependency.
        for statement in block.stmts {
            if !matches!(statement.kind, hir::StmtKind::Item(_)) {
                self.visit_stmt(statement);
            }
        }
        self.scalar(block.expr.is_some());
        if let Some(expr) = block.expr {
            self.visit_expr(expr);
        }
        self.bindings.truncate(depth);
        self.end();
    }

    fn visit_stmt(&mut self, statement: &'tcx hir::Stmt<'tcx>) {
        self.tag(&statement.kind);
        intravisit::walk_stmt(self, statement);
        self.end();
    }

    fn visit_local(&mut self, local: &'tcx hir::LetStmt<'tcx>) {
        self.text("let");
        self.scalar(local.ty.is_some());
        self.scalar(local.init.is_some());
        self.scalar(local.els.is_some());
        intravisit::walk_local(self, local);
        self.end();
    }

    fn visit_arm(&mut self, arm: &'tcx hir::Arm<'tcx>) {
        let depth = self.bindings.len();
        self.text("arm");
        self.scalar(arm.guard.is_some());
        intravisit::walk_arm(self, arm);
        self.bindings.truncate(depth);
        self.end();
    }

    fn visit_lit(&mut self, _: hir::HirId, literal: hir::Lit, negated: bool) {
        self.text("literal");
        self.scalar(literal.node);
        self.scalar(negated);
    }

    fn visit_pat_expr(&mut self, expr: &'tcx hir::PatExpr<'tcx>) {
        self.tag(&expr.kind);
        intravisit::walk_pat_expr(self, expr);
        self.end();
    }

    fn visit_qpath(&mut self, path: &'tcx hir::QPath<'tcx>, id: hir::HirId, _: Span) {
        self.text("resolved-path");
        match path {
            hir::QPath::Resolved(ty, path) => {
                self.resolution(path.res);
                if let Some(ty) = ty {
                    self.visit_ty_unambig(ty);
                }
                for segment in path.segments {
                    if let Some(args) = segment.args {
                        self.visit_generic_args(args);
                    }
                }
            }
            hir::QPath::TypeRelative(ty, segment) => {
                let resolution = if let Some(results) = self
                    .typeck
                    .filter(|results| results.type_dependent_def_id(id).is_some())
                {
                    results.qpath_res(path, id)
                } else {
                    let hir::Node::Ty(node) = self.tcx.hir_node(id) else {
                        self.unsupported("type-relative non-type path outside body");
                    };
                    let lowered = rustc_hir_analysis::lower_ty(self.tcx, node);
                    let rustc_middle::ty::Alias(_, projection) = lowered.kind() else {
                        self.unsupported("type-relative path without a projection");
                    };
                    let definition = match projection.kind {
                        rustc_middle::ty::AliasTyKind::Projection { def_id }
                        | rustc_middle::ty::AliasTyKind::Inherent { def_id }
                        | rustc_middle::ty::AliasTyKind::Opaque { def_id }
                        | rustc_middle::ty::AliasTyKind::Free { def_id } => def_id,
                    };
                    hir::def::Res::Def(self.tcx.def_kind(definition), definition)
                };
                self.resolution(resolution);
                self.visit_ty_unambig(ty);
                if let Some(args) = segment.args {
                    self.visit_generic_args(args);
                }
            }
        }
        self.end();
    }

    fn visit_path(&mut self, path: &hir::Path<'tcx>, _: hir::HirId) {
        self.text("path");
        self.resolution(path.res);
        for segment in path.segments {
            if let Some(args) = segment.args {
                self.visit_generic_args(args);
            }
        }
        self.end();
    }

    fn visit_ty(&mut self, ty: &'tcx hir::Ty<'tcx, hir::AmbigArg>) {
        self.ty(ty);
    }
    fn visit_generic_param(&mut self, param: &'tcx hir::GenericParam<'tcx>) {
        self.generic(param);
    }
    fn visit_lifetime(&mut self, lifetime: &'tcx hir::Lifetime) {
        self.tag(&lifetime.kind);
        if let hir::LifetimeKind::Param(id) = lifetime.kind {
            self.reference(id.to_def_id());
        }
    }

    fn visit_const_arg(&mut self, arg: &'tcx hir::ConstArg<'tcx, hir::AmbigArg>) {
        self.tag(&arg.kind);
        if let hir::ConstArgKind::Literal { lit, negated } = arg.kind {
            self.scalar(lit);
            self.scalar(negated);
        }
        intravisit::walk_const_arg(self, arg);
        self.end();
    }

    fn visit_infer(&mut self, _: hir::HirId, _: Span, kind: intravisit::InferKind<'tcx>) {
        self.tag(&kind);
    }

    fn visit_fn_decl(&mut self, declaration: &'tcx hir::FnDecl<'tcx>) {
        self.text("signature");
        self.scalar(declaration.fn_decl_kind);
        self.scalar(declaration.inputs.len());
        self.tag(&declaration.output);
        intravisit::walk_fn_decl(self, declaration);
        self.end();
    }

    fn visit_poly_trait_ref(&mut self, reference: &'tcx hir::PolyTraitRef<'tcx>) {
        self.text("poly-trait");
        self.tag(&reference.modifiers.constness);
        self.tag(&reference.modifiers.polarity);
        intravisit::walk_poly_trait_ref(self, reference);
        self.end();
    }

    fn visit_variant_data(&mut self, data: &'tcx hir::VariantData<'tcx>) {
        self.tag(data);
        intravisit::walk_struct_def(self, data);
        self.end();
    }

    fn visit_where_predicate(&mut self, predicate: &'tcx hir::WherePredicate<'tcx>) {
        self.tag(predicate.kind);
        if let hir::WherePredicateKind::BoundPredicate(bound) = predicate.kind {
            for parameter in bound.bound_generic_params {
                let position = self.parameters.len();
                self.parameters.entry(parameter.def_id).or_insert(position);
            }
        }
        intravisit::walk_where_predicate(self, predicate);
        self.end();
    }

    fn visit_generic_args(&mut self, args: &'tcx hir::GenericArgs<'tcx>) {
        self.text("generic-arguments");
        self.scalar(args.parenthesized);
        intravisit::walk_generic_args(self, args);
        self.end();
    }

    fn visit_assoc_item_constraint(&mut self, constraint: &'tcx hir::AssocItemConstraint<'tcx>) {
        self.tag(&constraint.kind);
        intravisit::walk_assoc_item_constraint(self, constraint);
        self.end();
    }

    fn visit_pattern_type_pattern(&mut self, pattern: &'tcx hir::TyPat<'tcx>) {
        self.tag(&pattern.kind);
        intravisit::walk_ty_pat(self, pattern);
        self.end();
    }

    fn visit_inline_asm(&mut self, assembly: &'tcx hir::InlineAsm<'tcx>, _: hir::HirId) {
        self.assembly(assembly);
    }

    fn visit_precise_capturing_arg(&mut self, argument: &'tcx hir::PreciseCapturingArg<'tcx>) {
        self.tag(argument);
        match argument {
            hir::PreciseCapturingArg::Lifetime(lifetime) => self.visit_lifetime(lifetime),
            hir::PreciseCapturingArg::Param(parameter) => self.resolution(parameter.res),
        }
    }

    fn visit_field_def(&mut self, field: &'tcx hir::FieldDef<'tcx>) {
        self.text("field");
        self.scalar(field.safety);
        self.tag(&field.mut_restriction.kind);
        self.scalar(field.default.is_some());
        intravisit::walk_field_def(self, field);
        self.end();
    }

    framed!(visit_param, hir::Param<'tcx>, walk_param);
    framed!(visit_expr_field, hir::ExprField<'tcx>, walk_expr_field);
    framed!(visit_pat_field, hir::PatField<'tcx>, walk_pat_field);
    framed!(visit_generics, hir::Generics<'tcx>, walk_generics);
    framed!(visit_generic_arg, hir::GenericArg<'tcx>, walk_generic_arg);
    framed!(visit_param_bound, hir::GenericBound<'tcx>, walk_param_bound);
    framed!(visit_variant, hir::Variant<'tcx>, walk_variant);
    framed!(visit_opaque_ty, hir::OpaqueTy<'tcx>, walk_opaque_ty);
}
