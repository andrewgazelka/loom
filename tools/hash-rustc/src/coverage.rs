//! Explicit diagnostic inventory; never used for cache admission or hashing.
use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use rustc_hir::def::DefKind;
use rustc_middle::ty::TyCtxt;
use serde::Serialize;

use crate::encode::{Encoder, Part};

#[derive(Serialize)]
struct Report {
    candidates: usize,
    encoded: usize,
    refused: usize,
    reasons: BTreeMap<String, usize>,
    items: BTreeMap<String, String>,
    mono: MonoReport,
}

#[derive(Default, Serialize)]
struct MonoReport {
    cgus: usize,
    placements: usize,
    unique_items: usize,
    refused_unique_items: usize,
    refused_placements: usize,
    items: BTreeMap<String, String>,
    hashes: BTreeMap<String, String>,
}

pub fn write(tcx: TyCtxt<'_>, path: &Path) {
    let ids: Vec<_> = tcx
        .iter_local_def_id()
        .filter(|id| crate::graph::supported(tcx.def_kind(*id)))
        .collect();
    let included: HashSet<_> = ids.iter().map(|id| id.to_def_id()).collect();
    let mut report = Report {
        candidates: ids.len(),
        encoded: 0,
        refused: 0,
        reasons: BTreeMap::new(),
        items: BTreeMap::new(),
        mono: MonoReport::default(),
    };
    let implementations = crate::graph::implementations(tcx);
    for id in ids {
        let result = Encoder::new(
            tcx,
            id,
            implementations
                .get(&id.to_def_id())
                .into_iter()
                .flatten()
                .copied()
                .collect(),
        )
        .audit()
        .and_then(|parts| {
            for mut referenced in parts.iter().flat_map(Part::references) {
                while referenced.is_local() && !included.contains(&referenced) {
                    let kind = tcx.def_kind(referenced);
                    if !matches!(kind, DefKind::Ctor(..) | DefKind::Variant | DefKind::Field) {
                        return Err(format!("reference to unsupported {kind:?}"));
                    }
                    referenced = tcx.parent(referenced);
                }
            }
            Ok(())
        });
        match result {
            Ok(()) => report.encoded += 1,
            Err(reason) => {
                report.refused += 1;
                *report.reasons.entry(reason.clone()).or_default() += 1;
                report
                    .items
                    .insert(crate::graph::item_path(tcx, id.to_def_id()), reason);
            }
        }
    }
    let document = (report.refused == 0).then(|| crate::graph::collect(tcx));
    let partitions = tcx.collect_and_partition_mono_items(());
    report.mono.cgus = partitions.codegen_units.len();
    let mut unique = HashSet::new();
    for cgu in partitions.codegen_units {
        for item in cgu.items().keys() {
            report.mono.placements += 1;
            let result = crate::mono::identity(tcx, *item, document.as_ref());
            if result.is_err() {
                report.mono.refused_placements += 1;
            }
            if unique.insert(*item) {
                report.mono.unique_items += 1;
                if let Ok(identity) = &result {
                    report.mono.hashes.insert(
                        format!("{item:?}"),
                        blake3::hash(&identity.bytes).to_hex().to_string(),
                    );
                }
                if let Err(reason) = result {
                    report.mono.refused_unique_items += 1;
                    report.mono.items.insert(format!("{item:?}"), reason);
                }
            }
        }
    }
    let bytes = serde_json::to_vec_pretty(&report).expect("coverage serialization");
    std::fs::write(path, bytes).unwrap_or_else(|error| {
        tcx.dcx()
            .fatal(format!("hash-rustc: coverage report: {error}"))
    });
    eprintln!(
        "item-coverage: candidates={} encoded={} refused={}",
        report.candidates, report.encoded, report.refused
    );
    eprintln!(
        "mono-coverage: cgus={} unique_items={} refused_unique_items={} placements={} refused_placements={}",
        report.mono.cgus,
        report.mono.unique_items,
        report.mono.refused_unique_items,
        report.mono.placements,
        report.mono.refused_placements
    );
}
