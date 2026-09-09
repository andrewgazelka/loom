use crate::Service;
use anyhow::{Context, Result};
use loom_proto::{CasInspectRequest, CasInspection, CasLink, DAG_CBOR_CODEC, Value};

const RAW_PREVIEW_BYTES: usize = 16 * 1024;
const DAG_DECODE_BYTES: usize = 1024 * 1024;
const VALUE_PREVIEW_BYTES: usize = 32 * 1024;
const HEX_PREVIEW_BYTES: usize = 256;
const MAX_LINKS: usize = 128;
const MAX_LINK_NODES: usize = 8192;
const MAX_PATH_BYTES: usize = 512;

impl Service {
    pub(crate) fn inspect_cas(&self, request: CasInspectRequest) -> Result<CasInspection> {
        let entry = self
            .store
            .cas_entry(&request.hash)?
            .context("CAS block not found")?;
        let selected = self
            .store
            .codec(&request.hash)?
            .context("CAS block not found")?;
        let codec = entry
            .codecs
            .iter()
            .find(|codec| codec.code == selected)
            .cloned()
            .context("CAS codec registration missing")?;
        let limit = if selected == DAG_CBOR_CODEC && entry.size <= DAG_DECODE_BYTES as u64 {
            DAG_DECODE_BYTES
        } else {
            RAW_PREVIEW_BYTES
        };
        let bytes = self
            .store
            .cas_prefix(&request.hash, limit)?
            .context("CAS block not found")?;
        let hex = bytes
            .iter()
            .take(HEX_PREVIEW_BYTES)
            .map(|byte| format!("{byte:02x}"))
            .collect();
        let mut inspection = CasInspection {
            entry,
            codec,
            value: None,
            links: Vec::new(),
            text: None,
            hex,
            truncated: false,
        };
        if selected == DAG_CBOR_CODEC {
            if inspection.entry.size > DAG_DECODE_BYTES as u64 {
                inspection.truncated = true;
                return Ok(inspection);
            }
            let value: Value = loom_proto::decode(&bytes).map_err(anyhow::Error::msg)?;
            let mut traversal = LinkTraversal {
                nodes_remaining: MAX_LINK_NODES,
                truncated: false,
            };
            collect_links(&value, "", &mut inspection.links, &mut traversal);
            if serde_json::to_vec(&value)?.len() <= VALUE_PREVIEW_BYTES {
                inspection.value = Some(value)
            } else {
                inspection.truncated = true;
            }
            inspection.truncated |= traversal.truncated;
        } else {
            let text = match std::str::from_utf8(&bytes) {
                Ok(text) => Some(text),
                Err(error) if error.error_len().is_none() => {
                    std::str::from_utf8(&bytes[..error.valid_up_to()]).ok()
                }
                Err(_) => None,
            };
            inspection.text = text
                .filter(|text| {
                    text.chars()
                        .all(|ch| !ch.is_control() || matches!(ch, '\n' | '\r' | '\t'))
                })
                .map(str::to_owned);
            inspection.truncated = inspection.entry.size
                > if inspection.text.is_some() {
                    RAW_PREVIEW_BYTES as u64
                } else {
                    HEX_PREVIEW_BYTES as u64
                };
        }
        Ok(inspection)
    }
}
struct LinkTraversal {
    nodes_remaining: usize,
    truncated: bool,
}
fn collect_links(
    value: &Value,
    path: &str,
    links: &mut Vec<CasLink>,
    traversal: &mut LinkTraversal,
) {
    if traversal.nodes_remaining == 0 {
        traversal.truncated = true;
        return;
    }
    traversal.nodes_remaining -= 1;
    if let Some(object) = value.as_object() {
        if object.len() == 1
            && let Some(cid) = object.get("$ref").and_then(Value::as_str)
        {
            if links.len() < MAX_LINKS {
                links.push(CasLink {
                    path: path.into(),
                    cid: cid.into(),
                })
            } else {
                traversal.truncated = true;
            }
            return;
        }
        for entry in object {
            let child = child_path(
                path,
                &entry.0.replace('~', "~0").replace('/', "~1"),
                traversal,
            );
            collect_links(entry.1, &child, links, traversal);
            if traversal.nodes_remaining == 0 {
                traversal.truncated = true;
                break;
            }
        }
    } else if let Some(array) = value.as_array() {
        for entry in array.iter().enumerate() {
            let child = child_path(path, &entry.0.to_string(), traversal);
            collect_links(entry.1, &child, links, traversal);
            if traversal.nodes_remaining == 0 {
                traversal.truncated = true;
                break;
            }
        }
    }
}
fn child_path(parent: &str, segment: &str, traversal: &mut LinkTraversal) -> String {
    let mut path = format!("{parent}/{segment}");
    if path.len() > MAX_PATH_BYTES {
        let mut end = MAX_PATH_BYTES;
        while !path.is_char_boundary(end) {
            end -= 1;
        }
        path.truncate(end);
        traversal.truncated = true;
    }
    path
}
