//! Adapter for the pinned compiler's LLVM backend. Cache hits enter the stock
//! scheduler but skip IR construction, optimization, and machine-code generation.
//! Tuple results below are imposed by the external rustc backend trait boundary.
use std::any::Any;

use rustc_ast::expand::allocator::AllocatorMethod;
use rustc_codegen_llvm::{LlvmCodegenBackend, ModuleLlvm};
use rustc_codegen_ssa::back::lto::ThinModule;
use rustc_codegen_ssa::back::write::{
    CodegenContext, FatLtoInput, ModuleConfig, OngoingCodegen, SharedEmitter,
    TargetMachineFactoryFn, ThinLtoInput,
};
use rustc_codegen_ssa::traits::{CodegenBackend, ExtraBackendMethods, WriteBackendMethods};
use rustc_codegen_ssa::{CompiledModule, CompiledModules, CrateInfo, ModuleCodegen, TargetConfig};
use rustc_data_structures::profiling::SelfProfilerRef;
use rustc_errors::{DiagCtxt, DiagCtxtHandle};
use rustc_metadata::{EncodedMetadata, creader::MetadataLoaderDyn};
use rustc_middle::{
    dep_graph::{WorkProduct, WorkProductMap},
    ty::TyCtxt,
    util::Providers,
};
use rustc_session::{
    IncrCompSession, Session,
    config::{self, OutputFilenames, OutputType, PrintRequest},
};
use rustc_span::Symbol;
use rustc_structures::CrateType;

use crate::cache_metrics::{Phase, Timer};
use crate::object_cache::{self, Request};

#[derive(Clone)]
pub struct CachingBackend {
    llvm: LlvmCodegenBackend,
}

pub enum CacheModule {
    Llvm {
        module: Option<ModuleLlvm>,
        request: Option<Request>,
    },
    Cached {
        request: Request,
    },
}

impl CachingBackend {
    pub fn new() -> Self {
        // The pinned rustc_codegen_llvm::LlvmCodegenBackend::new implementation
        // returns Box::new(LlvmCodegenBackend(())). Its concrete constructor is
        // private, so recover that same concrete allocation from its trait object.
        // This is not a cast of an arbitrary backend or a synthesized LLVM value.
        assert_eq!(std::mem::size_of::<LlvmCodegenBackend>(), 0);
        let raw = Box::into_raw(LlvmCodegenBackend::new());
        // SAFETY: pinned new() allocates exactly LlvmCodegenBackend; dropping the
        // metadata preserves its data pointer, allocation layout and ownership.
        let llvm = unsafe { *Box::from_raw(raw.cast::<LlvmCodegenBackend>()) };
        Self { llvm }
    }
}

