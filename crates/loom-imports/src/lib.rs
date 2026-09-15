//! Admission-only module resolution. Execution consumes the complete CAS artifact.
use anyhow::{Context, Result, bail, ensure};
use deno_ast::swc::{
    ast::*,
    ecma_visit::{Visit, VisitWith},
};
use deno_ast::{EmitOptions, MediaType, ParseParams, SourceMapOption, TranspileOptions};
use serde_json::Value;
use std::{path::PathBuf, time::Duration};

pub const DENO_VERSION: &str = "2.9.6";
pub use loom_proto::ScriptArtifact;
pub use loom_proto::script_artifact::MAX_ARTIFACT_BYTES;
use loom_proto::script_artifact::{BUNDLE_COMPILER, DIRECT_COMPILER};

#[derive(Clone, Debug)]
pub struct Compiler {
    executable: Option<PathBuf>,
    allowed_imports: Vec<String>,
    admission_slots: std::sync::Arc<tokio::sync::Semaphore>,
}
impl Default for Compiler {
    fn default() -> Self {
        Self {
            admission_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(2)),
            executable: std::env::var_os("LOOM_DENO")
                .map(PathBuf::from)
                .or_else(|| option_env!("LOOM_DENO").map(PathBuf::from)),
            allowed_imports: [
                "deno.land:443",
                "jsr.io:443",
                "registry.npmjs.org:443",
                "esm.sh:443",
                "raw.esm.sh:443",
                "cdn.jsdelivr.net:443",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        }
    }
}
impl Compiler {
    pub fn with_executable(path: impl Into<PathBuf>) -> Self {
        Self {
            executable: Some(path.into()),
            ..Self::default()
        }
    }
    /// Host configuration, never a field in a guest admission request.
    pub fn allow_import(mut self, host: impl Into<String>) -> Self {
        self.allowed_imports.push(host.into());
        self
    }
}

fn params(source: &str, language: &str) -> Result<ParseParams> {
    let media_type = match language {
        "typescript" => MediaType::TypeScript,
        "javascript" => MediaType::JavaScript,
        _ => bail!("unsupported script language {language}"),
    };
    Ok(ParseParams {
        specifier: deno_ast::ModuleSpecifier::parse("loom:///main.ts")?,
        text: source.into(),
        media_type,
        capture_tokens: false,
        scope_analysis: false,
        maybe_syntax: None,
    })
}

#[derive(Default)]
struct Imports {
    specifiers: Vec<String>,
    dynamic: bool,
}
impl Visit for Imports {
    fn visit_import_decl(&mut self, node: &ImportDecl) {
        self.specifiers
            .push(node.src.value.to_string_lossy().into_owned());
    }
    fn visit_named_export(&mut self, node: &NamedExport) {
        if let Some(source) = &node.src {
            self.specifiers
                .push(source.value.to_string_lossy().into_owned());
        }
    }
    fn visit_export_all(&mut self, node: &ExportAll) {
        self.specifiers
            .push(node.src.value.to_string_lossy().into_owned());
    }
    fn visit_call_expr(&mut self, node: &CallExpr) {
        if matches!(node.callee, Callee::Import(_)) {
            self.dynamic = true;
        }
        node.visit_children_with(self);
    }
}
fn validate_script(source: &str) -> Result<()> {
    let parsed = deno_ast::parse_script(params(source, "javascript")?)
        .context("bundled JavaScript is not self-contained")?;
    let mut imports = Imports::default();
    parsed
        .program_ref()
        .unwrap_script()
        .visit_with(&mut imports);
    ensure!(
        !imports.dynamic,
        "runtime dynamic imports are unsupported; use static imports"
    );
    Ok(())
}

/// Parse syntax, so imports in comments or string literals do not invoke a compiler.
pub fn requires_bundle(source: &str, language: &str) -> Result<bool> {
    let parsed =
        deno_ast::parse_module(params(source, language)?).context("script parse failed")?;
    let mut imports = Imports::default();
    parsed
        .program_ref()
        .unwrap_module()
        .visit_with(&mut imports);
    ensure!(
        !imports.dynamic,
        "runtime dynamic imports are unsupported; use static imports"
    );
    Ok(parsed
        .program_ref()
        .unwrap_module()
        .body
        .iter()
        .any(|item| matches!(item, ModuleItem::ModuleDecl(_))))
}

