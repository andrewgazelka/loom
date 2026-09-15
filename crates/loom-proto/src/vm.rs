//! Typed CAS filesystem and launch contract for isolated Linux guests.
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CasReference {
    #[serde(rename = "$ref")]
    pub reference: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum VmImageFormat {
    #[serde(rename = "loom.vm.rootfs.v1")]
    RootfsV1,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum VmArchitecture {
    #[serde(rename = "x86_64")]
    X86_64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct VmImageManifest {
    pub format: VmImageFormat,
    pub arch: VmArchitecture,
    pub entries: BTreeMap<String, VmImageEntry>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum VmImageEntry {
    File { reference: CasReference, mode: u32 },
    Directory { mode: u32 },
    Symlink { target: String },
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VmNetwork {
    #[default]
    None,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VmSpec {
    pub image: CasReference,
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_cwd")]
    pub cwd: String,
    #[serde(default = "default_memory")]
    pub memory_mb: u64,
    #[serde(default = "default_cpus")]
    pub cpus: u32,
    #[serde(default = "default_rootfs")]
    pub rootfs_mb: u64,
    #[serde(default = "default_ttl")]
    pub ttl_ms: u64,
    #[serde(default)]
    pub network: VmNetwork,
}
fn default_cwd() -> String {
    "/".into()
}
fn default_memory() -> u64 {
    512
}
fn default_cpus() -> u32 {
    1
}
fn default_rootfs() -> u64 {
    512
}
fn default_ttl() -> u64 {
    3_600_000
}
impl VmSpec {
    pub fn validate(&self) -> Result<(), String> {
        let image = crate::parse_reference(&self.image.reference)?;
        if image.codec != crate::DAG_CBOR_CODEC {
            return Err("VM image requires a DAG-CBOR reference".into());
        }
        if !self.command.starts_with('/')
            || !self.cwd.starts_with('/')
            || self.command.contains('\0')
            || self.cwd.contains('\0')
        {
            return Err("VM command and cwd must be absolute guest paths".into());
        }
        if !(64..=32768).contains(&self.memory_mb)
            || !(1..=8).contains(&self.cpus)
            || !(16..=32768).contains(&self.rootfs_mb)
            || !(1..=86_400_000).contains(&self.ttl_ms)
        {
            return Err("VM resource limits out of range".into());
        }
        if self.args.len() > 256
            || self.env.len() > 128
            || self
                .env
                .keys()
                .any(|key| key.is_empty() || key.contains(['=', '\0']))
            || self
                .args
                .iter()
                .chain(self.env.values())
                .any(|value| value.len() > 65536 || value.contains('\0'))
        {
            return Err("invalid VM arguments or environment".into());
        }
        Ok(())
    }
}

/// Host mount paths and the same validated guest specification used by actors.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VmLaunch {
    pub source_root: String,
    pub guest_root: String,
    #[serde(flatten)]
    pub spec: VmSpec,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn vm_wire_format_and_resource_bounds_are_shared() {
        let cid = crate::cid_for_hash(&"a".repeat(64), crate::DAG_CBOR_CODEC).unwrap();
        let mut spec: VmSpec = serde_json::from_value(serde_json::json!({
            "image":{"$ref":cid},"command":"/bin/main","memoryMb":256,"rootfsMb":128,"ttlMs":3000,"network":"none"
        })).unwrap();
        spec.validate().unwrap();
        let launch = VmLaunch {
            source_root: "/base".into(),
            guest_root: "/guest".into(),
            spec: spec.clone(),
        };
        let wire = serde_json::to_value(&launch).unwrap();
        assert_eq!(wire["sourceRoot"], "/base");
        assert_eq!(wire["memoryMb"], 256);
        assert!(wire.get("spec").is_none());
        spec.cpus = 0;
        assert!(spec.validate().is_err());
        spec.cpus = 1;
        spec.command = "relative".into();
        assert!(spec.validate().is_err());
        let mut network = wire;
        network["network"] = serde_json::json!("host");
        assert!(serde_json::from_value::<VmLaunch>(network).is_err());
    }
}
