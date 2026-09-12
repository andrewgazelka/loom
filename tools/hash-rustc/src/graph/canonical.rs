//! Content-based cycle positions and order-independent dependency groups.
use std::collections::{BTreeMap, HashMap};

use petgraph::graph::{DiGraph, NodeIndex};
use rustc_hir::def_id::DefId;
use rustc_middle::ty::TyCtxt;

use super::{Definition, external, frame};
use crate::encode::Part;

pub(super) struct Component {
    pub groups: Vec<Vec<NodeIndex>>,
    pub positions: HashMap<DefId, usize>,
}

pub(super) fn component(
    tcx: TyCtxt<'_>,
    nodes: Vec<NodeIndex>,
    graph: &DiGraph<usize, ()>,
    definitions: &[Definition],
    hashes: &HashMap<DefId, blake3::Hash>,
) -> Component {
    let mut positions: HashMap<DefId, usize> = nodes
        .iter()
        .map(|node| (definitions[graph[*node]].id.to_def_id(), 0))
        .collect();
    let mut class_count = 1;
    loop {
        let mut classes: BTreeMap<Vec<u8>, Vec<NodeIndex>> = BTreeMap::new();
        for node in &nodes {
            let definition = &definitions[graph[*node]];
            // Retaining the prior class makes refinement monotone: classes
            // split, never merge. At most nodes.len() rounds can split classes.
            let mut signature = (positions[&definition.id.to_def_id()] as u64)
                .to_le_bytes()
                .to_vec();
            signature.extend(encode(tcx, &definition.parts, &positions, hashes));
            classes.entry(signature).or_default().push(*node);
        }
        let groups: Vec<Vec<NodeIndex>> = classes.into_values().collect();
        for (position, group) in groups.iter().enumerate() {
            for node in group {
                positions.insert(definitions[graph[*node]].id.to_def_id(), position);
            }
        }
        if groups.len() == class_count {
            // Indistinguishable recursive definitions share one stream and
            // member hash, just as identical nonrecursive functions do.
            return Component { groups, positions };
        }
        class_count = groups.len();
    }
}

pub(super) fn encode(
    tcx: TyCtxt<'_>,
    parts: &[Part],
    positions: &HashMap<DefId, usize>,
    hashes: &HashMap<DefId, blake3::Hash>,
) -> Vec<u8> {
    let mut bytes = Vec::new();
    for part in parts {
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
            Part::Unordered(entries) => {
                bytes.push(3);
                bytes.extend_from_slice(&(entries.len() as u64).to_le_bytes());
                let mut entries: Vec<Vec<u8>> = entries
                    .iter()
                    .map(|entry| encode(tcx, entry, positions, hashes))
                    .collect();
                entries.sort();
                for entry in entries {
                    frame(&mut bytes, &entry);
                }
            }
        }
    }
    bytes
}
