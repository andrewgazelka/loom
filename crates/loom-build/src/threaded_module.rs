//! Lower only LLVM's one-time shared-memory initializer for serialized startup.
//! All task code must remain free of Wasm waits and notifications.
//!
//! The rewrite lengthens function bodies, so the compiler's DWARF (`.debug_*`
//! custom sections, addressed by offset into the code section's contents) is
//! relocated through `crate::dwarf` rather than copied; every other custom
//! section (`name`, `producers`, `target_features`) is copied byte for byte.
use crate::dwarf::{self, Body, CodeMap, Edit};
use std::collections::BTreeMap;
use wasm_encoder::{
    CodeSection, ConstExpr, Encode, ExportKind, ExportSection, GlobalSection, GlobalType,
    Instruction, Module, RawSection, Section, ValType,
};
use wasmparser::{BlockType, ExternalKind, FunctionBody, Operator, Parser, Payload, TypeRef};

struct StackBounds {
    pointer: u32,
    low: u32,
    high: u32,
    initial_low: i32,
    initial_high: i32,
}
struct GlobalDefinition {
    start: usize,
    initial: Option<i32>,
}
struct Export {
    name: String,
    kind: ExternalKind,
    index: u32,
}

/// Lower the compiler's core module for serialized startup: export stack
/// bounds, trap on stack overflow, and replace the linker initializer's
/// wait/notify pair. The input is parsed, not validated: it comes from the
/// pinned compiler, and the one validation pass runs over the result, which
/// fails closed on anything malformed in either the input or the rewrite.
pub fn prepare(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let mut imports = 0u32;
    let mut start = None;
    let mut imported_globals = 0u32;
    let mut global_definitions = Vec::new();
    let mut globals_end = 0;
    let mut exports = Vec::new();
    let mut bodies = Vec::new();
    let mut code_start = None;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|error| error.to_string())? {
            Payload::CodeSectionStart { range, .. } => code_start = Some(range.start),
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    match import.map_err(|error| error.to_string())?.ty {
                        TypeRef::Func(_) | TypeRef::FuncExact(_) => imports += 1,
                        TypeRef::Global(_) => imported_globals += 1,
                        _ => {}
                    }
                }
            }
            Payload::GlobalSection(section) => {
                globals_end = section.range().end;
                for global in section.into_iter_with_offsets() {
                    let (offset, global) = global.map_err(|error| error.to_string())?;
                    let mut initializer = global.init_expr.get_operators_reader();
                    let initial = match initializer.read().map_err(|error| error.to_string())? {
                        Operator::I32Const { value } => Some(value),
                        _ => None,
                    };
                    global_definitions.push(GlobalDefinition {
                        start: offset,
                        initial,
                    });
                }
            }
            Payload::ExportSection(section) => {
                for export in section {
                    let export = export.map_err(|error| error.to_string())?;
                    exports.push(Export {
                        name: export.name.into(),
                        kind: export.kind,
                        index: export.index,
                    });
                }
            }
            Payload::StartSection { func, .. } => start = Some(func),
            Payload::CodeSectionEntry(body) => bodies.push(body),
            _ => {}
        }
    }
    let stack = if let Some(pointer) = exports
        .iter()
        .find(|export| export.name == "__stack_pointer")
    {
        if exports.iter().any(|export| {
            matches!(
                export.name.as_str(),
                "__loom_stack_low" | "__loom_stack_high"
            )
        }) {
            return Err("shared stack bounds already exist".into());
        }
        let initial = |export: &Export| -> Result<i32, String> {
            if export.kind != ExternalKind::Global {
                return Err("stack bound must export a global".into());
            }
            export
                .index
                .checked_sub(imported_globals)
                .and_then(|index| global_definitions.get(index as usize))
                .and_then(|global| global.initial)
                .ok_or_else(|| "stack bound must have an i32 constant initializer".into())
        };
        let low = exports
            .iter()
            .find(|export| export.name == "__stack_low")
            .ok_or("missing linker __stack_low export")?;
        let initial_low = initial(low)?;
        let initial_high = initial(pointer)?;
        if initial_low as u32 >= initial_high as u32 {
            return Err("invalid linker stack bounds".into());
        }
        Some(StackBounds {
            pointer: pointer.index,
            low: imported_globals + global_definitions.len() as u32,
            high: imported_globals + global_definitions.len() as u32 + 1,
            initial_low,
            initial_high,
        })
    } else {
        None
    };
    let mut globals = GlobalSection::new();
    let mut exported = ExportSection::new();
    if let Some(stack) = &stack {
        for (index, global) in global_definitions.iter().enumerate() {
            let end = global_definitions
                .get(index + 1)
                .map_or(globals_end, |next| next.start);
            globals.raw(&bytes[global.start..end]);
        }
        let ty = GlobalType {
            val_type: ValType::I32,
            mutable: true,
            shared: false,
        };
        globals.global(ty, &ConstExpr::i32_const(stack.initial_low));
        globals.global(ty, &ConstExpr::i32_const(stack.initial_high));
        for export in &exports {
            let kind = match export.kind {
                ExternalKind::Func => ExportKind::Func,
                ExternalKind::Table => ExportKind::Table,
                ExternalKind::Memory => ExportKind::Memory,
                ExternalKind::Global => ExportKind::Global,
                ExternalKind::Tag => ExportKind::Tag,
                _ => return Err("unsupported shared core export kind".into()),
            };
            exported.export(&export.name, kind, export.index);
        }
        exported.export("__loom_stack_low", ExportKind::Global, stack.low);
        exported.export("__loom_stack_high", ExportKind::Global, stack.high);
    }
    let mut code = CodeSection::new();
    let mut edits = Vec::with_capacity(bodies.len());
    for (index, body) in bodies.iter().enumerate() {
        let function = imports + index as u32;
        let (rewritten, body_edits) = prepare_body(body, Some(function) == start, stack.as_ref())?;
        code.raw(&rewritten);
        edits.push(body_edits);
    }
    let mut module = Module::new();
    let mut debug: BTreeMap<&str, &[u8]> = BTreeMap::new();
    for payload in Parser::new(0).parse_all(bytes) {
        let payload = payload.map_err(|error| error.to_string())?;
        match payload {
            Payload::GlobalSection(_) if stack.is_some() => {
                module.section(&globals);
            }
            Payload::ExportSection(_) if stack.is_some() => {
                module.section(&exported);
            }
            Payload::CodeSectionStart { .. } => {
                module.section(&code);
            }
            Payload::CodeSectionEntry(_) => {}
            Payload::CustomSection(section) if section.name().starts_with(".debug_") => {
                if debug.insert(section.name(), section.data()).is_some() {
                    return Err(format!("duplicate DWARF section {}", section.name()));
                }
            }
            _ => {
                if let Some((id, range)) = payload.as_section() {
                    module.section(&RawSection {
                        id,
                        data: &bytes[range],
                    });
                }
            }
        }
    }
    let mut result = module.finish();
    if !debug.is_empty() {
        let code_start = code_start.ok_or("module carries DWARF but has no code section")?;
        let (new_start, new_bodies) = code_layout(&result)?;
        if new_bodies.len() != bodies.len() {
            return Err(format!(
                "rewrite produced {} function bodies from {}",
                new_bodies.len(),
                bodies.len()
            ));
        }
        let relative = |range: std::ops::Range<usize>, start: usize| {
            (range.start - start) as u64..(range.end - start) as u64
        };
        let map = CodeMap::new(
            bodies
                .iter()
                .zip(new_bodies)
                .zip(edits)
                .map(|((old, new), edits)| Body {
                    old: relative(old.range(), code_start),
                    new: relative(new, new_start),
                    edits,
                })
                .collect(),
        );
        for (name, data) in dwarf::relocate(&debug, &map)? {
            wasm_encoder::CustomSection {
                name: name.into(),
                data: data.into(),
            }
            .append_to(&mut result);
        }
    }
    wasmparser::Validator::new()
        .validate_all(&result)
        .map_err(|error| error.to_string())?;
    Ok(result)
}

