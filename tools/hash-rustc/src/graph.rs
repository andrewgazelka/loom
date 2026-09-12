use std::collections::{BTreeMap, BTreeSet, HashMap};

use petgraph::algo::kosaraju_scc;
use petgraph::graph::{DiGraph, NodeIndex};
use rustc_hir::def::DefKind;
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_middle::ty::TyCtxt;
use serde::Serialize;

use crate::encode::{Encoder, Part};

#[derive(Serialize)]
pub struct Document {
    pub toolchain: String,
    #[serde(skip)]
    pub preimages: crate::preimages::Preimages,
    items: BTreeMap<String, Item>,
    entry: BTreeMap<String, String>,
}

#[derive(Serialize)]
struct Item {
    hash: String,
    refs: Vec<String>,
    cycle: Option<Vec<String>>,
}

impl Document {
    pub fn hash_for(&self, path: &str) -> Option<&str> {
        self.items.get(path).map(|item| item.hash.as_str())
    }
}

struct Definition {
    id: LocalDefId,
    path: String,
    parts: Vec<Part>,
    entry: bool,
}

pub(crate) fn supported(kind: DefKind) -> bool {
    matches!(
        kind,
        DefKind::Fn
            | DefKind::AssocFn
            | DefKind::Const { .. }
            | DefKind::AssocConst { .. }
            | DefKind::Static { .. }
            | DefKind::TyAlias
            | DefKind::AssocTy
            | DefKind::Struct
            | DefKind::Enum
            | DefKind::Union
            | DefKind::Trait
            | DefKind::TraitAlias
    )
}

pub fn collect(tcx: TyCtxt<'_>) -> Document {
    let mut definitions: Vec<Definition> = tcx
        .iter_local_def_id()
        .filter(|id| supported(tcx.def_kind(*id)))
        .map(|id| Definition {
            id,
            path: tcx.def_path_str(id),
            parts: Encoder::new(tcx, id).encode(),
            entry: crate::entries::is_entry(tcx, id),
        })
        .collect();
    definitions.sort_by(|a, b| a.path.cmp(&b.path));
    let mut graph = DiGraph::<usize, ()>::new();
    let mut indices = HashMap::new();
    for (index, definition) in definitions.iter().enumerate() {
        indices.insert(definition.id.to_def_id(), graph.add_node(index));
    }
    for definition in &mut definitions {
        // Constructors and variants carry their structural position plus the
        // enclosing ADT hash. They are not independent HIR owners.
        definition.parts = expand_references(tcx, std::mem::take(&mut definition.parts), &indices);
        for part in &definition.parts {
            if let Part::Reference(id) = part
                && id.is_local()
            {
                graph.add_edge(indices[&definition.id.to_def_id()], indices[id], ());
            }
        }
    }
    let mut hashes: HashMap<DefId, blake3::Hash> = HashMap::new();
    let mut items = BTreeMap::new();
    let mut entry = BTreeMap::new();
    let mut preimages = crate::preimages::Preimages::default();
    // kosaraju_scc emits sinks first: every outbound dependency outside a
    // component has already been hashed.
    for mut component in kosaraju_scc(&graph) {
        component.sort_by_key(|node| graph[*node]);
        let cycle = component.len() > 1 || graph.contains_edge(component[0], component[0]);
        let positions: HashMap<DefId, usize> = component
            .iter()
            .enumerate()
            .map(|(position, node)| (definitions[graph[*node]].id.to_def_id(), position))
            .collect();
        let names: Vec<String> = component
            .iter()
            .map(|node| definitions[graph[*node]].path.clone())
            .collect();
        let mut cycle_bytes = Vec::new();
        let mut encoded = Vec::new();
        for node in &component {
            let definition = &definitions[graph[*node]];
            let mut bytes = Vec::new();
            for part in &definition.parts {
                match part {
                    Part::Bytes(value) => {
                        bytes.push(0);
                        frame(&mut bytes, value);
                    }
                    Part::Reference(id) => {
                        if let Some(position) = positions.get(id) {
                            bytes.push(1);
                            bytes.extend_from_slice(&(*position as u64).to_le_bytes());
                        } else {
                            bytes.push(2);
                            let hash = if id.is_local() {
                                hashes[id]
                            } else {
                                external(tcx, *id)
                            };
                            bytes.extend_from_slice(hash.as_bytes());
                        }
                    }
                }
            }
            frame(&mut cycle_bytes, &bytes);
            encoded.push(bytes);
        }
        let cycle_hash = blake3::hash(&cycle_bytes);
        if cycle {
            preimages
                .cycles
                .insert(cycle_hash.to_hex().to_string(), cycle_bytes);
        }
        for (position, node) in component.iter().enumerate() {
            let definition = &definitions[graph[*node]];
            let bytes = if cycle {
                let mut bytes = cycle_hash.as_bytes().to_vec();
                bytes.extend_from_slice(&(position as u64).to_le_bytes());
                bytes
            } else {
                std::mem::take(&mut encoded[position])
            };
            let hash = blake3::hash(&bytes);
            preimages.items.insert(hash.to_hex().to_string(), bytes);
            hashes.insert(definition.id.to_def_id(), hash);
            let refs: BTreeSet<String> = definition
                .parts
                .iter()
                .filter_map(|part| {
                    if let Part::Reference(id) = part {
                        Some(tcx.def_path_str(*id))
                    } else {
                        None
                    }
                })
                .collect();
            items.insert(
                definition.path.clone(),
                Item {
                    hash: hash.to_hex().to_string(),
                    refs: refs.into_iter().collect(),
                    cycle: cycle.then(|| names.clone()),
                },
            );
            if definition.entry {
                entry.insert(definition.path.clone(), hash.to_hex().to_string());
            }
        }
    }
    Document {
        toolchain: String::new(),
        preimages,
        items,
        entry,
    }
}

