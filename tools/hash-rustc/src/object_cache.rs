//! Content identities, admission, and symbol relocation for native object reuse.
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use rustc_middle::middle::codegen_fn_attrs::CodegenFnAttrFlags;
use rustc_middle::mir::TerminatorKind;
use rustc_middle::mono::{CodegenUnit, MonoItem};
use rustc_middle::ty::{self, GenericArgKind, Instance, Ty, TyCtxt};
use rustc_span::Symbol;

use crate::cache_metrics::{self, Phase, Timer};
use crate::object_store::{Entry, Store};

const FORMAT: &str = "loom-object-v2-macho-mono";
static STATE: OnceLock<State> = OnceLock::new();
static CGUS: AtomicUsize = AtomicUsize::new(0);
static HITS: AtomicUsize = AtomicUsize::new(0);
static MISSES: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static TEMP: AtomicU64 = AtomicU64::new(0);

struct State {
    store: Store,
    requests: BTreeMap<String, Request>,
}

#[derive(Clone)]
pub struct Request {
    pub key: String,
    pub item_count: usize,
    symbols: Vec<String>,
    entry: Option<Entry>,
}

impl Request {
    pub fn hit(&self) -> bool {
        self.entry.is_some()
    }
    fn canonical_symbols(&self) -> Vec<String> {
        (0..self.symbols.len())
            .map(|index| format!("_loom_{}_{}", self.key, index))
            .collect()
    }
}

struct ItemIdentity {
    hash: String,
    symbol: String,
}

struct FunctionIdentity {
    bytes: Vec<u8>,
    references: Vec<ItemIdentity>,
}

pub fn prepare(tcx: TyCtxt<'_>) -> bool {
    let Some(directory) = std::env::var_os("LOOM_OBJECT_CACHE") else {
        return false;
    };
    let partitions = tcx.collect_and_partition_mono_items(());
    CGUS.store(partitions.codegen_units.len(), Ordering::Relaxed);
    let flags = match crate::cache_flags::key(tcx) {
        Ok(flags) => flags,
        Err(reason) => {
            eprintln!("object-cache: bypass: {reason}");
            MISSES.store(partitions.codegen_units.len(), Ordering::Relaxed);
            return false;
        }
    };
    // Reject unsupported lowered code before invoking the HIR encoder. Real CGUs
    // commonly contain shims or pointer/aggregate layouts, and an unrelated HIR
    // definition outside that admitted surface must not abort ordinary compilation.
    let mut eligible = Vec::new();
    {
        cache_metrics::hashing_call();
        let _timer = Timer::start(Phase::Hashing);
        for cgu in partitions.codegen_units {
            let result = cgu.items().keys().try_for_each(|item| match item {
                MonoItem::Fn(instance) => {
                    function_identity(tcx, *instance, None, &mut Vec::new()).map(|_| ())
                }
                _ => Err("static or global assembly requires a relocation identity".into()),
            });
            match result {
                Ok(()) => eligible.push(cgu),
                Err(reason) => eprintln!("object-cache: bypass {}: {reason}", cgu.name()),
            }
        }
    }
    if eligible.is_empty() {
        MISSES.store(partitions.codegen_units.len(), Ordering::Relaxed);
        return false;
    }
    let store = Store::new(PathBuf::from(directory))
        .unwrap_or_else(|error| tcx.dcx().fatal(format!("object-cache: {error}")));
    let document = {
        cache_metrics::hashing_call();
        let _timer = Timer::start(Phase::Hashing);
        crate::graph::collect(tcx)
    };
    let mut requests = BTreeMap::new();
    for cgu in eligible {
        match request(tcx, cgu, &document, &flags, &store) {
            Ok(request) => {
                requests.insert(cgu.name().to_string(), request);
            }
            Err(reason) => {
                eprintln!("object-cache: bypass {}: {reason}", cgu.name());
            }
        }
    }
    assert!(
        STATE.set(State { store, requests }).is_ok(),
        "one compiler session per driver"
    );
    true
}

