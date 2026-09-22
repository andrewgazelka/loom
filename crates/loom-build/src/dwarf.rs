//! Relocate a module's DWARF after `threaded_module` rewrites its code section.
//!
//! DWARF for WebAssembly addresses an instruction by its byte offset from the
//! start of the code section's contents: the byte after the section size,
//! where the function count begins. That is where wasmparser's
//! `CodeSectionStart` range starts and where `rust-lld` counts function
//! offsets from. `threaded_module::prepare` replaces single instructions with
//! longer sequences, so every address at or after the first replacement moves.
//! This module reads the compiler's `.debug_*` sections with gimli, moves every
//! line-program row, `DW_AT_low_pc`/`DW_AT_high_pc` pair and range-list entry
//! through the [`CodeMap`] the rewrite recorded, and writes the sections back.
//!
//! It fails closed. An address inside a replaced instruction, in a body's size
//! prefix, or past the last body, a `.debug_*` section gimli does not read, or
//! any construct gimli cannot write rejects the module instead of shipping
//! stale addresses.
use gimli::write::{Address, AttributeValue, ConvertLineRow, EndianVec, Sections};
use gimli::{EndianSlice, LittleEndian, SectionId};
use std::collections::BTreeMap;
use std::ops::Range;

type Slice<'a> = EndianSlice<'a, LittleEndian>;

/// One instruction `threaded_module::prepare_body` replaced, in bytes
/// relative to the function body's first byte (its locals vector).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Edit {
    pub start: u64,
    pub old_len: u64,
    pub new_len: u64,
}

/// One function body before and after the rewrite. Both ranges are relative
/// to the start of their code section's contents.
#[derive(Clone, Debug)]
pub(crate) struct Body {
    pub old: Range<u64>,
    pub new: Range<u64>,
    /// In body order, as the rewrite met them.
    pub edits: Vec<Edit>,
}

/// wasm-ld's tombstone addresses for dead-stripped code, in the 32-bit form
/// the linker writes and the 64-bit form a reader may widen them to.
const TOMBSTONES: [u64; 4] = [0xffff_ffff, 0xffff_fffe, u64::MAX, u64::MAX - 1];

/// Old-to-new address map for the code section, one entry per body in order.
pub(crate) struct CodeMap {
    bodies: Vec<Body>,
}

impl CodeMap {
    pub(crate) fn new(bodies: Vec<Body>) -> Self {
        Self { bodies }
    }

    /// The new address of `address`, or `None` when no instruction boundary of
    /// the old module has it: inside a replaced instruction, in the size prefix
    /// before a body, or past the last body. Address 0 is the compilation unit's
    /// `DW_AT_low_pc` anchor for its range list and maps to 0. A body's end (its
    /// `DW_AT_high_pc` and the end of its line sequence) maps to the new end.
    pub(crate) fn translate(&self, address: u64) -> Option<u64> {
        if address == 0 {
            return Some(0);
        }
        // wasm-ld marks code it dead-stripped with tombstones instead of
        // deleting the DWARF that described it: -1 in `.debug_ranges`,
        // `.debug_rnglists` and `DW_AT_low_pc`, -2 for `.debug_line` sequences
        // (lld/wasm/Relocations.cpp). A real module carries thousands. They are
        // not addresses in this module and stay exactly as written.
        if TOMBSTONES.contains(&address) {
            return Some(address);
        }
        let index = self
            .bodies
            .partition_point(|body| body.old.start <= address);
        let body = self.bodies.get(index.checked_sub(1)?)?;
        if address > body.old.end {
            return None;
        }
        let local = address - body.old.start;
        let mut delta: i128 = 0;
        for edit in &body.edits {
            if local >= edit.start + edit.old_len {
                delta += i128::from(edit.new_len) - i128::from(edit.old_len);
            } else if local > edit.start {
                return None;
            } else {
                break;
            }
        }
        u64::try_from(i128::from(body.new.start) + i128::from(local) + delta).ok()
    }
}

