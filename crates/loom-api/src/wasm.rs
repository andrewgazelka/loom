//! `GET /v1/wasm/{component_hash}`: a compiled module as WebAssembly text with
//! every instruction joined to the source line its DWARF names.
//!
//! The text comes from wasmprinter, one instruction per line, and each printed
//! line's byte offset in the module is taken from the printer itself. DWARF for
//! WebAssembly addresses an instruction by its offset from the start of the
//! code section's contents (wasmparser's `CodeSectionStart` range start), so an
//! instruction's address is its module offset minus that start. The join takes
//! the line-program row with the greatest address not exceeding the
//! instruction's, provided the row's sequence has not ended before it.
//! Missing DWARF is not an error (`debug: false`, no `lines`); DWARF that is
//! present but does not parse is.
use anyhow::{Context, Result, ensure};
use gimli::{EndianSlice, LittleEndian, SectionId};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use wasmparser::{ExternalKind, KnownCustom, Name, Parser, Payload, TypeRef};

/// The exact text the wrapper compile saw for the definition whose build
/// produced `component_hash`: the checker's normalized reprint of the stored
/// source (what materialization writes to `src/lib.rs`) followed by the
/// generated entry wrappers from the identity document's `entry` table and
/// `schema`.
pub(crate) async fn compiled_source_for(
    service: &crate::Service,
    component_hash: &str,
) -> Result<String> {
    let store = &service.store;
    let definition = store
        .definitions()?
        .into_iter()
        .find(|def| def.component_hash.as_deref() == Some(component_hash))
        .context("no stored definition was built into this artifact")?;
    let stored = store
        .source(&definition.hash)?
        .context("definition source missing")?;
    let name = store
        .definition_name(&definition.hash)?
        .unwrap_or_else(|| definition.hash.clone());
    let checked = service
        .checker
        .check_with_signatures(
            &loom_proto::DefineRequest {
                lang: definition.lang,
                name,
                source: stored,
                deps: BTreeMap::new(),
                allowed_effects: definition.allowed_effects.clone(),
            },
            &BTreeMap::new(),
        )
        .await
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let source = if checked.source.trim_start().starts_with('{') {
        let bundle: serde_json::Value = serde_json::from_str(&checked.source)?;
        let file = &bundle["files"]["src/lib.rs"];
        file.as_str()
            .or_else(|| file["text"].as_str())
            .map(str::to_owned)
            .context("source bundle has no text src/lib.rs")?
    } else {
        checked.source
    };
    let identity = store
        .build_identity(&definition.hash)?
        .context("definition has no build identity")?;
    let document: serde_json::Value = serde_json::from_slice(
        &store
            .get(&identity.item_hashes_ref)?
            .context("item document missing from CAS")?,
    )?;
    let entries: BTreeMap<String, String> = serde_json::from_value(document["entry"].clone())
        .context("item document has no entry table")?;
    let schema = document["schema"].as_str().map(str::to_owned);
    loom_build::compiled_source(&source, &entries, schema)
        .map_err(|error| anyhow::anyhow!("{error}"))
}

/// Largest module the text view renders.
pub(crate) const MAX_MODULE_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Debug, Serialize)]
pub(crate) struct WasmView {
    /// The whole module as text, one instruction per line.
    pub wat: String,
    /// Every defined function in code-section order.
    pub functions: Vec<Function>,
    /// One entry per `wat` line whose instruction has a source line.
    pub lines: Vec<Line>,
    /// Whether the module carries `.debug_*` sections at all.
    pub debug: bool,
    /// The text the compiler saw for this artifact: the stored source followed
    /// by the generated entry wrappers, so every `lines[].line` for the
    /// definition's own file indexes into it. `None` when the artifact belongs
    /// to no stored definition or the wrappers cannot be regenerated; the
    /// reason is in `compiled_source_error`.
    pub compiled_source: Option<String>,
    pub compiled_source_error: Option<String>,
}

#[derive(Debug, Serialize)]
pub(crate) struct Function {
    pub index: u32,
    /// From the `name` section.
    pub name: Option<String>,
    /// Whether the export table names this function.
    pub exported: bool,
    /// 1-based `wat` line of `(func`.
    pub start_line: u32,
    /// 1-based `wat` line of the function's closing parenthesis.
    pub end_line: u32,
}

#[derive(Debug, Serialize)]
pub(crate) struct Line {
    /// 1-based line in `wat`.
    pub wat_line: u32,
    /// The path as rustc recorded it, relative to the crate root when it lies
    /// under the compilation directory; see [`file_path`].
    pub file: String,
    pub line: u32,
}