pub fn lookup(_: TyCtxt<'_>, name: Symbol) -> Option<Request> {
    let request = STATE
        .get()
        .and_then(|state| state.requests.get(name.as_str()))
        .cloned();
    if let Some(entry) = request.as_ref().and_then(|request| request.entry.as_ref()) {
        HITS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(entry.bytes, Ordering::Relaxed);
    } else {
        MISSES.fetch_add(1, Ordering::Relaxed);
    }
    request
}

fn request<'tcx>(
    tcx: TyCtxt<'tcx>,
    cgu: &CodegenUnit<'tcx>,
    document: &crate::graph::Document,
    flags: &[u8],
    store: &Store,
) -> Result<Request, String> {
    let hashing_timer = Timer::start(Phase::Hashing);
    let mut identities = Vec::new();
    let mut references = Vec::new();
    for (item, data) in cgu.items() {
        let MonoItem::Fn(instance) = item else {
            return Err("static or global assembly requires a relocation identity".into());
        };
        let identity = function_identity(tcx, *instance, Some(document), &mut Vec::new())?;
        references.extend(identity.references);
        let mut bytes = identity.bytes;
        frame(
            &mut bytes,
            format!("{:?}:{:?}:{}", data.linkage, data.visibility, data.inlined).as_bytes(),
        );
        identities.push(ItemIdentity {
            hash: blake3::hash(&bytes).to_hex().to_string(),
            symbol: format!("_{}", item.symbol_name(tcx).name),
        });
    }
    identities.sort_by(|a, b| a.hash.cmp(&b.hash).then(a.symbol.cmp(&b.symbol)));
    let mut bytes = Vec::new();
    frame(&mut bytes, FORMAT.as_bytes());
    frame(&mut bytes, flags);
    for identity in &identities {
        frame(&mut bytes, identity.hash.as_bytes());
    }
    let item_count = identities.len();
    let mut symbols: Vec<String> = identities.into_iter().map(|item| item.symbol).collect();
    references.sort_by(|a, b| a.hash.cmp(&b.hash).then(a.symbol.cmp(&b.symbol)));
    for reference in references {
        let index = if let Some(index) = symbols
            .iter()
            .position(|symbol| symbol == &reference.symbol)
        {
            index
        } else {
            let index = symbols.len();
            symbols.push(reference.symbol);
            index
        };
        frame(&mut bytes, reference.hash.as_bytes());
        frame(&mut bytes, &(index as u64).to_le_bytes());
    }
    let key = blake3::hash(&bytes).to_hex().to_string();
    drop(hashing_timer);
    let entry = store.lookup(&key).unwrap_or_else(|error| {
        tcx.dcx()
            .fatal(format!("object-cache: invalid cache entry {key}: {error}"))
    });
    let request = Request {
        key,
        item_count,
        symbols,
        entry,
    };
    if let Some(entry) = &request.entry
        && (entry.key != request.key || entry.symbols != request.canonical_symbols())
    {
        tcx.dcx()
            .fatal("object-cache: invalid canonical symbol map");
    }
    Ok(request)
}

fn function_identity<'tcx>(
    tcx: TyCtxt<'tcx>,
    instance: Instance<'tcx>,
    document: Option<&crate::graph::Document>,
    active: &mut Vec<Instance<'tcx>>,
) -> Result<FunctionIdentity, String> {
    if active.contains(&instance) || active.len() >= 64 {
        return Err("recursive mono call graph requires a cycle identity".into());
    }
    active.push(instance);
    let identity = encode_function(tcx, instance, document, active);
    active.pop();
    identity
}