impl CodegenBackend for CachingBackend {
    fn name(&self) -> &'static str {
        self.llvm.name()
    }
    fn init(&self, sess: &Session) {
        self.llvm.init(sess);
    }
    fn print(&self, req: &PrintRequest, out: &mut String, sess: &Session) {
        self.llvm.print(req, out, sess);
    }
    fn target_config(&self, sess: &Session) -> TargetConfig {
        self.llvm.target_config(sess)
    }
    fn supported_crate_types(&self, sess: &Session) -> Vec<CrateType> {
        self.llvm.supported_crate_types(sess)
    }
    fn print_passes(&self) {
        self.llvm.print_passes();
    }
    fn print_version(&self) {
        self.llvm.print_version();
    }
    fn replaced_intrinsics(&self) -> Vec<Symbol> {
        self.llvm.replaced_intrinsics()
    }
    fn fallback_intrinsics(&self) -> Vec<Symbol> {
        self.llvm.fallback_intrinsics()
    }
    fn thin_lto_supported(&self) -> bool {
        self.llvm.thin_lto_supported()
    }
    fn has_zstd(&self) -> bool {
        self.llvm.has_zstd()
    }
    fn has_mnemonic(&self, sess: &Session, mnemonic: &str) -> bool {
        self.llvm.has_mnemonic(sess, mnemonic)
    }
    fn metadata_loader(&self) -> Box<MetadataLoaderDyn> {
        self.llvm.metadata_loader()
    }
    fn provide(&self, providers: &mut Providers) {
        self.llvm.provide(providers);
    }
    fn target_cpu(&self, sess: &Session) -> String {
        self.llvm.target_cpu(sess)
    }
    fn codegen_crate<'tcx>(&self, tcx: TyCtxt<'tcx>) -> Box<dyn Any> {
        if object_cache::prepare(tcx) {
            assert!(
                matches!(tcx.sess.lto(), config::Lto::No),
                "object cache admission must exclude LTO"
            );
            Box::new(rustc_codegen_ssa::base::codegen_crate(self.clone(), tcx))
        } else {
            let _timer = Timer::start(Phase::Llvm);
            self.llvm.codegen_crate(tcx)
        }
    }
    fn join_codegen(
        &self,
        ongoing: Box<dyn Any>,
        sess: &Session,
        incr: Option<&IncrCompSession>,
        outputs: &OutputFilenames,
        info: &CrateInfo,
    ) -> (CompiledModules, WorkProductMap) {
        match ongoing.downcast::<OngoingCodegen<Self>>() {
            Ok(ongoing) => ongoing.join(sess, incr, info),
            Err(ongoing) => {
                let _timer = Timer::start(Phase::Llvm);
                self.llvm.join_codegen(ongoing, sess, incr, outputs, info)
            }
        }
    }
    fn print_pass_timings(&self) {
        self.llvm.print_pass_timings();
    }
    fn print_statistics(&self) {
        self.llvm.print_statistics();
    }
    fn print_statistics_json(&self) -> String {
        self.llvm.print_statistics_json()
    }
    fn link(
        &self,
        sess: &Session,
        modules: CompiledModules,
        info: CrateInfo,
        metadata: EncodedMetadata,
        outputs: &OutputFilenames,
    ) {
        self.llvm.link(sess, modules, info, metadata, outputs);
    }
}

impl ExtraBackendMethods for CachingBackend {
    type Module = CacheModule;
    fn codegen_allocator<'tcx>(
        &self,
        tcx: TyCtxt<'tcx>,
        name: &str,
        methods: &[AllocatorMethod],
    ) -> CacheModule {
        CacheModule::Llvm {
            module: Some(self.llvm.codegen_allocator(tcx, name, methods)),
            request: None,
        }
    }
    fn compile_codegen_unit(
        &self,
        tcx: TyCtxt<'_>,
        name: Symbol,
    ) -> (ModuleCodegen<CacheModule>, u64) {
        let request = object_cache::lookup(tcx, name);
        if let Some(request) = request.as_ref().filter(|request| request.hit()) {
            return (
                ModuleCodegen::new_regular(
                    name.to_string(),
                    CacheModule::Cached {
                        request: request.clone(),
                    },
                ),
                0,
            );
        }
        let _timer = Timer::start(Phase::Llvm);
        let (module, cost) = self.llvm.compile_codegen_unit(tcx, name);
        (
            ModuleCodegen {
                name: module.name,
                kind: module.kind,
                thin_lto_buffer: module.thin_lto_buffer,
                module_llvm: CacheModule::Llvm {
                    module: Some(module.module_llvm),
                    request,
                },
            },
            cost,
        )
    }
}