/// One `.debug_line` row: the instructions at `address..end` (offsets from
/// the start of the code section's contents) came from `file:line`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Row {
    pub address: u64,
    pub end: u64,
    pub file: String,
    pub line: u32,
}

/// Every row of every line program, sorted by address.
pub(crate) struct LineTable {
    rows: Vec<Row>,
}

impl LineTable {
    pub(crate) fn new(mut rows: Vec<Row>) -> Self {
        rows.sort_by_key(|row| row.address);
        Self { rows }
    }

    /// The row covering `address`: the last row starting at or before it, if
    /// its sequence has not ended by then.
    pub(crate) fn lookup(&self, address: u64) -> Option<&Row> {
        let index = self.rows.partition_point(|row| row.address <= address);
        let row = self.rows[..index].last()?;
        (address < row.end).then_some(row)
    }
}

/// Wasmprinter's output plus the module offset behind every printed line that
/// has one (1-based line number, offset).
struct Tracking {
    text: String,
    newlines: u32,
    lines: Vec<(u32, usize)>,
}

impl wasmprinter::Print for Tracking {
    fn write_str(&mut self, text: &str) -> std::io::Result<()> {
        self.newlines += text.matches('\n').count() as u32;
        self.text.push_str(text);
        Ok(())
    }
    fn start_line(&mut self, offset: Option<usize>) {
        if let Some(offset) = offset {
            // The printer writes the newline before announcing the line.
            self.lines.push((self.newlines + 1, offset));
        }
    }
}