/// The `.debug_*` sections gimli reads. Anything else is refused rather than
/// dropped or copied with stale addresses. `.debug_aranges` is read and not
/// written back: it is an address index over `.debug_info` that gimli
/// regenerates nothing for and that no consumer of these modules needs.
const READABLE: [&str; 12] = [
    ".debug_abbrev",
    ".debug_addr",
    ".debug_aranges",
    ".debug_info",
    ".debug_line",
    ".debug_line_str",
    ".debug_str",
    ".debug_str_offsets",
    ".debug_loc",
    ".debug_loclists",
    ".debug_ranges",
    ".debug_rnglists",
];

/// Convert every `.debug_*` section in `sections` through `map`. Returns the
/// rewritten sections by name in gimli's section order, omitting empty ones.
pub(crate) fn relocate<'a>(
    sections: &BTreeMap<&'a str, &'a [u8]>,
    map: &CodeMap,
) -> Result<Vec<(&'static str, Vec<u8>)>, String> {
    if let Some(name) = sections.keys().find(|name| !READABLE.contains(name)) {
        return Err(format!("DWARF section {name} cannot be relocated"));
    }
    let empty: &'a [u8] = &[];
    let dwarf = gimli::Dwarf::load(|id: SectionId| -> Result<Slice<'a>, gimli::Error> {
        Ok(EndianSlice::new(
            sections.get(id.name()).copied().unwrap_or(empty),
            LittleEndian,
        ))
    })
    .map_err(|error| format!("DWARF sections: {error}"))?;
    let convert_address = |address: u64| map.translate(address).map(Address::Constant);
    let mut converted = gimli::write::Dwarf::new();
    {
        let mut units = converted.convert(&dwarf).map_err(convert_error)?;
        while let Some((mut unit, root)) = units.read_unit().map_err(convert_error)? {
            if let Some(mut program) = unit.read_line_program(None, None).map_err(convert_error)? {
                // gimli converts `DW_LNE_set_address` through `convert_address`
                // but keeps row and end-of-sequence offsets as deltas, which is
                // wrong once bytes are inserted inside a body: every row is
                // re-addressed here against the relocated sequence start.
                let (mut old_base, mut new_base) = (0u64, 0u64);
                while let Some(row) = program.read_row().map_err(convert_error)? {
                    match row {
                        ConvertLineRow::SetAddress(address) => {
                            old_base = address;
                            new_base = translate(map, address)?;
                            program.set_address(Address::Constant(new_base));
                        }
                        ConvertLineRow::Row(mut row) => {
                            row.address_offset = span(map, old_base, new_base, row.address_offset)?;
                            program.generate_row(row);
                        }
                        ConvertLineRow::EndSequence(length) => {
                            program.end_sequence(span(map, old_base, new_base, length)?);
                        }
                    }
                }
                let (program, files) = program.program();
                unit.set_line_program(program, files);
            }
            let root_id = unit.unit.root();
            convert_entry(&mut unit, root_id, &root, map, &convert_address)?;
            let mut entry = root;
            while let Some(id) = unit.read_entry(&mut entry).map_err(convert_error)? {
                let id = unit.add_entry(id, &entry);
                convert_entry(&mut unit, id, &entry, map, &convert_address)?;
            }
        }
    }
    let mut output = Sections::new(EndianVec::new(LittleEndian));
    converted
        .write(&mut output)
        .map_err(|error| format!("DWARF write: {error}"))?;
    let mut written = Vec::new();
    output
        .for_each(|id, data| {
            if !data.slice().is_empty() {
                written.push((id.name(), data.slice().to_vec()));
            }
            Ok::<(), gimli::write::Error>(())
        })
        .map_err(|error| format!("DWARF write: {error}"))?;
    Ok(written)
}

/// Convert one DIE's attributes. gimli converts address-class values through
/// `convert_address` but leaves constant-class values alone, and a
/// `DW_AT_high_pc` in a constant class is a length from `DW_AT_low_pc` that
/// grows with every replacement inside the function; it is recomputed from the
/// relocated bounds.
fn convert_entry<'u, 'a>(
    unit: &mut gimli::write::ConvertUnit<'u, Slice<'a>>,
    id: gimli::write::UnitEntryId,
    entry: &gimli::write::ConvertUnitEntry<'u, Slice<'a>>,
    map: &CodeMap,
    convert_address: &dyn Fn(u64) -> Option<Address>,
) -> Result<(), String> {
    let low_pc =
        entry
            .attrs
            .iter()
            .find_map(|attribute| match (attribute.name(), attribute.value()) {
                (gimli::DW_AT_low_pc, gimli::AttributeValue::Addr(address)) => Some(address),
                _ => None,
            });
    for attribute in &entry.attrs {
        let value = match (attribute.name(), attribute.udata_value(), low_pc) {
            (gimli::DW_AT_high_pc, Some(length), Some(low)) => {
                AttributeValue::Udata(span(map, low, translate(map, low)?, length)?)
            }
            _ => unit
                .convert_attribute_value(entry.read_unit, attribute, convert_address)
                .map_err(convert_error)?,
        };
        unit.unit.get_mut(id).set(attribute.name(), value);
    }
    Ok(())
}