pub async fn compile(source: &str, language: &str, config: &Compiler) -> Result<ScriptArtifact> {
    ensure!(
        source.len() <= MAX_ARTIFACT_BYTES / 2,
        "script source exceeds size limit"
    );
    let parsed =
        deno_ast::parse_module(params(source, language)?).context("script parse failed")?;
    let mut imports = Imports::default();
    parsed
        .program_ref()
        .unwrap_module()
        .visit_with(&mut imports);
    ensure!(
        !imports.dynamic,
        "runtime dynamic imports are unsupported; use static imports"
    );
    let is_script = parsed
        .program_ref()
        .unwrap_module()
        .body
        .iter()
        .all(|item| matches!(item, ModuleItem::Stmt(_)));
    if is_script {
        let javascript = if language == "typescript" {
            parsed
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
                )?
                .into_source()
                .text
        } else {
            source.to_owned()
        };
        let artifact = ScriptArtifact {
            version: 1,
            source: source.into(),
            language: language.into(),
            compiler: DIRECT_COMPILER.into(),
            javascript,
            lock: Value::Null,
            import_origins: Vec::new(),
            source_map: Value::Null,
        };
        artifact.validate()?;
        validate_script(&artifact.javascript)?;
        return Ok(artifact);
    }
    for specifier in imports.specifiers {
        ensure!(
            specifier.starts_with("npm:")
                || specifier.starts_with("jsr:")
                || specifier.starts_with("https://")
                || (specifier.starts_with("http://")
                    && config
                        .allowed_imports
                        .iter()
                        .any(|host| specifier.starts_with(&format!("http://{host}/")))),
            "unsupported import {specifier}: use npm:, jsr:, or an allowed HTTPS origin"
        );
    }
    let executable = config
        .executable
        .as_ref()
        .context("module imports require the pinned Deno compiler (LOOM_DENO)")?;
    let _permit = config.admission_slots.acquire().await?;
    let temp = tempfile::tempdir()?;
    // Darwin aliases /var to /private/var; canonical paths keep esbuild source-map
    // references relative to the actual admission directory.
    let root = temp.path().canonicalize()?;
    tokio::fs::create_dir(root.join("cache")).await?;
    let entry = root.as_path().join(if language == "typescript" {
        "main.ts"
    } else {
        "main.js"
    });
    // Preserve the existing main/schema script ABI through esbuild's IIFE scope.
    tokio::fs::write(&entry, format!("{source}\n;globalThis.main = main;\nif (typeof LOOM_SCHEMA !== 'undefined') globalThis.LOOM_SCHEMA = LOOM_SCHEMA;\n")).await?;
    let output_path = root.as_path().join("bundle.js");
    let lock_path = root.as_path().join("deno.lock");
    // Deno does not write a lock for an export-only module. Seed its current
    // empty format so every module artifact has the same explicit lock contract.
    tokio::fs::write(&lock_path, br#"{"version":"5"}"#).await?;
    let version = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(executable)
            .env_clear()
            .env("DENO_DIR", root.as_path().join("cache"))
            .env("LOOM_IMPORT_ROOT", root.as_path())
            .env("DENO_NO_UPDATE_CHECK", "1")
            .env("DENO_NO_PROMPT", "1")
            .current_dir(root.as_path())
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .arg("--version")
            .output(),
    )
    .await
    .context("Deno compiler version check timed out")??;
    ensure!(
        version.status.success()
            && String::from_utf8_lossy(&version.stdout)
                .lines()
                .next()
                .is_some_and(|line| line.starts_with("deno 2.9.6 (stable, release, ")),
        "module compiler must be pinned Deno {DENO_VERSION}"
    );
    let mut command = tokio::process::Command::new(executable);
    command
        .env_clear()
        .env("DENO_DIR", root.as_path().join("cache"))
        .env("LOOM_IMPORT_ROOT", root.as_path())
        .env("DENO_NO_UPDATE_CHECK", "1")
        .env("DENO_NO_PROMPT", "1")
        .current_dir(root.as_path())
        .kill_on_drop(true)
        .args([
            "bundle",
            "--no-config",
            "--node-modules-dir=none",
            "--platform=browser",
            "--format=iife",
            "--sourcemap=external",
        ])
        .arg(format!(
            "--allow-import={}",
            config.allowed_imports.join(",")
        ))
        .arg(format!("--lock={}", lock_path.display()))
        .arg("--output")
        .arg(&output_path)
        .arg(&entry);
    run_bounded(command).await?;
    let javascript = String::from_utf8(read_bounded(&output_path).await?)?;
    let lock = serde_json::from_slice(&read_bounded(&lock_path).await?)?;
    let source_map_text =
        String::from_utf8(read_bounded(&output_path.with_extension("js.map")).await?)?;
    let mut source_map: Value = serde_json::from_str(&source_map_text)?;
    for source in source_map["sources"]
        .as_array_mut()
        .context("bundle source graph missing")?
    {
        let name = source
            .as_str()
            .context("bundle source name is not a string")?;
        if name.starts_with("https://") || name.starts_with("http://") {
            let url = deno_ast::ModuleSpecifier::parse(name)?;
            let host = format!(
                "{}:{}",
                url.host_str().context("module host missing")?,
                url.port_or_known_default().context("module port missing")?
            );
            ensure!(
                config.allowed_imports.contains(&host),
                "dependency origin is not allowed: {host}"
            );
        } else {
            let path = root
                .join(name)
                .canonicalize()
                .with_context(|| format!("resolve bundled source {name}"))?;
            let relative = path
                .strip_prefix(&root)
                .context("dependency escaped admission source directory")?;
            *source = Value::String(format!("loom:///{}", relative.to_string_lossy()));
        }
    }
    let artifact = ScriptArtifact {
        version: 1,
        source: source.into(),
        language: language.into(),
        compiler: BUNDLE_COMPILER.into(),
        javascript,
        lock,
        import_origins: config.allowed_imports.clone(),
        source_map,
    };
    artifact.validate()?;
    validate_script(&artifact.javascript)?;
    Ok(artifact)
}

