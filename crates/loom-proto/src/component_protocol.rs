//! Executable admission for the typed effect wire protocol.
const HEADER: &[u8] = b"\0asm\x0d\0\x01\0";
const CORE_HEADER: &[u8] = b"\0asm\x01\0\0\0";
const CORE_VERSION: &[u8] = b"core-shared-v1";
const NAME: &[u8] = b"loom.effect-protocol";
const VERSION: &[u8] = b"typed-fs-list-v1";

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

/// Requires exactly one current marker in a structurally bounded component.
/// Wasmtime remains responsible for validating executable instructions.
pub fn is_current(component: &[u8]) -> bool {
    let version = if component.starts_with(HEADER) {
        VERSION
    } else if component.starts_with(CORE_HEADER) {
        CORE_VERSION
    } else {
        return false;
    };
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
            if found || &content[length..] != version {
                return false;
            }
            found = true;
        }
    }
    found
}

/// Stamp freshly built components, never preexisting cached artifacts.
pub fn is_core_current(bytes: &[u8]) -> bool {
    bytes.starts_with(CORE_HEADER) && is_current(bytes)
}

pub fn stamp(component: &mut Vec<u8>) {
    let version = if component.starts_with(CORE_HEADER) {
        CORE_VERSION
    } else {
        VERSION
    };
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
    fn core_and_component_protocols_are_distinct() {
        let mut core = CORE_HEADER.to_vec();
        stamp(&mut core);
        assert!(is_core_current(&core));
        let mut wrong_kind = core.clone();
        wrong_kind[..8].copy_from_slice(HEADER);
        assert!(!is_current(&wrong_kind));
        let mut component = HEADER.to_vec();
        stamp(&mut component);
        assert!(is_current(&component));
        assert!(!is_core_current(&component));
    }
    #[test]
    fn admits_only_one_current_complete_marker() {
        let mut bytes = HEADER.to_vec();
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
