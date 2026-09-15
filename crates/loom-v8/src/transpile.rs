//! TypeScript is an admission-time transform; actor turns execute cached V8 code.
use anyhow::{Context, Result};
use deno_ast::{EmitOptions, MediaType, ParseParams, SourceMapOption, TranspileOptions};

/// Original source identities include the exact compiler and transform contract.
pub fn typescript_abi() -> String {
    format!(
        "{};deno_ast={};typescript=1",
        crate::ABI_VERSION,
        deno_ast::VERSION
    )
}

/// Parse as a script so static module imports/exports cannot disappear during
/// type erasure. There is no module loader or ambient Deno/Node host authority.
/// Compiler API: https://docs.rs/deno_ast/0.53.3/deno_ast/struct.ParsedSource.html
pub fn transpile_typescript(source: &str) -> Result<String> {
    let parsed = deno_ast::parse_script(ParseParams {
        specifier: deno_ast::ModuleSpecifier::parse("loom:///main.ts")?,
        text: source.into(),
        media_type: MediaType::TypeScript,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
    .context("TypeScript parse failed")?;
    Ok(parsed
        .transpile(
            &TranspileOptions {
                jsx: None,
                ..Default::default()
            },
            &Default::default(),
            &EmitOptions {
                source_map: SourceMapOption::None,
                ..Default::default()
            },
        )
        .context("TypeScript transpilation failed")?
        .into_source()
        .text)
}
