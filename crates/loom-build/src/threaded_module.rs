//! Lower only LLVM's one-time shared-memory initializer for serialized startup.
//! All task code must remain free of Wasm waits and notifications.
use wasm_encoder::{
    CodeSection, ConstExpr, Encode, ExportKind, ExportSection, GlobalSection, GlobalType,
    Instruction, Module, RawSection, ValType,
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
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|error| error.to_string())? {
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
    for (index, body) in bodies.iter().enumerate() {
        let function = imports + index as u32;
        code.raw(&prepare_body(
            body,
            Some(function) == start,
            stack.as_ref(),
        )?);
    }
    let mut module = Module::new();
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
    let result = module.finish();
    wasmparser::Validator::new()
        .validate_all(&result)
        .map_err(|error| error.to_string())?;
    Ok(result)
}

/// Rewrite one function body in a single streaming pass over its operators.
/// Only the start function may contain a wait or notify (the linker
/// initializer), and recognising that initializer needs its whole operator
/// list, so only the start body is collected; every other body is rewritten
/// as it is read, without an operator list. A wait or notify anywhere else is
/// an error at the first occurrence.
fn prepare_body(
    body: &FunctionBody<'_>,
    is_start: bool,
    stack: Option<&StackBounds>,
) -> Result<Vec<u8>, String> {
    let mut reader = body
        .get_operators_reader()
        .map_err(|error| error.to_string())?;
    let mut collected: Option<Vec<Operator<'_>>> = is_start.then(Vec::new);
    let mut has_wait = false;
    let mut output = Vec::new();
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
            for instruction in replacement {
                instruction.encode(&mut output);
            }
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
    Ok(output)
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

    #[test]
    fn stack_bounds_are_exported_and_checked_in_functions_without_waits() {
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
        let prepared = prepare(&module.finish()).unwrap();
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
