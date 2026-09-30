//! `eval`: compile one Rust cell and run one of its entries in a single call.
//!
//! `add` and `update` are the durable path: a named revision, dependents
//! rebuilt, a staged copy of the store committed atomically. A REPL cell needs
//! none of that. `eval` compiles into the live store without binding a name
//! (the cell is addressed by its hash) and without staging a copy of the whole
//! store, then calls the entry, and reports where the time went.
use super::*;
use crate::definitions::resolve_dependency_pins;

/// The lineage every `eval` of one session builds in: its incremental
/// workspace stays warm from one cell to the next.
const DEFAULT_SESSION: &str = "repl";

impl Service {
    /// Build and run a trivial cell so the first `eval` a client sends finds
    /// the dependency graph, the compiler process and the module cache warm.
    /// On a fresh build cache the graph is 75 compiles (about 20 s); every cell
    /// after it takes under 100 ms. `loomd` runs this once after it listens.
    pub async fn prewarm_repl(&self) -> Result<()> {
        self.eval(&json!({"source":"0"}), None).await.map(|_| ())
    }

    pub(super) async fn eval(&self, args: &Value, token: Option<call_cancel::CancelToken>) -> Result<Value> {
        let started = Instant::now();
        self.access.require(Scope::Execute)?;
        let source = field(args, "source")?;
        let session = match args.get("session").filter(|value| !value.is_null()) {
            Some(value) => value.as_str().context("session must be a string")?,
            None => DEFAULT_SESSION,
        };
        ensure!(session.len() <= 64, "session name is longer than 64 bytes");
        crate::bundles::validate_name(session)?;
        let deps: BTreeMap<String, String> = args
            .get("deps")
            .filter(|value| !value.is_null())
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()?
            .unwrap_or_default();
        let allowed_effects = args
            .get("allowed_effects")
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()?
            .flatten();
        let profile = match args.get("profile").filter(|value| !value.is_null()) {
            Some(value) => match value.as_str().context("profile must be a string")? {
                "interactive" => loom_build::BuildProfile::Interactive,
                "standard" => loom_build::BuildProfile::Standard,
                other => bail!("profile is interactive or standard, not {other:?}"),
            },
            // `optimize: true` is the older spelling of `profile: "standard"`.
            None if args.get("optimize").and_then(Value::as_bool).unwrap_or(false) => {
                loom_build::BuildProfile::Standard
            }
            None => loom_build::BuildProfile::Interactive,
        };
        let cell = loom_build::interactive_cell(source);
        let wrapped = matches!(cell, std::borrow::Cow::Owned(_));
        let mut request = DefineRequest {
            lang: Lang::Rust,
            name: session.into(),
            source: crate::definitions::with_crates(args, Lang::Rust, cell.clone().into_owned())?,
            deps,
            allowed_effects,
        };
        request.deps = resolve_dependency_pins(&self.store, &request.deps)?;

        // The build runs as its own task: if this call is cancelled while it builds, the build still
        // finishes and is cached (dropping the future would kill the warm compiler server), and only the
        // run after it is abandoned. A build still waiting for the gate when cancelled is skipped.
        let response = {
            let service = self.clone();
            let gate_token = token.clone();
            let build = tokio::spawn(async move {
                let _guard = service.definitions_gate.lock().await;
                if gate_token.as_ref().is_some_and(call_cancel::CancelToken::is_cancelled) {
                    anyhow::bail!("call cancelled before it started building");
                }
                service.define_ephemeral(request, profile).await
            });
            match &token {
                Some(token) => tokio::select! {
                    built = build => built??,
                    () = token.cancelled() => anyhow::bail!("call cancelled"),
                },
                None => build.await??,
            }
        };
        let built = started.elapsed();
        if !response.ok {
            let mut response = response;
            // The compiler reads the cell unparsed (comments and blank lines dropped, statements
            // re-wrapped), so its line numbers count that text; report lines of the text the caller sent.
            let map = loom_check::rust_original_lines(&cell);
            for diagnostic in &mut response.diagnostics {
                // Only rustc's diagnostics count the compiler's text. The checker's (LOOM_*, RUST_PARSE) carry a
                // placeholder location that is not a line of anything.
                let from_rustc =
                    !(diagnostic.code.starts_with("LOOM_") || diagnostic.code == "RUST_PARSE");
                let own_file = from_rustc
                    && matches!(diagnostic.file.as_str(), "" | "src/lib.rs" | "compiled.rs");
                if let Some(original) = map
                    .as_ref()
                    .filter(|_| own_file)
                    .and_then(|map| map.get((diagnostic.line as usize).checked_sub(1)?))
                {
                    diagnostic.line = *original as usize;
                }
                if wrapped {
                    // The header line is ours; the caller's text starts on the next.
                    diagnostic.line = diagnostic.line.saturating_sub(1).max(1);
                }
            }
            return Err(CompileFailure {
                message: render_failure(&response),
                diagnostics: response.diagnostics,
            }
            .into());
        }
        let hash = response.result["def"]["hash"]
            .as_str()
            .context("built definition hash missing")?
            .to_owned();
        let def = self
            .store
            .resolve(&hash)?
            .context("built definition is missing from the store")?;
        let entry = match args.get("entry").filter(|value| !value.is_null()) {
            Some(value) => {
                let name = value.as_str().context("entry must be a string")?;
                ensure!(
                    def.sig.exports.iter().any(|export| export.name == name),
                    "entry {name:?} is not exported; exports: {}",
                    export_names(&def)
                );
                name
            }
            None => match def.sig.exports.as_slice() {
                [only] => only.name.as_str(),
                _ => bail!(
                    "the cell exports several entries; pass entry. exports: {}",
                    export_names(&def)
                ),
            },
        };
        let run_started = Instant::now();
        let mut result = self
            .run_entry(
                &def,
                entry,
                args.get("args").cloned().unwrap_or_else(|| json!([])),
                false,
            )
            .await?;
        result["build"] = response.result["build"].clone();
        // The compiler sees the cell unparsed (no comments, statements re-wrapped), so the lines in
        // `run`'s `sites` count that text. `line_map[n - 1]` is the line of the cell you sent where
        // line `n` of the compiler's text starts.
        if let Some(map) = loom_check::rust_original_lines(&cell) {
            let header = u32::from(wrapped);
            result["line_map"] = json!(
                map.iter()
                    .map(|line| line.saturating_sub(header).max(1))
                    .collect::<Vec<_>>()
            );
        }
        result["timings_ms"] = json!({
            "compile": u64::try_from(built.as_millis()).unwrap_or(u64::MAX),
            "run": u64::try_from(run_started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "total": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        });
        Ok(result)
    }
}

/// A cell that did not compile: the text an agent reads, and the structured
/// diagnostics that travel beside it in `Response.diagnostics`.
#[derive(Debug)]
pub(crate) struct CompileFailure {
    pub message: String,
    pub diagnostics: Vec<loom_proto::Diagnostic>,
}

impl std::fmt::Display for CompileFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CompileFailure {}

fn export_names(def: &Def) -> String {
    def.sig
        .exports
        .iter()
        .map(|export| export.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// The compile failure as text an agent reads directly: each diagnostic as
/// `line:col: message`, then the compiler's own log when it produced none.
fn render_failure(response: &Response) -> String {
    let mut lines = vec!["compile failed".to_owned()];
    for diagnostic in &response.diagnostics {
        // Code the host generated (the entry wrapper macros) reports a file of
        // the SDK, whose line numbers mean nothing in the cell.
        let file = match diagnostic.file.as_str() {
            "" | "src/lib.rs" | "compiled.rs" => String::new(),
            other => format!("{other}:"),
        };
        lines.push(format!(
            "{file}{}:{}: {}{}",
            diagnostic.line,
            diagnostic.col,
            diagnostic.message,
            diagnostic
                .snippet
                .as_deref()
                .map(|snippet| format!("\n{snippet}"))
                .unwrap_or_default()
        ));
    }
    if response.diagnostics.is_empty()
        && let Some(logs) = response.result["build"]["logs"].as_str()
    {
        lines.push(logs.to_owned());
    }
    lines.join("\n")
}
