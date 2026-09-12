use rustc_hir::intravisit::VisitorExt;
mod assembly;
mod expressions;
mod items;
mod metadata;
mod types;
mod visitor;

use std::collections::HashMap;
use std::fmt::Debug;

use rustc_hir as hir;
use rustc_hir::def::{DefKind, Res};
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_middle::ty::{TyCtxt, TypeckResults};

pub enum Part {
    Bytes(Vec<u8>),
    Reference(DefId),
}

pub struct Encoder<'tcx> {
    pub tcx: TyCtxt<'tcx>,
    pub parts: Vec<Part>,
    owner: LocalDefId,
    typeck: Option<&'tcx TypeckResults<'tcx>>,
    bindings: Vec<hir::HirId>,
    parameters: HashMap<LocalDefId, usize>,
    targets: Vec<hir::HirId>,
    auditing: bool,
}

impl<'tcx> Encoder<'tcx> {
    pub fn new(tcx: TyCtxt<'tcx>, owner: LocalDefId) -> Self {
        Self {
            tcx,
            owner,
            parts: Vec::new(),
            typeck: None,
            bindings: Vec::new(),
            parameters: HashMap::new(),
            targets: Vec::new(),
            auditing: false,
        }
    }

    pub fn audit(mut self) -> Result<Vec<Part>, String> {
        self.auditing = true;
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.encode())) {
            Ok(parts) => Ok(parts),
            Err(payload) => match payload.downcast::<CoverageRefusal>() {
                Ok(refusal) => Err(refusal.reason),
                Err(payload) => std::panic::resume_unwind(payload),
            },
        }
    }

    pub fn encode(mut self) -> Vec<Part> {
        self.text("loom-hir-v2");
        self.metadata(self.owner);
        self.item(self.owner);
        self.parts
    }

    fn text(&mut self, value: &str) {
        self.parts.push(Part::Bytes(value.as_bytes().to_vec()));
    }

    fn scalar(&mut self, value: impl Debug) {
        self.text(&format!("{value:?}"));
    }

    fn tag<T>(&mut self, value: &T) {
        self.text(std::any::type_name::<T>());
        self.scalar(std::mem::discriminant(value));
    }

    fn end(&mut self) {
        self.text("end");
    }

    fn unsupported(&self, thing: &str) -> ! {
        if self.auditing {
            // A typed, local unwind exits the HIR visitor without emitting a
            // fatal diagnostic or swallowing unrelated compiler panics.
            std::panic::resume_unwind(Box::new(CoverageRefusal {
                reason: thing.to_owned(),
            }));
        }
        self.tcx.dcx().fatal(format!(
            "hash-rustc: unsupported {thing} in {}",
            self.tcx.def_path_str(self.owner)
        ))
    }

    fn reference(&mut self, mut id: DefId) {
        // Explicit UFCS calls to trait impl methods have the same dispatch rule
        // as dot calls: identity belongs to the trait declaration.
        if matches!(self.tcx.def_kind(id), DefKind::AssocFn)
            && let Some(trait_id) = self
                .tcx
                .opt_associated_item(id)
                .and_then(|item| item.trait_item_def_id())
        {
            id = trait_id;
        }
        if let Some(local) = id.as_local()
            && let Some(position) = self.parameters.get(&local).copied()
        {
            self.text("generic-position");
            self.scalar(position);
        } else {
            self.parts.push(Part::Reference(id));
        }
    }

    fn resolution(&mut self, res: Res) {
        match res {
            Res::Def(_, id) => self.reference(id),
            Res::Local(id) => {
                let position = self
                    .bindings
                    .iter()
                    .rev()
                    .position(|bound| *bound == id)
                    .unwrap_or_else(|| self.unsupported("unbound local"));
                self.text("local-debruijn");
                self.scalar(position);
            }
            Res::PrimTy(ty) => {
                self.text("primitive");
                self.scalar(ty);
            }
            Res::SelfTyParam { trait_ } => {
                self.text("trait-self");
                self.reference(trait_);
            }
            Res::SelfTyAlias { alias_to, .. } => {
                let hir::Node::Item(item) = self.tcx.hir_node_by_def_id(alias_to.expect_local())
                else {
                    self.unsupported("Self outside impl");
                };
                let hir::ItemKind::Impl(implementation) = item.kind else {
                    self.unsupported("Self alias owner");
                };
                self.visit_ty_unambig(implementation.self_ty);
            }
            Res::SelfCtor(id) => self.resolution(Res::SelfTyAlias {
                alias_to: id,
                is_trait_impl: false,
            }),
            _ => self.unsupported("unresolved HIR path"),
        }
    }

    fn dependent(&mut self, id: hir::HirId, required: bool) {
        if let Some(definition) = self
            .typeck
            .and_then(|results| results.type_dependent_def_id(id))
        {
            self.text("type-dependent");
            self.reference(definition);
        } else if required {
            self.unsupported("method without a type_dependent_def_id");
        }
    }

    fn destination(&mut self, destination: hir::Destination) {
        let id = destination
            .target_id
            .unwrap_or_else(|_| self.unsupported("unresolved loop target"));
        let position = self
            .targets
            .iter()
            .rev()
            .position(|target| *target == id)
            .unwrap_or_else(|| self.unsupported("missing loop target"));
        self.scalar(position);
    }
}

struct CoverageRefusal {
    reason: String,
}