/// The code section's contents offset and every function body's byte range in
/// `bytes`, the two quantities DWARF addresses are relative to.
fn code_layout(bytes: &[u8]) -> Result<(usize, Vec<std::ops::Range<usize>>), String> {
    let mut start = None;
    let mut bodies = Vec::new();
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|error| error.to_string())? {
            Payload::CodeSectionStart { range, .. } => start = Some(range.start),
            Payload::CodeSectionEntry(body) => bodies.push(body.range()),
            _ => {}
        }
    }
    Ok((start.ok_or("module has no code section")?, bodies))
}

/// Rewrite one function body in a single streaming pass over its operators.
/// Only the start function may contain a wait or notify (the linker
/// initializer), and recognising that initializer needs its whole operator
/// list, so only the start body is collected; every other body is rewritten
/// as it is read, without an operator list. A wait or notify anywhere else is
/// an error at the first occurrence. Every replaced instruction is returned as
/// an [`Edit`] (body-relative offset, old and new length) so the module's DWARF
/// can follow the bytes that moved.
fn prepare_body(
    body: &FunctionBody<'_>,
    is_start: bool,
    stack: Option<&StackBounds>,
) -> Result<(Vec<u8>, Vec<Edit>), String> {
    let mut reader = body
        .get_operators_reader()
        .map_err(|error| error.to_string())?;
    let mut collected: Option<Vec<Operator<'_>>> = is_start.then(Vec::new);
    let mut has_wait = false;
    let mut output = Vec::new();
    let mut edits = Vec::new();
    let base = body.range().start;
    let mut cursor = 0;
    while !reader.eof() {
        let start = reader.original_position();
        let operation = reader.read().map_err(|error| error.to_string())?;
        let end = reader.original_position();
        if matches!(
            operation,
            Operator::MemoryAtomicWait32 { .. }
                | Operator::MemoryAtomicWait64 { .. }
                | Operator::MemoryAtomicNotify { .. }
        ) {
            if !is_start {
                return Err(
                    "shared core contains a wait/notify outside the verified linker initializer"
                        .into(),
                );
            }
            has_wait = true;
        }
        let replacement = match &operation {
            Operator::MemoryAtomicNotify { .. } => Some(vec![
                Instruction::Drop,
                Instruction::Drop,
                Instruction::I32Const(0),
            ]),
            // Serial instantiation cannot observe initialization in progress.
            // Trap if that invariant is broken; never turn a real wait into success.
            Operator::MemoryAtomicWait32 { .. } => Some(vec![
                Instruction::Drop,
                Instruction::Drop,
                Instruction::Drop,
                Instruction::Unreachable,
            ]),
            &Operator::GlobalSet { global_index }
                if stack.is_some_and(|stack| stack.pointer == global_index) =>
            {
                let stack = stack.unwrap();
                Some(vec![
                    Instruction::GlobalSet(global_index),
                    Instruction::GlobalGet(global_index),
                    Instruction::GlobalGet(stack.low),
                    Instruction::I32LtU,
                    Instruction::If(wasm_encoder::BlockType::Empty),
                    Instruction::Unreachable,
                    Instruction::End,
                    Instruction::GlobalGet(global_index),
                    Instruction::GlobalGet(stack.high),
                    Instruction::I32GtU,
                    Instruction::If(wasm_encoder::BlockType::Empty),
                    Instruction::Unreachable,
                    Instruction::End,
                ])
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            output.extend_from_slice(&body.as_bytes()[cursor..start - base]);
            let replaced_at = output.len();
            for instruction in replacement {
                instruction.encode(&mut output);
            }
            edits.push(Edit {
                start: (start - base) as u64,
                old_len: (end - start) as u64,
                new_len: (output.len() - replaced_at) as u64,
            });
            cursor = end - base;
        }
        if let Some(collected) = &mut collected {
            collected.push(operation);
        }
    }
    if has_wait {
        let operations = collected
            .as_deref()
            .expect("the start body collects its operators");
        if !is_linker_initializer(operations) {
            return Err(
                "shared core contains a wait/notify outside the verified linker initializer".into(),
            );
        }
        if body
            .get_locals_reader()
            .map_err(|error| error.to_string())?
            .get_count()
            != 0
        {
            return Err("shared linker initializer unexpectedly has locals".into());
        }
    }
    output.extend_from_slice(&body.as_bytes()[cursor..]);
    Ok((output, edits))
}

fn is_linker_initializer(ops: &[Operator<'_>]) -> bool {
    let operations: Vec<&Operator<'_>> = ops.iter().collect();
    let [
        Operator::Block {
            blockty: BlockType::Empty,
        },
        Operator::Block {
            blockty: BlockType::Empty,
        },
        Operator::Block {
            blockty: BlockType::Empty,
        },
        Operator::I32Const { value: guard },
        Operator::I32Const { value: 0 },
        Operator::I32Const { value: 1 },
        Operator::I32AtomicRmwCmpxchg { memarg },
        Operator::BrTable { targets },
        Operator::End,
        rest @ ..,
    ] = operations.as_slice()
    else {
        return false;
    };
    if memarg.memory != 0
        || memarg.offset != 0
        || memarg.align != 2
        || targets.default() != 2
        || targets
            .targets()
            .collect::<Result<Vec<_>, _>>()
            .ok()
            .as_deref()
            != Some(&[0, 1])
    {
        return false;
    }
    let Some(store) = rest
        .iter()
        .position(|op| matches!(op, Operator::I32AtomicStore { .. }))
    else {
        return false;
    };
    if store < 2 {
        return false;
    }
    // The straight-line initializer can only copy/fill passive data and set TLS.
    if rest[..store - 2].iter().any(|op| {
        !matches!(
            op,
            Operator::I32Const { .. }
                | Operator::GlobalSet { .. }
                | Operator::MemoryInit { mem: 0, .. }
                | Operator::MemoryFill { mem: 0 }
        )
    }) {
        return false;
    }
    let [
        Operator::I32Const { value: store_guard },
        Operator::I32Const { value: 2 },
        Operator::I32AtomicStore { memarg: store_arg },
        Operator::I32Const {
            value: notify_guard,
        },
        Operator::I32Const { value: -1 },
        Operator::MemoryAtomicNotify { memarg: notify_arg },
        Operator::Drop,
        Operator::Br { relative_depth: 1 },
        Operator::End,
        Operator::I32Const { value: wait_guard },
        Operator::I32Const { value: 1 },
        Operator::I64Const { value: -1 },
        Operator::MemoryAtomicWait32 { memarg: wait_arg },
        Operator::Drop,
        Operator::End,
        tail @ ..,
    ] = &rest[store - 2..]
    else {
        return false;
    };
    if guard != store_guard || guard != notify_guard || guard != wait_guard {
        return false;
    }
    for arg in [store_arg, notify_arg, wait_arg] {
        if arg.memory != 0 || arg.offset != 0 || arg.align != 2 {
            return false;
        }
    }
    let Some((last, drops)) = tail.split_last() else {
        return false;
    };
    matches!(last, Operator::End)
        && drops
            .iter()
            .all(|op| matches!(op, Operator::DataDrop { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_encoder::{
        BlockType, Function, FunctionSection, MemArg, MemorySection, MemoryType, StartSection,
        TypeSection,
    };

    fn fixture(startup: bool, expected_state: i32) -> Vec<u8> {
        let mut module = Module::new();
        let mut types = TypeSection::new();
        types.ty().function([], []);
        module.section(&types);
        let mut functions = FunctionSection::new();
        functions.function(0);
        module.section(&functions);
        let mut memory = MemorySection::new();
        memory.memory(MemoryType {
            minimum: 1,
            maximum: Some(1),
            memory64: false,
            shared: true,
            page_size_log2: None,
        });
        module.section(&memory);
        if startup {
            module.section(&StartSection { function_index: 0 });
        }
        let arg = MemArg {
            offset: 0,
            align: 2,
            memory_index: 0,
        };
        let mut function = Function::new([]);
        for instruction in [
            Instruction::Block(BlockType::Empty),
            Instruction::Block(BlockType::Empty),
            Instruction::Block(BlockType::Empty),
            Instruction::I32Const(0),
            Instruction::I32Const(0),
            Instruction::I32Const(expected_state),
            Instruction::I32AtomicRmwCmpxchg(arg),
            Instruction::BrTable(vec![0, 1].into(), 2),
            Instruction::End,
            Instruction::I32Const(0),
            Instruction::I32Const(2),
            Instruction::I32AtomicStore(arg),
            Instruction::I32Const(0),
            Instruction::I32Const(-1),
            Instruction::MemoryAtomicNotify(arg),
            Instruction::Drop,
            Instruction::Br(1),
            Instruction::End,
            Instruction::I32Const(0),
            Instruction::I32Const(1),
            Instruction::I64Const(-1),
            Instruction::MemoryAtomicWait32(arg),
            Instruction::Drop,
            Instruction::End,
            Instruction::End,
        ] {
            function.instruction(&instruction);
        }
        let mut code = CodeSection::new();
        code.function(&function);
        module.section(&code);
        module.finish()
    }

    /// One function `local.get 0; global.set $__stack_pointer; end` with the
    /// linker's stack exports, so `prepare` inserts the stack check.
    fn stack_module() -> Vec<u8> {
        let mut module = Module::new();
        let mut types = TypeSection::new();
        types.ty().function([ValType::I32], []);
        module.section(&types);
        let mut functions = FunctionSection::new();
        functions.function(0);
        module.section(&functions);
        let mut globals = GlobalSection::new();
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: true,
                shared: false,
            },
            &ConstExpr::i32_const(128),
        );
        globals.global(
            GlobalType {
                val_type: ValType::I32,
                mutable: false,
                shared: false,
            },
            &ConstExpr::i32_const(64),
        );
        module.section(&globals);
        let mut exports = ExportSection::new();
        exports.export("__stack_pointer", ExportKind::Global, 0);
        exports.export("__stack_low", ExportKind::Global, 1);
        module.section(&exports);
        let mut function = Function::new([]);
        function.instruction(&Instruction::LocalGet(0));
        function.instruction(&Instruction::GlobalSet(0));
        function.instruction(&Instruction::End);
        let mut code = CodeSection::new();
        code.function(&function);
        module.section(&code);
        module.finish()
    }

    /// DWARF 4 for one compilation unit (`/loom/source`, `src/lib.rs`) with a
    /// subprogram `greet` spanning `start..start + length` and one line row per
    /// `(body-relative offset, line)`; addresses are code-contents offsets.
    fn dwarf_fixture(start: u64, length: u64, rows: &[(u64, u64)]) -> Vec<(&'static str, Vec<u8>)> {
        use gimli::write::{
            Address, AttributeValue, EndianVec, LineProgram, LineString, Sections, Unit,
        };
        let encoding = gimli::Encoding {
            format: gimli::Format::Dwarf32,
            version: 4,
            address_size: 4,
        };
        let mut program = LineProgram::new(
            encoding,
            gimli::LineEncoding::default(),
            LineString::String(b"/loom/source".to_vec()),
            None,
            LineString::String(b"src/lib.rs".to_vec()),
            None,
        );
        let directory = program.default_directory();
        let file = program.add_file(LineString::String(b"src/lib.rs".to_vec()), directory, None);
        program.begin_sequence(Some(Address::Constant(start)));
        for &(offset, line) in rows {
            let row = program.row();
            row.address_offset = offset;
            row.file = file;
            row.line = line;
            program.generate_row();
        }
        program.end_sequence(length);
        let mut unit = Unit::new(encoding, program);
        let root = unit.root();
        let entry = unit.get_mut(root);
        entry.set(
            gimli::DW_AT_name,
            AttributeValue::String(b"src/lib.rs".to_vec()),
        );
        entry.set(
            gimli::DW_AT_comp_dir,
            AttributeValue::String(b"/loom/source".to_vec()),
        );
        entry.set(
            gimli::DW_AT_low_pc,
            AttributeValue::Address(Address::Constant(start)),
        );
        entry.set(gimli::DW_AT_high_pc, AttributeValue::Udata(length));
        let function = unit.add(root, gimli::DW_TAG_subprogram);
        let entry = unit.get_mut(function);
        entry.set(gimli::DW_AT_name, AttributeValue::String(b"greet".to_vec()));
        entry.set(
            gimli::DW_AT_low_pc,
            AttributeValue::Address(Address::Constant(start)),
        );
        entry.set(gimli::DW_AT_high_pc, AttributeValue::Udata(length));
        let mut dwarf = gimli::write::Dwarf::new();
        dwarf.units.add(unit);
        let mut sections = Sections::new(EndianVec::new(gimli::LittleEndian));
        dwarf.write(&mut sections).unwrap();
        let mut written = Vec::new();
        sections
            .for_each(|id, data| {
                if !data.slice().is_empty() {
                    written.push((id.name(), data.slice().to_vec()));
                }
                Ok::<(), gimli::write::Error>(())
            })
            .unwrap();
        written
    }

    fn with_custom_sections(mut module: Vec<u8>, sections: &[(&str, Vec<u8>)]) -> Vec<u8> {
        for (name, data) in sections {
            wasm_encoder::CustomSection {
                name: (*name).into(),
                data: data.as_slice().into(),
            }
            .append_to(&mut module);
        }
        module
    }

    fn debug_sections(bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
        let mut sections = BTreeMap::new();
        for payload in Parser::new(0).parse_all(bytes) {
            if let Payload::CustomSection(section) = payload.unwrap()
                && section.name().starts_with(".debug_")
            {
                sections.insert(section.name().to_owned(), section.data().to_vec());
            }
        }
        sections
    }

    /// The body of the single function of `bytes`, relative to the code
    /// section's contents, as DWARF addresses it.
    fn only_body(bytes: &[u8]) -> std::ops::Range<u64> {
        let (start, bodies) = code_layout(bytes).unwrap();
        assert_eq!(bodies.len(), 1);
        (bodies[0].start - start) as u64..(bodies[0].end - start) as u64
    }

    #[test]
    fn debug_sections_follow_the_rewritten_code() {
        let module = stack_module();
        let old = only_body(&module);
        // locals vector (1 byte), local.get 0 (2), global.set 0 (2), end (1)
        assert_eq!(old.end - old.start, 6);
        let module = with_custom_sections(
            module,
            &dwarf_fixture(old.start, old.end - old.start, &[(1, 5), (3, 6)]),
        );
        let prepared = prepare(&module).unwrap();
        let new = only_body(&prepared);
        assert!(
            new.end - new.start > 6,
            "the stack check lengthened the body"
        );
        let sections = debug_sections(&prepared);
        for required in [".debug_abbrev", ".debug_info", ".debug_line"] {
            assert!(sections.contains_key(required), "{:?}", sections.keys());
        }
        let dwarf = gimli::Dwarf::load(|id: gimli::SectionId| {
            Ok::<_, gimli::Error>(gimli::EndianSlice::new(
                sections.get(id.name()).map_or(&[][..], Vec::as_slice),
                gimli::LittleEndian,
            ))
        })
        .unwrap();
        let mut headers = dwarf.units();
        let unit = dwarf.unit(headers.next().unwrap().unwrap()).unwrap();
        assert!(headers.next().unwrap().is_none());
        let mut rows = BTreeMap::new();
        let mut sequence_end = None;
        let mut line_rows = unit.line_program.clone().unwrap().rows();
        while let Some((_, row)) = line_rows.next_row().unwrap() {
            if row.end_sequence() {
                sequence_end = Some(row.address());
            } else {
                rows.insert(row.address(), row.line().unwrap().get());
            }
        }
        // local.get keeps its place; global.set starts where it did; the end moved.
        assert_eq!(
            rows,
            BTreeMap::from([(new.start + 1, 5), (new.start + 3, 6)])
        );
        assert_eq!(sequence_end, Some(new.end));
        let mut entries = unit.entries();
        let mut function = None;
        while let Some(entry) = entries.next_dfs().unwrap() {
            if entry.tag() == gimli::DW_TAG_subprogram {
                function = Some((
                    entry.attr_value(gimli::DW_AT_low_pc).unwrap().unwrap(),
                    entry.attr_value(gimli::DW_AT_high_pc).unwrap().unwrap(),
                ));
            }
        }
        assert_eq!(
            function,
            Some((
                gimli::AttributeValue::Addr(new.start),
                gimli::AttributeValue::Udata(new.end - new.start),
            ))
        );
        let mut stamped = prepared;
        loom_proto::core_protocol::stamp(&mut stamped);
        assert!(loom_proto::core_protocol::is_current(&stamped));
    }

    #[test]
    fn debug_rows_inside_a_replaced_instruction_fail_closed() {
        let module = stack_module();
        let old = only_body(&module);
        // Offset 4 is the second byte of `global.set 0`, never an instruction start.
        let module = with_custom_sections(
            module,
            &dwarf_fixture(old.start, old.end - old.start, &[(1, 5), (4, 6)]),
        );
        let error = prepare(&module).unwrap_err();
        assert!(error.contains("not an instruction boundary"), "{error}");
        let module = with_custom_sections(stack_module(), &[(".debug_frame", Vec::new())]);
        let error = prepare(&module).unwrap_err();
        assert!(error.contains(".debug_frame"), "{error}");
    }

    #[test]
    fn stack_bounds_are_exported_and_checked_in_functions_without_waits() {
        let prepared = prepare(&stack_module()).unwrap();
        let mut bounds = 0;
        let mut traps = 0;
        for payload in Parser::new(0).parse_all(&prepared) {
            match payload.unwrap() {
                Payload::ExportSection(exports) => {
                    for export in exports {
                        if matches!(
                            export.unwrap().name,
                            "__loom_stack_low" | "__loom_stack_high"
                        ) {
                            bounds += 1;
                        }
                    }
                }
                Payload::CodeSectionEntry(body) => {
                    for operator in body.get_operators_reader().unwrap() {
                        if matches!(operator.unwrap(), Operator::Unreachable) {
                            traps += 1;
                        }
                    }
                }
                _ => {}
            }
        }
        assert_eq!(bounds, 2);
        assert_eq!(traps, 2);
    }

    #[test]
    fn only_verified_serial_initializer_is_lowered() {
        let prepared = prepare(&fixture(true, 1)).unwrap();
        for payload in Parser::new(0).parse_all(&prepared) {
            if let Payload::CodeSectionEntry(body) = payload.unwrap() {
                for operator in body.get_operators_reader().unwrap() {
                    assert!(!matches!(
                        operator.unwrap(),
                        Operator::MemoryAtomicWait32 { .. }
                            | Operator::MemoryAtomicWait64 { .. }
                            | Operator::MemoryAtomicNotify { .. }
                    ));
                }
            }
        }
        assert!(
            prepare(&fixture(false, 1)).is_err(),
            "arbitrary task waits must fail"
        );
        assert!(
            prepare(&fixture(true, 3)).is_err(),
            "unrecognized initializer must fail"
        );
    }
}
