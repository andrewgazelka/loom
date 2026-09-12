//! The resolved driver output is the single authority for residual host rows.
use crate::{CheckError, CheckedDef, diagnostic};
use loom_proto::EffectSet;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnknownEffect {
    pub item: String,
    pub span: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverEffectRow {
    pub labels: Vec<String>,
    pub unknown: Vec<UnknownEffect>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverEffects {
    /// Keys match exported entry names. Each row unions its concrete instances.
    pub entries: BTreeMap<String, DriverEffectRow>,
    pub instances: BTreeMap<String, DriverEffectRow>,
}

impl CheckedDef {
    /// Finalize source checking with the complete rustc driver's JSON object.
    ///
    /// Source checking leaves rows pending (`unknown = true`). The build must
    /// call this before admitting executable code; missing rows are errors.
    /// Unrelated driver fields are ignored so this contract can evolve independently.
    pub fn apply_driver_effects_json(&mut self, json: &str) -> Result<(), CheckError> {
        #[derive(Deserialize)]
        struct Output {
            effects: DriverEffects,
        }
        let output: Output = serde_json::from_str(json)?;
        self.apply_driver_effects(&output.effects)
    }

    pub fn apply_driver_effects(&mut self, effects: &DriverEffects) -> Result<(), CheckError> {
        if self.sig.exports.is_empty() {
            return Err(CheckError::Effects(
                "definition has no exported entry".into(),
            ));
        }
        // Resolve all keys before changing any signature or diagnostics.
        let mut rows = BTreeMap::new();
        for export in &self.sig.exports {
            let row = if let Some(row) = effects.entries.get(&export.name) {
                row
            } else {
                let suffix = format!("::{}", export.name);
                let mut matches = effects
                    .entries
                    .iter()
                    .filter(|(name, _)| name.ends_with(&suffix));
                let Some((_, row)) = matches.next() else {
                    return Err(CheckError::Effects(format!(
                        "driver omitted effect row for entry {}",
                        export.name
                    )));
                };
                if matches.next().is_some() {
                    return Err(CheckError::Effects(format!(
                        "ambiguous driver effect rows for entry {}",
                        export.name
                    )));
                }
                row
            };
            rows.insert(export.name.clone(), row);
        }
        self.diagnostics
            .retain(|error| error.code != "LOOM_EFFECT_INFERENCE");
        let mut aggregate = BTreeSet::new();
        let mut unknown = false;
        for export in &mut self.sig.exports {
            let row = rows[&export.name];
            let labels: BTreeSet<_> = row.labels.iter().cloned().collect();
            for site in &row.unknown {
                let mut error = diagnostic(
                    self.lang,
                    "LOOM_EFFECT_INFERENCE",
                    &format!(
                        "{}: perform label must be a string literal or a const at {} ({})",
                        export.name, site.span, site.item
                    ),
                );
                if let Some((file_line, col)) = site.span.rsplit_once(':')
                    && let Some((file, line)) = file_line.rsplit_once(':')
                    && let (Ok(line), Ok(col)) = (line.parse(), col.parse())
                {
                    error.file = file.into();
                    error.line = line;
                    error.col = col;
                }
                self.diagnostics.push(error);
            }
            export.effects.declared = None;
            export.effects.labels = labels.iter().cloned().collect();
            export.effects.unknown = !row.unknown.is_empty();
            aggregate.extend(labels);
            unknown |= export.effects.unknown;
        }
        self.sig.effects = EffectSet {
            labels: aggregate.into_iter().collect(),
            unknown,
            declared: None,
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests;