pub(crate) fn wasm_view(bytes: &[u8]) -> Result<WasmView> {
    let mut imports = 0u32;
    let mut code: Option<Range<usize>> = None;
    let mut bodies: Vec<Range<usize>> = Vec::new();
    let mut exported = BTreeSet::new();
    let mut names = BTreeMap::new();
    let mut debug: BTreeMap<&str, &[u8]> = BTreeMap::new();
    for payload in Parser::new(0).parse_all(bytes) {
        match payload? {
            Payload::ImportSection(section) => {
                for import in section.into_imports() {
                    if matches!(import?.ty, TypeRef::Func(_) | TypeRef::FuncExact(_)) {
                        imports += 1;
                    }
                }
            }
            Payload::ExportSection(section) => {
                for export in section {
                    let export = export?;
                    if export.kind == ExternalKind::Func {
                        exported.insert(export.index);
                    }
                }
            }
            Payload::CodeSectionStart { range, .. } => code = Some(range),
            Payload::CodeSectionEntry(body) => bodies.push(body.range()),
            Payload::CustomSection(section) => match section.as_known() {
                KnownCustom::Name(reader) => {
                    for name in reader {
                        if let Name::Function(map) = name? {
                            for naming in map {
                                let naming = naming?;
                                names.insert(naming.index, naming.name.to_owned());
                            }
                        }
                    }
                }
                _ if section.name().starts_with(".debug_") => {
                    ensure!(
                        debug.insert(section.name(), section.data()).is_none(),
                        "duplicate DWARF section {}",
                        section.name()
                    );
                }
                _ => {}
            },
            _ => {}
        }
    }
    let mut printer = Tracking {
        text: String::new(),
        newlines: 0,
        lines: Vec::new(),
    };
    wasmprinter::Config::new().print(bytes, &mut printer)?;
    let mut by_offset: Vec<(usize, u32)> = printer
        .lines
        .iter()
        .map(|&(line, offset)| (offset, line))
        .collect();
    by_offset.sort_unstable();
    let functions = bodies
        .iter()
        .enumerate()
        .map(|(position, range)| {
            let index = imports + position as u32;
            let first = by_offset.partition_point(|&(offset, _)| offset < range.start);
            let last = by_offset.partition_point(|&(offset, _)| offset <= range.end);
            let lines = by_offset[first..last].iter().map(|&(_, line)| line);
            let (start_line, end_line) = lines
                .clone()
                .min()
                .zip(lines.max())
                .with_context(|| format!("function {index} printed no lines"))?;
            Ok(Function {
                index,
                name: names.get(&index).cloned(),
                exported: exported.contains(&index),
                start_line,
                end_line,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let table = if debug.is_empty() {
        LineTable::new(Vec::new())
    } else {
        line_table(&debug)?
    };
    let lines = match &code {
        Some(code) => printer
            .lines
            .iter()
            .filter(|(_, offset)| code.contains(offset))
            .filter_map(|&(wat_line, offset)| {
                let row = table.lookup((offset - code.start) as u64)?;
                Some(Line {
                    wat_line,
                    file: row.file.clone(),
                    line: row.line,
                })
            })
            .collect(),
        None => Vec::new(),
    };
    Ok(WasmView {
        wat: printer.text,
        functions,
        lines,
        debug: !debug.is_empty(),
        compiled_source: None,
        compiled_source_error: None,
    })
}

/// Every line-program row of every unit, each covering the instructions from
/// its address up to the next row of its sequence. Rows for line 0 (no source)
/// are dropped.
fn line_table(sections: &BTreeMap<&str, &[u8]>) -> Result<LineTable> {
    let empty: &[u8] = &[];
    let dwarf = gimli::Dwarf::load(|id: SectionId| -> Result<_, gimli::Error> {
        Ok(EndianSlice::new(
            sections.get(id.name()).copied().unwrap_or(empty),
            LittleEndian,
        ))
    })?;
    let mut rows = Vec::new();
    let mut headers = dwarf.units();
    while let Some(header) = headers.next()? {
        let unit = dwarf.unit(header)?;
        let comp_dir = unit
            .comp_dir
            .as_ref()
            .map(|directory| directory.to_string_lossy().into_owned());
        let Some(program) = unit.line_program.clone() else {
            continue;
        };
        let mut files: BTreeMap<u64, String> = BTreeMap::new();
        let mut open: Option<(u64, String, u32)> = None;
        let mut program_rows = program.rows();
        while let Some((header, row)) = program_rows.next_row()? {
            let address = row.address();
            if let Some((start, file, line)) = open.take() {
                ensure!(address >= start, "DWARF line rows run backwards");
                if address > start && line != 0 {
                    rows.push(Row {
                        address: start,
                        end: address,
                        file,
                        line,
                    });
                }
            }
            if row.end_sequence() {
                continue;
            }
            let file = match files.get(&row.file_index()) {
                Some(file) => file.clone(),
                None => {
                    let entry = header.file(row.file_index()).with_context(|| {
                        format!("DWARF row names missing file {}", row.file_index())
                    })?;
                    let name = dwarf.attr_string(&unit, entry.path_name())?;
                    let directory = entry
                        .directory(header)
                        .map(|directory| dwarf.attr_string(&unit, directory))
                        .transpose()?;
                    let path = file_path(
                        comp_dir.as_deref(),
                        directory
                            .as_ref()
                            .map(|directory| directory.to_string_lossy())
                            .as_deref(),
                        &name.to_string_lossy(),
                    );
                    files.insert(row.file_index(), path.clone());
                    path
                }
            };
            let line = row.line().map_or(0, |line| line.get() as u32);
            open = Some((address, file, line));
        }
        ensure!(open.is_none(), "DWARF line sequence has no end");
    }
    Ok(LineTable::new(rows))
}

/// The path a line row names. Relative names join their directory (itself
/// relative to the compilation directory when not absolute); a path under the
/// compilation directory is returned relative to it, so the guest's own file
/// reads `src/lib.rs`, while paths elsewhere (rustc's sysroot, a registry
/// checkout) stay as rustc recorded them for the client to classify.
pub(crate) fn file_path(comp_dir: Option<&str>, directory: Option<&str>, name: &str) -> String {
    fn join(base: &str, rest: &str) -> String {
        format!("{}/{rest}", base.trim_end_matches('/'))
    }
    let comp_dir = comp_dir.map(|root| root.trim_end_matches('/'));
    let path = if name.starts_with('/') {
        name.to_owned()
    } else {
        match (
            directory.filter(|directory| !directory.is_empty()),
            comp_dir,
        ) {
            (Some(directory), _) if directory.starts_with('/') => join(directory, name),
            (Some(directory), Some(root)) => join(&join(root, directory), name),
            (Some(directory), None) => join(directory, name),
            (None, Some(root)) => join(root, name),
            (None, None) => name.to_owned(),
        }
    };
    match comp_dir {
        Some(root) if !root.is_empty() => path
            .strip_prefix(root)
            .and_then(|rest| rest.strip_prefix('/'))
            .filter(|rest| !rest.is_empty())
            .map_or(path.clone(), str::to_owned),
        _ => path,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(address: u64, end: u64, line: u32) -> Row {
        Row {
            address,
            end,
            file: "src/lib.rs".into(),
            line,
        }
    }

    #[test]
    fn instructions_join_to_the_last_row_not_past_them() {
        let table = LineTable::new(vec![row(20, 30, 7), row(0, 10, 5), row(10, 20, 6)]);
        assert_eq!(table.lookup(5).map(|row| row.line), Some(5));
        assert_eq!(table.lookup(10).map(|row| row.line), Some(6));
        assert_eq!(table.lookup(25).map(|row| row.line), Some(7));
        assert_eq!(table.lookup(0).map(|row| row.line), Some(5));
        assert_eq!(table.lookup(30), None, "past the sequence end");
    }

    #[test]
    fn instructions_before_the_first_row_map_to_nothing() {
        let table = LineTable::new(vec![row(10, 20, 6), row(20, 30, 7)]);
        assert_eq!(table.lookup(5), None);
        assert_eq!(table.lookup(9), None);
        assert_eq!(table.lookup(10).map(|row| row.line), Some(6));
        assert_eq!(LineTable::new(Vec::new()).lookup(0), None);
    }

    #[test]
    fn crate_files_are_relative_and_external_paths_stay_as_recorded() {
        let root = Some("/loom/source");
        assert_eq!(
            file_path(root, Some("/loom/source"), "src/lib.rs"),
            "src/lib.rs"
        );
        assert_eq!(file_path(root, None, "src/lib.rs"), "src/lib.rs");
        assert_eq!(file_path(root, Some("src"), "lib.rs"), "src/lib.rs");
        assert_eq!(
            file_path(root, None, "/loom/source/src/lib.rs"),
            "src/lib.rs"
        );
        assert_eq!(
            file_path(root, Some("/rustc/abc/library/core/src"), "num.rs"),
            "/rustc/abc/library/core/src/num.rs"
        );
        assert_eq!(
            file_path(root, None, "/home/x/.cargo/registry/src/serde/lib.rs"),
            "/home/x/.cargo/registry/src/serde/lib.rs"
        );
        assert_eq!(
            file_path(None, Some("/other"), "lib.rs"),
            "/other/lib.rs",
            "no compilation directory leaves the path alone"
        );
        assert_eq!(
            file_path(root, None, "/loom/source"),
            "/loom/source",
            "the root itself is not shortened to nothing"
        );
    }

    #[test]
    fn modules_without_dwarf_report_debug_false_and_no_lines() {
        let mut module = wasm_encoder_free_module();
        let view = wasm_view(&module).unwrap();
        assert!(!view.debug);
        assert!(view.lines.is_empty());
        assert_eq!(view.functions.len(), 1);
        assert_eq!(view.functions[0].index, 0);
        assert_eq!(view.functions[0].name.as_deref(), Some("answer"));
        assert!(view.functions[0].exported);
        assert!(view.functions[0].start_line <= view.functions[0].end_line);
        let printed = view
            .wat
            .lines()
            .nth(view.functions[0].start_line as usize - 1)
            .unwrap();
        assert!(printed.contains("func $answer"), "{printed}");
        // A `.debug_*` section that is not DWARF is an error, not an empty map.
        module.extend_from_slice(&[0, 15, 11]);
        module.extend_from_slice(b".debug_info");
        module.extend_from_slice(&[1, 2, 3]);
        let error = wasm_view(&module).unwrap_err().to_string();
        assert!(!error.is_empty());
    }

    /// `(module (func $answer (export "answer") (result i32) i32.const 42))`
    /// with a `name` section, hand-encoded so this crate needs no encoder.
    fn wasm_encoder_free_module() -> Vec<u8> {
        let mut module = b"\0asm\x01\0\0\0".to_vec();
        // type: () -> i32
        module.extend_from_slice(&[1, 5, 1, 0x60, 0, 1, 0x7f]);
        // function: type 0
        module.extend_from_slice(&[3, 2, 1, 0]);
        // export "answer" func 0
        module.extend_from_slice(&[7, 10, 1, 6]);
        module.extend_from_slice(b"answer");
        module.extend_from_slice(&[0, 0]);
        // code: one body, no locals, i32.const 42, end
        module.extend_from_slice(&[10, 6, 1, 4, 0, 0x41, 42, 0x0b]);
        // name section (16 bytes: 1 + "name" + subsection 1 of 9 bytes),
        // function names subsection, 1 entry: 0 -> "answer"
        module.extend_from_slice(&[0, 16, 4]);
        module.extend_from_slice(b"name");
        module.extend_from_slice(&[1, 9, 1, 0, 6]);
        module.extend_from_slice(b"answer");
        module
    }
}
