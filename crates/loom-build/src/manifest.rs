use super::*;

pub(super) fn validate_manifest(value: &toml::Value, isolated: bool) -> Result<(), BuildError> {
    if let Some(table) = value.as_table() {
        for (name, value) in table {
            if ["patch", "replace"].contains(&name.as_str()) {
                return Err(BuildError::Rejected(format!(
                    "Cargo [{name}] overrides are unavailable"
                )));
            }
            if ["dependencies", "build-dependencies", "dev-dependencies"].contains(&name.as_str()) {
                if let Some(deps) = value.as_table() {
                    for (name, dependency) in deps {
                        if dependency.get("path").is_some()
                            || dependency.get("git").is_some()
                            || dependency.get("registry").is_some()
                        {
                            return Err(BuildError::Rejected(format!(
                                "{name}: use locked crates.io or loom.deps, not path/git"
                            )));
                        }
                        if !isolated && !trusted_dependency(name, dependency) {
                            return Err(BuildError::Rejected(format!(
                                "{name} requires the isolated vendored build worker"
                            )));
                        }
                    }
                }
            } else {
                validate_manifest(value, isolated)?;
            }
        }
    }
    Ok(())
}