fn translate(map: &CodeMap, address: u64) -> Result<u64, String> {
    map.translate(address).ok_or_else(|| {
        format!("DWARF names code offset {address:#x}, which is not an instruction boundary")
    })
}

/// The relocated distance of `old_base + length` from `new_base`, where
/// `new_base` is `old_base` relocated: row offsets, sequence lengths and
/// `DW_AT_high_pc` lengths all take this shape.
fn span(map: &CodeMap, old_base: u64, new_base: u64, length: u64) -> Result<u64, String> {
    // A dead-stripped sequence or range starts at a linker tombstone; its rows
    // and length describe code that no longer exists and stay as written.
    if TOMBSTONES.contains(&old_base) {
        return Ok(length);
    }
    let end = translate(map, old_base + length)?;
    end.checked_sub(new_base).ok_or_else(|| {
        format!(
            "DWARF range {old_base:#x}+{length:#x} relocates to {end:#x}, before its start {new_base:#x}"
        )
    })
}

fn convert_error(error: gimli::write::ConvertError) -> String {
    format!("DWARF conversion: {error}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> CodeMap {
        // Two bodies: the first grows by 18 bytes at its `global.set`, the
        // second is untouched but moves because the first grew (and its size
        // prefix went from one to two bytes).
        CodeMap::new(vec![
            Body {
                old: 3..9,
                new: 3..27,
                edits: vec![Edit {
                    start: 3,
                    old_len: 2,
                    new_len: 20,
                }],
            },
            Body {
                old: 10..14,
                new: 29..33,
                edits: Vec::new(),
            },
        ])
    }

    #[test]
    fn addresses_move_by_the_bytes_inserted_before_them() {
        let map = map();
        assert_eq!(map.translate(0), Some(0), "unit anchor");
        assert_eq!(map.translate(3), Some(3), "first body start");
        assert_eq!(map.translate(4), Some(4), "before the edit");
        assert_eq!(
            map.translate(6),
            Some(6),
            "the replaced instruction's start"
        );
        assert_eq!(map.translate(8), Some(26), "after the edit");
        assert_eq!(map.translate(9), Some(27), "first body end");
        assert_eq!(map.translate(10), Some(29), "second body start");
        assert_eq!(map.translate(14), Some(33), "second body end");
    }

    #[test]
    fn linker_tombstones_pass_through_unchanged() {
        let map = map();
        for tombstone in [0xffff_ffff_u64, 0xffff_fffe, u64::MAX, u64::MAX - 1] {
            assert_eq!(map.translate(tombstone), Some(tombstone));
        }
        // An ordinary address past the last body is still refused.
        let last_end = map.bodies.last().unwrap().old.end;
        assert_eq!(map.translate(last_end + 1), None);
    }

    #[test]
    fn addresses_that_are_not_instruction_boundaries_are_refused() {
        let map = map();
        assert_eq!(map.translate(7), None, "inside the replaced instruction");
        assert_eq!(map.translate(2), None, "size prefix before the first body");
        assert_eq!(map.translate(15), None, "past the last body");
        assert_eq!(CodeMap::new(Vec::new()).translate(1), None);
        assert_eq!(CodeMap::new(Vec::new()).translate(0), Some(0));
    }

    #[test]
    fn unreadable_debug_sections_are_refused_by_name() {
        let bytes: &[u8] = b"";
        let sections = BTreeMap::from([(".debug_frame", bytes)]);
        let error = relocate(&sections, &CodeMap::new(Vec::new())).unwrap_err();
        assert!(error.contains(".debug_frame"), "{error}");
    }
}