fn encode_function<'tcx>(
    tcx: TyCtxt<'tcx>,
    instance: Instance<'tcx>,
    document: Option<&crate::graph::Document>,
    active: &mut Vec<Instance<'tcx>>,
) -> Result<FunctionIdentity, String> {
    let identity = crate::mono::identity(tcx, MonoItem::Fn(instance), document);
    // External code with wholly external substituted identities is fixed by
    // dependency crate hashes. Local substitutions can select local impls and
    // require a codegen dependency graph before they can be admitted.
    if !instance.def_id().is_local() {
        let identity = identity?;
        if !identity.local {
            let mut bytes = identity.bytes;
            // Generic symbols referenced by this object can name the local
            // instantiating crate. Until those relocations are mapped, keep
            // reuse within that symbol namespace, including -Cmetadata.
            frame(&mut bytes, b"instantiating-crate");
            frame(
                &mut bytes,
                &tcx.stable_crate_id(rustc_hir::def_id::LOCAL_CRATE)
                    .as_u64()
                    .to_le_bytes(),
            );
            return Ok(FunctionIdentity {
                bytes,
                references: Vec::new(),
            });
        }
        return Err(
            "external instance with local substitutions requires codegen dependency identity"
                .into(),
        );
    }
    if !matches!(instance.def, ty::InstanceKind::Item(_)) {
        return Err("local compiler shim requires codegen dependency identity".into());
    }
    let path = tcx.def_path_str(instance.def_id());
    let mut bytes = if document.is_some() {
        identity?.bytes
    } else {
        Vec::new()
    };
    let mut references = Vec::new();
    // Local codegen admission retains scalar arguments and MIR. Actual key
    // construction uses the canonical mono encoder and complete HIR document.
    for argument in instance.args {
        match argument.kind() {
            GenericArgKind::Type(ty) => {
                scalar_type(ty)?;
            }
            GenericArgKind::Lifetime(_) => {}
            GenericArgKind::Const(_) => {
                return Err("local const generic requires lowered-code admission".into());
            }
        }
    }
    let attributes = tcx.codegen_fn_attrs(instance.def_id());
    frame(
        &mut bytes,
        format!("{:?}:{:?}", attributes.inline, attributes.optimize).as_bytes(),
    );
    // HIR identity intentionally excludes compiler adjustments and selected trait
    // implementations. Record resolved direct callees as well as scalar lowered
    // code. Drop glue, allocations and source-location panic records bypass.
    let body = tcx.instance_mir(instance.def);
    for local in body.local_decls.iter() {
        let ty = instance.instantiate_mir_and_normalize_erasing_regions(
            tcx,
            ty::TypingEnv::fully_monomorphized(),
            ty::EarlyBinder::bind(tcx, local.ty),
        );
        frame(&mut bytes, &scalar_type(ty)?);
    }
    for block in body.basic_blocks.iter() {
        frame(
            &mut bytes,
            if block.is_cleanup {
                b"cleanup"
            } else {
                b"normal"
            },
        );
        for statement in &block.statements {
            frame(&mut bytes, format!("{:?}", statement.kind).as_bytes());
        }
        match &block.terminator().kind {
            TerminatorKind::Return
            | TerminatorKind::Goto { .. }
            | TerminatorKind::SwitchInt { .. }
            | TerminatorKind::Unreachable
            | TerminatorKind::UnwindResume => {
                frame(
                    &mut bytes,
                    format!("{:?}", block.terminator().kind).as_bytes(),
                );
            }
            TerminatorKind::Call {
                func,
                args,
                destination,
                target,
                unwind,
                call_source,
                fn_span: _,
            } => {
                let callable = instance.instantiate_mir_and_normalize_erasing_regions(
                    tcx,
                    ty::TypingEnv::fully_monomorphized(),
                    ty::EarlyBinder::bind(tcx, func.ty(&body.local_decls, tcx)),
                );
                let ty::FnDef(def_id, arguments) = callable.kind() else {
                    return Err("indirect call requires callable identity".into());
                };
                let callee = Instance::try_resolve(
                    tcx,
                    ty::TypingEnv::fully_monomorphized(),
                    *def_id,
                    arguments
                        .no_bound_vars()
                        .ok_or("late-bound callee arguments")?,
                )
                .map_err(|_| "callee resolution failed")?
                .ok_or("callee remains unresolved")?;
                if tcx
                    .codegen_fn_attrs(callee.def_id())
                    .flags
                    .contains(CodegenFnAttrFlags::TRACK_CALLER)
                {
                    return Err("track_caller call requires source location identity".into());
                }
                let identity = function_identity(tcx, callee, document, active)?;
                let callee_hash = blake3::hash(&identity.bytes).to_hex().to_string();
                references.extend(identity.references);
                references.push(ItemIdentity {
                    hash: callee_hash.clone(),
                    symbol: format!("_{}", tcx.symbol_name(callee).name),
                });
                frame(&mut bytes, b"resolved-call");
                frame(&mut bytes, callee_hash.as_bytes());
                for argument in args {
                    frame(&mut bytes, format!("{:?}", argument.node).as_bytes());
                }
                frame(
                    &mut bytes,
                    format!("{destination:?}:{target:?}:{unwind:?}:{call_source:?}").as_bytes(),
                );
            }
            _ => {
                return Err(format!(
                    "{path} has a terminator requiring an additional codegen identity"
                ));
            }
        }
    }
    Ok(FunctionIdentity { bytes, references })
}

