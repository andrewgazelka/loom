//! Executable admission for the typed effect wire protocol.
const CORE_HEADER: &[u8] = b"\0asm\x01\0\0\0";
const CORE_VERSION: &[u8] = b"core-handlers-v2";
const NAME: &[u8] = b"loom.effect-protocol";

fn integer(bytes: &mut &[u8]) -> Option<usize> {
    let mut result = 0u32;
    for shift in (0..35).step_by(7) {
        let (&byte, remaining) = bytes.split_first()?;
        *bytes = remaining;
        if shift == 28 && byte > 15 {
            return None;
        }
        result |= u32::from(byte & 127) << shift;
        if byte & 128 == 0 {
            return Some(result as usize);
        }
    }
    None
}

/// Requires exactly one current marker in a structurally bounded core module.
/// Wasmtime remains responsible for validating executable instructions.
pub fn is_current(component: &[u8]) -> bool {
    if !component.starts_with(CORE_HEADER) {
        return false;
    }
    let mut remaining = &component[8..];
    let mut found = false;
    while !remaining.is_empty() {
        let Some((&id, rest)) = remaining.split_first() else {
            return false;
        };
        remaining = rest;
        let Some(length) = integer(&mut remaining) else {
            return false;
        };
        let Some(section) = remaining.get(..length) else {
            return false;
        };
        remaining = &remaining[length..];
        if id != 0 {
            continue;
        }
        let mut content = section;
        let Some(length) = integer(&mut content) else {
            return false;
        };
        let Some(name) = content.get(..length) else {
            return false;
        };
        if name == NAME {
            if found || &content[length..] != CORE_VERSION {
                return false;
            }
            found = true;
        }
    }
    found
}

pub fn stamp(component: &mut Vec<u8>) {
    assert!(
        component.starts_with(CORE_HEADER),
        "expected core wasm module"
    );
    let version = CORE_VERSION;
    // This fixed custom section is shorter than 128 bytes, so both lengths
    // have a single-byte unsigned LEB128 representation.
    component.extend_from_slice(&[0, (1 + NAME.len() + version.len()) as u8, NAME.len() as u8]);
    component.extend_from_slice(NAME);
    component.extend_from_slice(version);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn component_model_is_rejected() {
        let mut core = CORE_HEADER.to_vec();
        stamp(&mut core);
        assert!(is_current(&core));
        core[..8].copy_from_slice(b"\0asm\x0d\0\x01\0");
        assert!(!is_current(&core));
    }
    #[test]
    fn previous_shared_core_requires_rebuild() {
        let mut old = CORE_HEADER.to_vec();
        let previous = b"core-shared-v1";
        old.extend_from_slice(&[0, (1 + NAME.len() + previous.len()) as u8, NAME.len() as u8]);
        old.extend_from_slice(NAME);
        old.extend_from_slice(previous);
        assert!(!is_current(&old));
    }
    #[test]
    fn admits_only_one_current_complete_marker() {
        let mut bytes = CORE_HEADER.to_vec();
        assert!(!is_current(&bytes));
        stamp(&mut bytes);
        assert!(is_current(&bytes));
        let mut old = bytes.clone();
        *old.last_mut().unwrap() = b'0';
        assert!(!is_current(&old));
        assert!(!is_current(&bytes[..bytes.len() - 1]));
        stamp(&mut bytes);
        assert!(!is_current(&bytes));
    }
}