async fn read_bounded(path: &std::path::Path) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await?;
    ensure!(
        file.metadata().await?.len() <= MAX_ARTIFACT_BYTES as u64,
        "compiler artifact exceeds size limit"
    );
    let mut bytes = Vec::new();
    file.take(MAX_ARTIFACT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= MAX_ARTIFACT_BYTES,
        "compiler artifact exceeds size limit"
    );
    Ok(bytes)
}

async fn run_bounded(mut command: tokio::process::Command) -> Result<()> {
    use std::process::Stdio;
    use tokio::io::AsyncReadExt;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn().context("start pinned module compiler")?;
    let stderr = child
        .stderr
        .take()
        .context("compiler diagnostics pipe missing")?;
    let mut diagnostics = Vec::new();
    let result = tokio::time::timeout(Duration::from_secs(60), async {
        stderr.take(65537).read_to_end(&mut diagnostics).await?;
        ensure!(
            diagnostics.len() <= 65536,
            "compiler diagnostics exceed size limit"
        );
        let status = child.wait().await?;
        ensure!(
            status.success(),
            "Deno module admission failed: {}",
            String::from_utf8_lossy(&diagnostics)
        );
        Ok::<_, anyhow::Error>(())
    })
    .await;
    match result {
        Ok(Ok(())) => Ok(()),
        other => {
            let _ = child.kill().await;
            match other {
                Ok(Err(error)) => Err(error),
                Err(error) => Err(error).context("module admission timed out"),
                _ => unreachable!(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_syntax_instead_of_import_text() -> Result<()> {
        assert!(!requires_bundle(
            "// import x from 'file:///secret'\nconst main = () => 'import x';",
            "typescript"
        )?);
        assert!(requires_bundle(
            "import {x} from 'npm:example@1'; const main = () => x",
            "typescript"
        )?);
        assert!(requires_bundle("export function main() {}", "javascript")?);
        assert!(
            requires_bundle(
                "async function main() { return import('npm:x') }",
                "javascript"
            )
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn source_only_typescript_needs_no_external_compiler() -> Result<()> {
        let artifact = compile(
            "function main(value: number): number { return value + 1; }",
            "typescript",
            &Compiler::with_executable("/does/not/exist"),
        )
        .await?;
        assert!(!artifact.javascript.contains(": number"));
        artifact.validate()?;
        validate_script(&artifact.javascript)?;
        Ok(())
    }

    #[tokio::test]
    async fn local_and_unapproved_http_imports_fail_before_compiler() {
        for specifier in [
            "file:///etc/secret.ts",
            "./secret.ts",
            "http://127.0.0.1:1234/secret.ts",
            "node:fs",
        ] {
            let result = compile(
                &format!("import x from {specifier:?}; function main() {{ return x }}"),
                "javascript",
                &Compiler::with_executable("/does/not/exist"),
            )
            .await;
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("unsupported import")
            );
        }
    }

    #[tokio::test]
    async fn artifact_rejects_runtime_fetch_syntax_and_unknown_compiler() -> Result<()> {
        let mut artifact = compile(
            "function main() { return 1 }",
            "javascript",
            &Compiler::default(),
        )
        .await?;
        artifact.javascript =
            "function main() { return import('https://example.test/a.js') }".into();
        assert!(validate_script(&artifact.javascript).is_err());
        artifact.javascript = "function main() { return 1 }".into();
        artifact.compiler = "unrecognized".into();
        assert!(artifact.validate().is_err());
        Ok(())
    }
}
