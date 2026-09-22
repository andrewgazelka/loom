//! Residual host rows over compiler-resolved instances, independent of item hashes.
use rustc_hir::def_id::DefId;
use rustc_middle::ty::{self, Instance, TyCtxt};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashMap};
mod schema;
mod sdk;
pub(crate) use schema::{append_contract, schema};
use sdk::effect as sdk_effect;
mod hir;
mod indirect;
mod mir;

#[derive(Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Row {
    pub labels: BTreeSet<String>,
    pub unknown: BTreeSet<Unknown>,
}
#[derive(Clone, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Unknown {
    pub item: String,
    pub span: String,
}
#[derive(Default, Serialize)]
pub struct Document {
    pub entries: BTreeMap<String, Row>,
    pub instances: BTreeMap<String, Row>,
}
#[derive(Deserialize)]
struct HandlerRow {
    #[serde(flatten)]
    row: Row,
    #[serde(default)]
    handled: BTreeSet<String>,
}
struct Edge<'tcx> {
    callee: Instance<'tcx>,
    handled: BTreeSet<String>,
}
#[derive(Default)]
struct Node<'tcx> {
    row: Row,
    edges: Vec<Edge<'tcx>>,
}
struct Analysis<'tcx> {
    tcx: TyCtxt<'tcx>,
    nodes: HashMap<Instance<'tcx>, Node<'tcx>>,
    instances: Vec<Instance<'tcx>>,
    handlers: BTreeMap<String, HandlerRow>,
}
impl Row {
    fn merge(&mut self, row: &Self, handled: &BTreeSet<String>) {
        self.labels.extend(row.labels.difference(handled).cloned());
        self.unknown.extend(row.unknown.iter().cloned());
    }
}
impl<'tcx> Analysis<'tcx> {
    fn resolve(&self, id: DefId, args: ty::GenericArgsRef<'tcx>) -> Option<Instance<'tcx>> {
        Instance::try_resolve(self.tcx, ty::TypingEnv::fully_monomorphized(), id, args)
            .unwrap_or_else(|_| self.tcx.dcx().fatal("effect callee resolution failed"))
    }
    fn discover(&mut self, root: Instance<'tcx>) {
        let mut pending = vec![root];
        while let Some(instance) = pending.pop() {
            if self.nodes.contains_key(&instance) {
                continue;
            }
            let mut node = Node::default();
            if let Some(label) = sdk_effect(self.tcx, instance.def_id()) {
                // Both primitives own wire dispatch; only the user's Serialize
                // and Deserialize implementations they reach can perform.
                if label == "$perform" || label == "$isolated_call" {
                    mir::scan_primitive(self, instance, &mut node);
                }
            } else if let Some(local) = instance.def_id().as_local()
                && let Some(body) = self.tcx.hir_maybe_body_owned_by(local)
                && matches!(instance.def, ty::InstanceKind::Item(_))
            {
                hir::scan(self, instance, body, &mut node);
                mir::scan_implicit(self, instance, &mut node);
            } else {
                mir::scan(self, instance, &mut node);
            }
            pending.extend(node.edges.iter().map(|edge| edge.callee));
            self.nodes.insert(instance, node);
        }
    }
}
fn dynamic_label(tcx: TyCtxt<'_>, span: rustc_span::Span) -> ! {
    let span = span.source_callsite();
    let location = tcx.sess.source_map().lookup_char_pos(span.lo());
    tcx.dcx().span_fatal(
        span,
        format!(
            "effect label at {}:{}:{} is not a literal or const; rows are inferred and need a static label",
            location.file.name.prefer_local_unconditionally(),
            location.line,
            location.col.0 + 1
        ),
    )
}
pub fn collect(tcx: TyCtxt<'_>) -> Document {
    let handlers = std::env::var("LOOM_HANDLER_ROWS")
        .map(|value| {
            serde_json::from_str(&value).unwrap_or_else(|error| {
                tcx.dcx()
                    .fatal(format!("invalid LOOM_HANDLER_ROWS: {error}"))
            })
        })
        .unwrap_or_default();
    let mut analysis = Analysis {
        tcx,
        nodes: HashMap::new(),
        instances: Vec::new(),
        handlers,
    };
    let partitions = tcx.collect_and_partition_mono_items(());
    let mut roots = Vec::new();
    for unit in partitions.codegen_units {
        for item in unit.items().keys() {
            if let rustc_middle::mono::MonoItem::Fn(instance) = item {
                roots.push(*instance);
            }
        }
    }
    // Public non-generic entries remain analyzable even when codegen drops them.
    for id in tcx.iter_local_def_id() {
        if crate::entries::is_entry(tcx, id)
            && tcx.def_kind(id).is_fn_like()
            && tcx.generics_of(id).count() == 0
        {
            roots.push(Instance::mono(tcx, id.to_def_id()));
        }
    }
    analysis.instances = roots.clone();
    let has_sdk = tcx.crate_name(rustc_hir::def_id::LOCAL_CRATE).as_str() == "loom_guest_rs"
        || tcx
            .crates(())
            .iter()
            .any(|id| tcx.crate_name(*id).as_str() == "loom_guest_rs");
    for root in &roots {
        if has_sdk {
            analysis.discover(*root);
        } else {
            analysis.nodes.insert(*root, Node::default());
        }
    }
    loop {
        let previous: HashMap<_, _> = analysis
            .nodes
            .iter()
            .map(|(id, node)| (*id, node.row.clone()))
            .collect();
        let mut changed = false;
        for node in analysis.nodes.values_mut() {
            let before = node.row.clone();
            for edge in &node.edges {
                node.row.merge(&previous[&edge.callee], &edge.handled);
            }
            changed |= before != node.row;
        }
        if !changed {
            break;
        }
    }
    let mut document = Document::default();
    for (instance, node) in &analysis.nodes {
        document
            .instances
            .insert(format!("{instance}"), node.row.clone());
        if let Some(local) = instance.def_id().as_local()
            && crate::entries::is_entry(tcx, local)
        {
            document
                .entries
                .entry(crate::graph::item_path(tcx, instance.def_id()))
                .or_default()
                .merge(&node.row, &BTreeSet::new());
        }
    }
    document
}