impl WriteBackendMethods for CachingBackend {
    type Module = CacheModule;
    type TargetMachine = <LlvmCodegenBackend as WriteBackendMethods>::TargetMachine;
    type ModuleBuffer = <LlvmCodegenBackend as WriteBackendMethods>::ModuleBuffer;
    type ThinData = <LlvmCodegenBackend as WriteBackendMethods>::ThinData;
    fn supports_parallel(&self) -> bool {
        self.llvm.supports_parallel()
    }
    fn thread_profiler() -> Box<dyn Any> {
        LlvmCodegenBackend::thread_profiler()
    }
    fn target_machine_factory(
        &self,
        sess: &Session,
        level: config::OptLevel,
        features: &[String],
    ) -> TargetMachineFactoryFn<Self> {
        self.llvm.target_machine_factory(sess, level, features)
    }
    fn optimize(
        cgcx: &CodegenContext,
        prof: &SelfProfilerRef,
        emitter: &SharedEmitter,
        module: &mut ModuleCodegen<CacheModule>,
        config: &ModuleConfig,
    ) {
        if let CacheModule::Llvm { module: llvm, .. } = &mut module.module_llvm {
            let mut inner = ModuleCodegen {
                name: module.name.clone(),
                kind: module.kind,
                thin_lto_buffer: module.thin_lto_buffer.take(),
                module_llvm: llvm
                    .take()
                    .expect("LLVM module available before optimization"),
            };
            let _timer = Timer::start(Phase::Llvm);
            LlvmCodegenBackend::optimize(cgcx, prof, emitter, &mut inner, config);
            module.name = inner.name;
            module.kind = inner.kind;
            module.thin_lto_buffer = inner.thin_lto_buffer;
            *llvm = Some(inner.module_llvm);
        }
    }
    fn codegen(
        cgcx: &CodegenContext,
        prof: &SelfProfilerRef,
        emitter: &SharedEmitter,
        module: ModuleCodegen<CacheModule>,
        config: &ModuleConfig,
    ) -> CompiledModule {
        match module.module_llvm {
            CacheModule::Cached { request } => {
                let object = cgcx
                    .output_filenames
                    .temp_path_for_cgu(OutputType::Object, &module.name);
                object_cache::restore(&request, &object).unwrap_or_else(|error| {
                    DiagCtxt::new(Box::new(emitter.clone()))
                        .handle()
                        .fatal(format!("object cache restore {}: {error}", request.key));
                });
                CompiledModule {
                    name: module.name,
                    kind: module.kind,
                    object: Some(object),
                    global_asm_object: None,
                    dwarf_object: None,
                    bytecode: None,
                    assembly: None,
                    llvm_ir: None,
                    links_from_incr_cache: Vec::new(),
                }
            }
            CacheModule::Llvm {
                module: llvm,
                request,
            } => {
                let inner = ModuleCodegen {
                    name: module.name,
                    kind: module.kind,
                    thin_lto_buffer: module.thin_lto_buffer,
                    module_llvm: llvm.expect("LLVM module available for codegen"),
                };
                let timer = Timer::start(Phase::Llvm);
                let compiled = LlvmCodegenBackend::codegen(cgcx, prof, emitter, inner, config);
                drop(timer);
                if let Some(request) = request {
                    let object = compiled
                        .object
                        .as_ref()
                        .expect("admitted cache miss must emit object");
                    if let Err(error) = object_cache::publish(&request, object) {
                        DiagCtxt::new(Box::new(emitter.clone()))
                            .handle()
                            .fatal(format!(
                                "object-cache: publish {} failed: {error}",
                                request.key
                            ));
                    }
                }
                compiled
            }
        }
    }
    fn serialize_module(module: CacheModule, is_thin: bool) -> Self::ModuleBuffer {
        match module {
            CacheModule::Llvm { module, .. } => LlvmCodegenBackend::serialize_module(
                module.expect("LLVM module available for serialization"),
                is_thin,
            ),
            CacheModule::Cached { .. } => {
                panic!("cache admission incorrectly allowed bitcode serialization")
            }
        }
    }
    fn optimize_and_codegen_fat_lto(
        _sess: &Session,
        _cgcx: &CodegenContext,
        _emitter: &SharedEmitter,
        _factory: TargetMachineFactoryFn<Self>,
        _symbols: &[String],
        _rlibs: &[std::path::PathBuf],
        _modules: Vec<FatLtoInput<Self>>,
    ) -> CompiledModule {
        panic!("cache admission incorrectly allowed fat LTO")
    }
    fn run_thin_lto(
        _cgcx: &CodegenContext,
        _prof: &SelfProfilerRef,
        _dcx: DiagCtxtHandle<'_>,
        _symbols: &[String],
        _rlibs: &[std::path::PathBuf],
        _modules: Vec<ThinLtoInput<Self>>,
    ) -> (Vec<ThinModule<Self>>, Vec<WorkProduct>) {
        panic!("cache admission incorrectly allowed thin LTO")
    }
    fn optimize_and_codegen_thin(
        _cgcx: &CodegenContext,
        _prof: &SelfProfilerRef,
        _emitter: &SharedEmitter,
        _factory: TargetMachineFactoryFn<Self>,
        _thin: ThinModule<Self>,
    ) -> CompiledModule {
        panic!("cache admission incorrectly allowed thin LTO")
    }
}
