//! Immutable module payloads are protocol data; reading them needs no compiler.
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MAX_ARTIFACT_BYTES: usize = 16 * 1024 * 1024;
pub const DIRECT_COMPILER: &str = "deno_ast=0.53.3;script=1";
pub const BUNDLE_COMPILER: &str = "deno=2.9.6;esbuild=0.25.5;browser-iife=1";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptArtifact {
    pub version: u32,
    pub source: String,
    pub language: String,
    pub compiler: String,
    pub javascript: String,
    pub lock: Value,
    pub import_origins: Vec<String>,
    /// Includes sourcesContent: the actual dependency bytes, not just URLs.
    pub source_map: Value,
}

#[derive(Debug)]
pub struct ScriptArtifactError(&'static str);
impl std::fmt::Display for ScriptArtifactError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.0)
    }
}
impl std::error::Error for ScriptArtifactError {}

impl ScriptArtifact {
    /// Validate the envelope. Admission parses JavaScript before publishing;
    /// stores verify the artifact's content address and definition identity.
    pub fn validate(&self) -> Result<(), ScriptArtifactError> {
        let check = |condition, reason| {
            if condition {
                Ok(())
            } else {
                Err(ScriptArtifactError(reason))
            }
        };
        check(self.version == 1, "unsupported script artifact version")?;
        check(
            matches!(self.language.as_str(), "javascript" | "typescript"),
            "unsupported script artifact language",
        )?;
        check(
            matches!(self.compiler.as_str(), DIRECT_COMPILER | BUNDLE_COMPILER),
            "unsupported script compiler identity",
        )?;
        let bytes = serde_json::to_vec(self)
            .map_err(|_| ScriptArtifactError("script artifact serialization failed"))?;
        check(
            bytes.len() <= MAX_ARTIFACT_BYTES,
            "script artifact exceeds size limit",
        )?;
        if self.compiler == BUNDLE_COMPILER {
            check(self.lock.is_object(), "bundle lock is missing")?;
            let sources = self.source_map["sources"]
                .as_array()
                .ok_or(ScriptArtifactError("bundle sources missing"))?;
            let contents = self.source_map["sourcesContent"]
                .as_array()
                .ok_or(ScriptArtifactError("bundle source contents missing"))?;
            check(
                sources.len() == contents.len()
                    && sources.iter().all(Value::is_string)
                    && contents.iter().all(Value::is_string),
                "incomplete bundled source graph",
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn artifact() -> ScriptArtifact {
        ScriptArtifact {
            version: 1,
            source: "export function main(){}".into(),
            language: "typescript".into(),
            compiler: BUNDLE_COMPILER.into(),
            javascript: "globalThis.main=()=>null".into(),
            lock: json!({"version":"5"}),
            import_origins: Vec::new(),
            source_map: json!({"sources":["loom:///main.ts"],"sourcesContent":["export function main(){}"]}),
        }
    }
    #[test]
    fn validates_complete_envelope_and_rejects_unknown_identity() {
        let mut value = artifact();
        assert!(value.validate().is_ok());
        value.compiler = "unknown".into();
        assert!(value.validate().is_err());
        value = artifact();
        value.version = 2;
        assert!(value.validate().is_err());
    }
    #[test]
    fn rejects_incomplete_graph_and_lock() {
        let mut value = artifact();
        value.source_map["sourcesContent"] = json!([]);
        assert!(value.validate().is_err());
        value = artifact();
        value.source_map["sources"][0] = Value::Null;
        assert!(value.validate().is_err());
        value = artifact();
        value.lock = Value::Null;
        assert!(value.validate().is_err());
    }
}