fn scalar_type(ty: Ty<'_>) -> Result<Vec<u8>, String> {
    match ty.kind() {
        ty::Bool | ty::Char | ty::Int(_) | ty::Uint(_) | ty::Float(_) | ty::Never => {
            Ok(format!("{:?}", ty.kind()).into_bytes())
        }
        ty::Tuple(fields) if fields.is_empty() => Ok(b"unit".to_vec()),
        _ => Err(format!(
            "type {ty} requires layout, allocation, or drop identity"
        )),
    }
}

fn frame(bytes: &mut Vec<u8>, value: &[u8]) {
    bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
    bytes.extend_from_slice(value);
}

pub fn restore(request: &Request, destination: &Path) -> io::Result<()> {
    let state = STATE.get().expect("prepared cache");
    let entry = state
        .store
        .lookup(&request.key)?
        .ok_or_else(|| io::Error::other("cache object disappeared"))?;
    let _timer = Timer::start(Phase::ObjectCopy);
    relocate(&entry.object, destination, &entry.symbols, &request.symbols)
}

pub fn publish(request: &Request, object: &Path) -> io::Result<()> {
    let _timer = Timer::start(Phase::ObjectCopy);
    let canonical = request.canonical_symbols();
    let temporary = Temporary {
        path: object.with_extension(format!(
            "canonical-{}-{}",
            std::process::id(),
            TEMP.fetch_add(1, Ordering::Relaxed)
        )),
    };
    relocate(object, &temporary.path, &request.symbols, &canonical)?;
    STATE.get().expect("prepared cache").store.publish(
        &request.key,
        request.item_count,
        &temporary.path,
        &canonical,
    )?;
    Ok(())
}

fn relocate(source: &Path, destination: &Path, from: &[String], to: &[String]) -> io::Result<()> {
    cache_metrics::copy_call();
    if from.len() != to.len() {
        return Err(io::Error::other("cache symbol cardinality mismatch"));
    }
    let tool = Path::new(env!("HASH_RUSTC_SYSROOT"))
        .join("lib/rustlib/aarch64-apple-darwin/bin/llvm-objcopy");
    let mut command = Command::new(tool);
    for (from, to) in from.iter().zip(to) {
        command.arg(format!("--redefine-sym={from}={to}"));
    }
    let output = command.arg(source).arg(destination).output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "symbol relocation failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
    Ok(())
}

struct Temporary {
    path: PathBuf,
}
impl Drop for Temporary {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn print_stats() {
    if std::env::var("LOOM_OBJECT_CACHE_STATS").as_deref() == Ok("1") {
        eprintln!(
            "object-cache: cgus={} hits={} misses={} bytes_reused={}",
            CGUS.load(Ordering::Relaxed),
            HITS.load(Ordering::Relaxed),
            MISSES.load(Ordering::Relaxed),
            BYTES.load(Ordering::Relaxed)
        );
    }
}
