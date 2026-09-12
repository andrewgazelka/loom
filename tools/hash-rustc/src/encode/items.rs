use rustc_hir as hir;
use rustc_hir::def::DefKind;
use rustc_hir::def_id::LocalDefId;
use rustc_hir::intravisit::Visitor;
use rustc_hir::intravisit::VisitorExt;

use super::Encoder;

impl<'tcx> Encoder<'tcx> {
    pub(super) fn item(&mut self, id: LocalDefId) {
        let parent = self.tcx.local_parent(id);
        if matches!(self.tcx.def_kind(parent), DefKind::Impl { .. }) {
            let hir::Node::Item(parent) = self.tcx.hir_node_by_def_id(parent) else {
                unreachable!()
            };
            let hir::ItemKind::Impl(implementation) = parent.kind else {
                unreachable!()
            };
            self.visit_generics(implementation.generics);
            self.visit_ty_unambig(implementation.self_ty);
        } else if self.tcx.def_kind(parent) == DefKind::Trait {
            let hir::Node::Item(parent) = self.tcx.hir_node_by_def_id(parent) else {
                unreachable!()
            };
            let hir::ItemKind::Trait { generics, .. } = parent.kind else {
                unreachable!()
            };
            self.visit_generics(generics);
        }
        match self.tcx.hir_node_by_def_id(id) {
            hir::Node::Item(item) => self.free_item(item),
            hir::Node::ForeignItem(item) => {
                self.text("foreign");
                match item.kind {
                    hir::ForeignItemKind::Fn(sig, _, generics) => {
                        self.visit_generics(generics);
                        self.header(sig.header);
                        self.visit_fn_decl(sig.decl);
                    }
                    hir::ForeignItemKind::Static(ty, mutability, safety) => {
                        self.scalar(mutability);
                        self.scalar(safety);
                        self.visit_ty_unambig(ty);
                    }
                    hir::ForeignItemKind::Type => self.text(item.ident.name.as_str()),
                }
            }
            hir::Node::ImplItem(item) => {
                self.visit_generics(item.generics);
                match item.kind {
                    hir::ImplItemKind::Fn(sig, body) => self.function(sig, body),
                    hir::ImplItemKind::Const(ty, rhs) => {
                        self.text("const");
                        self.visit_ty_unambig(ty);
                        self.visit_const_item_rhs(rhs);
                    }
                    hir::ImplItemKind::Type(ty) => {
                        self.text("type");
                        self.visit_ty_unambig(ty);
                    }
                }
            }
            hir::Node::TraitItem(item) => {
                self.visit_generics(item.generics);
                match item.kind {
                    hir::TraitItemKind::Fn(sig, body) => {
                        self.text("trait-function");
                        self.header(sig.header);
                        self.visit_fn_decl(sig.decl);
                        self.tag(&body);
                        if let hir::TraitFn::Provided(body) = body {
                            self.visit_nested_body(body);
                        }
                    }
                    hir::TraitItemKind::Const(ty, rhs) => {
                        self.text("trait-const");
                        self.visit_ty_unambig(ty);
                        self.scalar(rhs.is_some());
                        if let Some(rhs) = rhs {
                            self.visit_const_item_rhs(rhs);
                        }
                    }
                    hir::TraitItemKind::Type(bounds, default) => {
                        self.text("trait-type");
                        for bound in bounds {
                            self.visit_param_bound(bound);
                        }
                        self.scalar(default.is_some());
                        if let Some(ty) = default {
                            self.visit_ty_unambig(ty);
                        }
                    }
                }
            }
            _ => self.unsupported("item kind"),
        }
        self.end();
    }

    fn function(&mut self, sig: hir::FnSig<'tcx>, body: hir::BodyId) {
        self.text("function");
        self.header(sig.header);
        self.visit_fn_decl(sig.decl);
        self.visit_nested_body(body);
    }

    fn header(&mut self, header: hir::FnHeader) {
        self.scalar(header.safety);
        self.scalar(header.constness);
        self.tag(&header.asyncness);
        self.scalar(header.abi);
    }

    fn free_item(&mut self, item: &'tcx hir::Item<'tcx>) {
        use hir::ItemKind::*;
        match item.kind {
            Fn {
                sig,
                generics,
                body,
                has_body,
                ..
            } => {
                self.scalar(has_body);
                self.visit_generics(generics);
                self.function(sig, body);
            }
            Const(_, generics, ty, rhs) => {
                self.text("const");
                self.visit_generics(generics);
                self.visit_ty_unambig(ty);
                self.visit_const_item_rhs(rhs);
            }
            Static(mutability, _, ty, body) => {
                self.text("static");
                self.scalar(mutability);
                self.visit_ty_unambig(ty);
                self.visit_nested_body(body);
            }
            TyAlias(_, generics, ty) => {
                self.text("type");
                self.visit_generics(generics);
                self.visit_ty_unambig(ty);
            }
            Struct(_, generics, data) | Union(_, generics, data) => {
                self.tag(&item.kind);
                self.visit_generics(generics);
                // The data is copied out of the item; visit through its borrowed original below.
                let original = match &item.kind {
                    Struct(_, _, data) | Union(_, _, data) => data,
                    _ => unreachable!(),
                };
                self.scalar(data.fields().len());
                self.visit_variant_data(original);
            }
            Enum(_, generics, ref definition) => {
                self.text("enum");
                self.visit_generics(generics);
                self.visit_enum_def(definition);
            }
            Trait {
                constness,
                is_auto,
                safety,
                generics,
                bounds,
                ..
            } => {
                self.text("trait");
                self.scalar(constness);
                self.scalar(is_auto);
                self.scalar(safety);
                self.visit_generics(generics);
                for bound in bounds {
                    self.visit_param_bound(bound);
                }
                // Methods have independent nodes, avoiding dependencies on every
                // sibling method merely because a signature mentions Self.
            }
            TraitAlias(constness, _, generics, bounds) => {
                self.text("trait-alias");
                self.scalar(constness);
                self.visit_generics(generics);
                for bound in bounds {
                    self.visit_param_bound(bound);
                }
            }
            _ => self.unsupported("non-definition item"),
        }
    }
}
