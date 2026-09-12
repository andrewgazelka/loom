use rustc_hir as hir;
use rustc_hir::def::DefKind;
use rustc_hir::def_id::LocalDefId;
use rustc_hir::intravisit::Visitor;
use rustc_hir::intravisit::VisitorExt;

use super::{Encoder, Part};

impl<'tcx> Encoder<'tcx> {
    pub(super) fn item(&mut self, id: LocalDefId) {
        let parent = self.tcx.local_parent(id);
        if self.tcx.def_kind(id) == DefKind::AssocTy {
            self.text("associated-type-slot");
            let slot = self
                .tcx
                .associated_item_def_ids(parent)
                .iter()
                .filter(|member| self.tcx.def_kind(**member) == DefKind::AssocTy)
                .position(|member| *member == id.to_def_id())
                .expect("associated type belongs to its container");
            self.scalar(slot);
        }
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

    fn impl_dependencies(&mut self) {
        self.text("adt-impls");
        self.parts.push(Part::Unordered(
            std::mem::take(&mut self.implementations)
                .into_iter()
                .map(|id| vec![Part::Reference(id)])
                .collect(),
        ));
    }

    fn implementation(&mut self, implementation: &'tcx hir::Impl<'tcx>) {
        self.text("impl");
        self.scalar(implementation.constness);
        self.visit_generics(implementation.generics);
        self.visit_ty_unambig(implementation.self_ty);
        self.scalar(implementation.of_trait.is_some());
        if let Some(header) = implementation.of_trait {
            self.scalar(header.safety);
            self.scalar(header.polarity);
            self.scalar(header.defaultness);
            self.visit_trait_ref(&header.trait_ref);
        }
        let members = implementation
            .items
            .iter()
            .map(|member| {
                let id = member.owner_id.def_id.to_def_id();
                let mut parts = Vec::new();
                if let Some(declaration) = self.tcx.associated_item(id).trait_item_def_id() {
                    let slot = self
                        .tcx
                        .associated_item_def_ids(self.tcx.parent(declaration))
                        .iter()
                        .position(|candidate| *candidate == declaration)
                        .expect("trait impl member has a declaration");
                    parts.push(Part::Bytes(b"trait-member-slot".to_vec()));
                    parts.push(Part::Bytes((slot as u64).to_le_bytes().to_vec()));
                }
                // Implementation membership references the actual body, not the
                // trait declaration used to identify generic dispatch in callers.
                parts.push(Part::Reference(id));
                parts
            })
            .collect();
        self.parts.push(Part::Unordered(members));
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
            Struct(name, generics, data) | Union(name, generics, data) => {
                self.tag(&item.kind);
                self.text(name.name.as_str());
                self.visit_generics(generics);
                // The data is copied out of the item; visit through its borrowed original below.
                let original = match &item.kind {
                    Struct(_, _, data) | Union(_, _, data) => data,
                    _ => unreachable!(),
                };
                self.scalar(data.fields().len());
                self.visit_variant_data(original);
                self.impl_dependencies();
            }
            Enum(name, generics, ref definition) => {
                self.text("enum");
                self.text(name.name.as_str());
                self.visit_generics(generics);
                self.visit_enum_def(definition);
                self.impl_dependencies();
            }
            Impl(ref implementation) => self.implementation(implementation),
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