fn frame(bytes: &mut Vec<u8>, value: &[u8]) {
    bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
    bytes.extend_from_slice(value);
}

pub(crate) fn external(tcx: TyCtxt<'_>, id: DefId) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    let name = tcx.crate_name(id.krate);
    hasher.update(&(name.as_str().len() as u64).to_le_bytes());
    hasher.update(name.as_str().as_bytes());
    hasher.update(&tcx.crate_hash(id.krate).as_u128().to_le_bytes());
    hasher.update(&tcx.def_path_hash(id).to_raw_def_path_hash().0);
    hasher.finalize()
}

fn expand_references(
    tcx: TyCtxt<'_>,
    parts: Vec<Part>,
    indices: &HashMap<DefId, NodeIndex>,
) -> Vec<Part> {
    let mut output = Vec::new();
    for part in parts {
        if let Part::Reference(mut id) = part {
            while id.is_local() && !indices.contains_key(&id) {
                let kind = tcx.def_kind(id);
                if !matches!(kind, DefKind::Ctor(..) | DefKind::Variant | DefKind::Field) {
                    tcx.dcx().fatal(format!(
                        "hash-rustc: reference to unsupported {} ({kind:?})",
                        tcx.def_path_str(id)
                    ));
                }
                let key = tcx.def_key(id);
                output.push(Part::Bytes(
                    format!("{kind:?}:{}", key.disambiguated_data.disambiguator).into_bytes(),
                ));
                if kind == DefKind::Variant {
                    let parent = tcx.parent(id);
                    let position = tcx
                        .adt_def(parent)
                        .variants()
                        .iter()
                        .position(|variant| variant.def_id == id)
                        .expect("variant position");
                    output.push(Part::Bytes(position.to_le_bytes().to_vec()));
                }
                id = tcx.parent(id);
            }
            output.push(Part::Reference(id));
        } else {
            output.push(part);
        }
    }
    output
}
