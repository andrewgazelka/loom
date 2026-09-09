use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CrateDependency {
    pub hash: String,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default = "enabled")]
    pub default_features: bool,
}
fn enabled() -> bool {
    true
}

pub fn crate_dependencies(source: &str) -> Result<BTreeMap<String, CrateDependency>, String> {
    let manifest: toml::Value = source
        .parse()
        .map_err(|error: toml::de::Error| error.to_string())?;
    let Some(crates) = manifest.get("loom").and_then(|loom| loom.get("crates")) else {
        return Ok(BTreeMap::new());
    };
    let deps: BTreeMap<String, CrateDependency> = crates
        .clone()
        .try_into()
        .map_err(|error: toml::de::Error| error.to_string())?;
    for (alias, dep) in &deps {
        if alias.is_empty()
            || !alias
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            || alias.as_bytes()[0].is_ascii_digit()
        {
            return Err(format!("invalid crate alias {alias}"));
        }
        if dep.hash.len() != 64
            || !dep
                .hash
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(format!(
                "crate {alias} requires a lowercase 64-digit content hash"
            ));
        }
    }
    Ok(deps)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_content_pins() {
        let hash = "a".repeat(64);
        let deps = crate_dependencies(&format!(
            "[loom.crates]\nserde={{hash='{hash}',features=['derive']}}"
        ))
        .unwrap();
        assert_eq!(deps["serde"].features, ["derive"]);
        assert!(crate_dependencies("[loom.crates]\nserde={hash='1.0'}").is_err());
        assert!(
            crate_dependencies(&format!(
                "[loom.crates]\nserde={{hash='{hash}',version='1'}}"
            ))
            .is_err()
        );
    }
}
