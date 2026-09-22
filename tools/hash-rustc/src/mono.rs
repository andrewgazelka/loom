//! Canonical instantiated identities. Codegen admission is a separate consumer.
use rustc_hir::def::DefKind;
use rustc_hir::def_id::DefId;
use rustc_middle::mono::MonoItem;
use rustc_middle::ty::{self, GenericArgKind, GenericArgsRef, Instance, Ty, TyCtxt};

pub struct Identity {
    pub bytes: Vec<u8>,
    pub local: bool,
}

pub struct Encoder<'a, 'tcx> {
    tcx: TyCtxt<'tcx>,
    document: Option<&'a crate::graph::Document>,
    pub bytes: Vec<u8>,
    pub local: bool,
}

impl<'a, 'tcx> Encoder<'a, 'tcx> {
    pub fn new(tcx: TyCtxt<'tcx>, document: Option<&'a crate::graph::Document>) -> Self {
        Self {
            tcx,
            document,
            bytes: Vec::new(),
            local: false,
        }
    }

    pub fn field(&mut self, bytes: &[u8]) {
        self.bytes
            .extend_from_slice(&(bytes.len() as u64).to_le_bytes());
        self.bytes.extend_from_slice(bytes);
    }

    fn scalar(&mut self, value: impl std::fmt::Debug) {
        self.field(format!("{value:?}").as_bytes());
    }

    pub fn definition(&mut self, id: DefId) -> Result<(), String> {
        let hash = self.definition_hash(id)?;
        self.field(hash.as_bytes());
        Ok(())
    }

    fn definition_hash(&mut self, id: DefId) -> Result<blake3::Hash, String> {
        if !id.is_local() {
            return Ok(crate::graph::crate_reference(self.tcx, id));
        }
        self.local = true;
        let path = crate::graph::item_path(self.tcx, id);
        if let Some(hash) = self.document.and_then(|document| document.hash_for(&path)) {
            return blake3::Hash::from_hex(hash).map_err(|error| error.to_string());
        }
        match self.tcx.def_kind(id) {
            DefKind::Closure
            | DefKind::Ctor(..)
            | DefKind::Variant
            | DefKind::Field
            | DefKind::AnonConst
            | DefKind::OpaqueTy => {
                let mut nested = Self::new(self.tcx, self.document);
                nested.field(b"nested-definition");
                nested.scalar(self.tcx.def_kind(id));
                // The enclosing HIR includes the nested body. Its lexical ordinal
                // distinguishes two closures with the same signature in that body.
                nested.scalar(self.tcx.def_key(id).disambiguated_data.disambiguator);
                if self.tcx.def_kind(id) == DefKind::Variant {
                    nested.scalar(
                        self.tcx
                            .adt_def(self.tcx.parent(id))
                            .variant_index_with_id(id),
                    );
                }
                nested.definition(self.tcx.parent(id))?;
                Ok(blake3::hash(&nested.bytes))
            }
            kind => Err(format!("no HIR identity for {path} ({kind:?})")),
        }
    }

    pub fn args(&mut self, args: GenericArgsRef<'tcx>) -> Result<(), String> {
        self.field(b"generic-args-v1");
        self.scalar(args.len());
        for argument in args {
            match argument.kind() {
                GenericArgKind::Lifetime(_) => self.field(b"erased-region"),
                GenericArgKind::Type(ty) => {
                    self.field(b"type");
                    self.ty(ty)?;
                }
                GenericArgKind::Const(value) => {
                    self.field(b"const");
                    self.constant(value)?;
                }
            }
        }
        Ok(())
    }

    fn binder(&mut self, variables: &'tcx rustc_middle::ty::List<ty::BoundVariableKind<'tcx>>) {
        self.scalar(variables.len());
        for variable in variables {
            match variable {
                ty::BoundVariableKind::Region(_) => self.field(b"region"),
                ty::BoundVariableKind::Ty(_) => self.field(b"type"),
                ty::BoundVariableKind::Const => self.field(b"const"),
            }
        }
    }

    pub fn ty(&mut self, value: Ty<'tcx>) -> Result<(), String> {
        self.scalar(std::mem::discriminant(value.kind()));
        match *value.kind() {
            ty::Bool | ty::Char | ty::Str | ty::Never => {}
            ty::Int(kind) => self.scalar(kind),
            ty::Uint(kind) => self.scalar(kind),
            ty::Float(kind) => self.scalar(kind),
            ty::Adt(definition, args) => {
                self.definition(definition.did())?;
                self.args(args)?;
            }
            ty::Foreign(id) => self.definition(id)?,
            ty::Array(element, count) => {
                self.ty(element)?;
                self.constant(count)?;
            }
            ty::Slice(element) => self.ty(element)?,
            ty::RawPtr(element, mutability) | ty::Ref(_, element, mutability) => {
                self.scalar(mutability);
                self.ty(element)?;
            }
            ty::Tuple(elements) => {
                self.scalar(elements.len());
                for element in elements {
                    self.ty(element)?;
                }
            }
            ty::FnDef(id, args) => {
                self.definition(id)?;
                self.binder(args.bound_vars());
                self.args(args.skip_binder())?;
            }
            ty::FnPtr(signature, header) => {
                self.binder(signature.bound_vars());
                self.scalar(header);
                let signature = signature.skip_binder();
                self.scalar(signature.inputs_and_output.len());
                for element in signature.inputs_and_output {
                    self.ty(element)?;
                }
            }
            ty::Closure(id, args)
            | ty::CoroutineClosure(id, args)
            | ty::Coroutine(id, args)
            | ty::CoroutineWitness(id, args) => {
                self.definition(id)?;
                self.args(args)?;
            }
            ty::Dynamic(predicates, _) => {
                self.scalar(predicates.len());
                for predicate in predicates {
                    self.binder(predicate.bound_vars());
                    let predicate = predicate.skip_binder();
                    self.scalar(std::mem::discriminant(&predicate));
                    match predicate {
                        ty::ExistentialPredicate::Trait(reference) => {
                            self.definition(reference.def_id)?;
                            self.args(reference.args)?;
                        }
                        ty::ExistentialPredicate::AutoTrait(id) => self.definition(id)?,
                        ty::ExistentialPredicate::Projection(projection) => {
                            self.definition(projection.def_id)?;
                            self.args(projection.args)?;
                            match projection.term.kind() {
                                ty::TermKind::Ty(ty) => {
                                    self.field(b"type");
                                    self.ty(ty)?;
                                }
                                ty::TermKind::Const(value) => {
                                    self.field(b"const");
                                    self.constant(value)?;
                                }
                            }
                        }
                    }
                }
            }
            ty::UnsafeBinder(inner) => {
                self.binder(inner.bound_vars());
                self.ty(inner.skip_binder())?;
            }
            ty::Param(parameter) => self.scalar(parameter.index),
            ty::Bound(depth, bound) => {
                self.scalar(depth);
                self.scalar(bound.var);
            }
            ty::Alias(..) | ty::Placeholder(_) | ty::Infer(_) | ty::Error(_) | ty::Pat(..) => {
                return Err(format!(
                    "type {value} has no canonical instantiated identity"
                ));
            }
        }
        Ok(())
    }

    fn constant(&mut self, value: ty::Const<'tcx>) -> Result<(), String> {
        match value.kind() {
            ty::ConstKind::Value(value) => {
                self.ty(value.ty)?;
                match &**value.valtree {
                    ty::ValTreeKind::Leaf(bits) => {
                        self.field(b"leaf");
                        self.field(&bits.size().bytes().to_le_bytes());
                        self.field(&bits.to_bits_unchecked().to_le_bytes());
                    }
                    ty::ValTreeKind::Branch(values) => {
                        self.field(b"branch");
                        self.scalar(values.len());
                        for value in *values {
                            self.constant(value)?;
                        }
                    }
                }
                Ok(())
            }
            _ => Err(format!("const {value} has no evaluated canonical value")),
        }
    }

    pub fn instance(&mut self, instance: Instance<'tcx>) -> Result<(), String> {
        // The generic definition hash comes first; the argument encoding is
        // self-delimiting. Shim recipes extend the generic definition identity.
        let mut generic = Self::new(self.tcx, self.document);
        let definition = generic.definition_hash(instance.def_id())?;
        generic.bytes.extend_from_slice(definition.as_bytes());
        match instance.def {
            ty::InstanceKind::Item(_) => {}
            ty::InstanceKind::Intrinsic(_) => generic.field(b"intrinsic"),
            ty::InstanceKind::LlvmIntrinsic(_) => generic.field(b"llvm-intrinsic"),
            ty::InstanceKind::Virtual(_, slot) => {
                generic.field(b"virtual");
                generic.scalar(slot);
            }
            ty::InstanceKind::Shim(shim) => {
                generic.field(b"shim");
                generic.scalar(std::mem::discriminant(&shim));
                use ty::ShimKind::*;
                match shim {
                    VTable(_) | ThreadLocal(_) => {}
                    Reify(_, reason) => generic.scalar(reason),
                    FnPtr(_, ty)
                    | Clone(_, ty)
                    | FnPtrAsPtr(_, ty)
                    | FnPtrFromPtr(_, ty)
                    | AsyncDropGlueCtor(_, ty)
                    | AsyncDropGlue(_, ty) => generic.ty(ty)?,
                    DropGlue(_, ty) => {
                        generic.scalar(ty.is_some());
                        if let Some(ty) = ty {
                            generic.ty(ty)?;
                        }
                    }
                    FutureDropPoll(_, proxy, implementation) => {
                        generic.ty(proxy)?;
                        generic.ty(implementation)?;
                    }
                    ClosureOnce {
                        closure,
                        track_caller,
                        ..
                    } => {
                        generic.definition(closure)?;
                        generic.scalar(track_caller);
                    }
                    ConstructCoroutineInClosure {
                        receiver_by_ref, ..
                    } => generic.scalar(receiver_by_ref),
                }
            }
        }
        self.local |= generic.local;
        if matches!(instance.def, ty::InstanceKind::Item(_)) {
            self.bytes.extend_from_slice(definition.as_bytes());
        } else {
            self.bytes
                .extend_from_slice(blake3::hash(&generic.bytes).as_bytes());
        }
        self.args(instance.args)
    }
}

pub fn identity<'tcx>(
    tcx: TyCtxt<'tcx>,
    item: MonoItem<'tcx>,
    document: Option<&crate::graph::Document>,
) -> Result<Identity, String> {
    let mut encoder = Encoder::new(tcx, document);
    match item {
        MonoItem::Fn(instance) => encoder.instance(instance)?,
        MonoItem::Static(id) => {
            encoder.field(b"static");
            encoder.definition(id)?;
        }
        MonoItem::GlobalAsm(_) => {
            return Err("global assembly has no independent HIR identity".into());
        }
    }
    Ok(Identity {
        bytes: encoder.bytes,
        local: encoder.local,
    })
}
