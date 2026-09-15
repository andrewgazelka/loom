use anyhow::{Context, Result};
use axum::{Router, routing::get};
use loom_api::Service;
use loom_proto::{CommandRequest, Lang};
use loom_store::Store;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

fn service(store: Store, compiler: loom_imports::Compiler) -> Result<Service> {
    Ok(Service::new(
        store,
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        vec![Lang::TypeScript, Lang::JavaScript],
    )?
    .with_script_compiler(compiler))
}

async fn command(service: &Service, name: &str, args: Value) -> loom_proto::Response {
    service
        .command(CommandRequest {
            session: None,
            command: name.into(),
            args,
        })
        .await
}

#[tokio::test]
async fn imported_typescript_graph_is_pinned_and_runs_after_reopen_offline() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("store.sqlite");
    let hits = Arc::new(AtomicUsize::new(0));
    let observed = hits.clone();
    let constant_hits = hits.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/math.ts",
                get(move || {
                    let hits = observed.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        axum::http::Response::builder()
                            .header("content-type", "application/typescript")
                            .body(axum::body::Body::from("import { zero } from './constant.ts'; export const add = (a: number, b: number): number => a + b + zero;"))
                            .unwrap()
                    }
                }),
            ).route("/constant.ts", get(move || {
                let hits = constant_hits.clone();
                async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    axum::http::Response::builder()
                        .header("content-type", "application/typescript")
                        .body(axum::body::Body::from("export const zero: number = 0;")).unwrap()
                }
            })),
        )
        .await
    });
    let compiler = loom_imports::Compiler::default().allow_import(address.to_string());
    let app = service(Store::open(&path)?, compiler)?;
    let source = format!(
        "import {{ add }} from 'http://{address}/math.ts';\nexport function main(a: number,b: number): number {{ return add(a,b); }}"
    );
    let added = command(&app, "add", json!({"name":"sum","source":source})).await;
    assert!(added.ok, "{added:?}");
    assert!(
        hits.load(Ordering::SeqCst) >= 2,
        "transitive fixture dependencies were not fetched"
    );
    let definition = app.store.resolve("sum")?.context("module missing")?;
    let executable = app
        .store
        .executable_script(&definition.hash, &loom_v8::typescript_abi())?;
    assert_eq!(executable.source, source);
    assert!(executable.javascript.is_some());
    let run = command(&app, "run", json!({"target":"sum","args":[20,22]})).await;
    assert!(run.ok, "{run:?}");
    assert_eq!(run.result["output"], 42);
    drop(app);
    server.abort();
    let _ = server.await;
    // A deliberately absent compiler proves reopen executes pinned output.
    let reopened = service(
        Store::open(&path)?,
        loom_imports::Compiler::with_executable("/nonexistent/deno"),
    )?;
    let run = command(&reopened, "run", json!({"target":"sum","args":[1,2]})).await;
    assert!(run.ok, "{run:?}");
    assert_eq!(run.result["output"], 3);
    let hash = definition
        .component_hash
        .context("module artifact missing")?;
    let mut artifact: Value =
        serde_json::from_slice(&reopened.store.get(&hash)?.context("artifact missing")?)?;
    artifact["javascript"] = json!("function main(){return 999;}");
    let replacement = reopened
        .store
        .put("javascript_module", &serde_json::to_vec(&artifact)?)?;
    reopened.store.with_connection(|connection| {
        connection.execute(
            "UPDATE defs SET component_hash=? WHERE hash=?",
            [&replacement, &definition.hash],
        )?;
        Ok(())
    })?;
    assert!(
        reopened
            .store
            .executable_script(&definition.hash, &loom_v8::typescript_abi())
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn imports_never_fall_back_to_script_or_host_authority() -> Result<()> {
    let app = service(
        Store::memory()?,
        loom_imports::Compiler::with_executable("/nonexistent/deno"),
    )?;
    for source in [
        "import { readFileSync } from 'node:fs'; function main(){return readFileSync('/etc/passwd','utf8');}",
        "import secret from 'file:///etc/passwd'; function main(){return secret;}",
        "async function main(){return await import('https://esm.sh/escape');}",
        "import { value } from 'https://esm.sh/missing'; function main(){return value;}",
    ] {
        let added = command(&app, "add", json!({"name":"denied","source":source})).await;
        assert!(!added.ok, "{added:?}");
        assert!(app.store.resolve("denied")?.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn remote_dependencies_cannot_read_host_files() -> Result<()> {
    let outside = tempfile::tempdir()?;
    let secret_path = outside.path().join("secret.ts");
    let secret = "LOOM_PRIVATE_HOST_SENTINEL_643759";
    tokio::fs::write(&secret_path, format!("export const value = '{secret}';")).await?;
    let dependency = format!(
        "export {{ value }} from 'file://{}';",
        secret_path.display()
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/escape.ts",
                get(move || {
                    let source = dependency.clone();
                    async move {
                        axum::http::Response::builder()
                            .header("content-type", "application/typescript")
                            .body(axum::body::Body::from(source))
                            .unwrap()
                    }
                }),
            ),
        )
        .await
    });
    let app = service(
        Store::memory()?,
        loom_imports::Compiler::default().allow_import(address.to_string()),
    )?;
    let source = format!(
        "import {{ value }} from 'http://{address}/escape.ts'; export function main(){{return value;}}"
    );
    let added = command(&app, "add", json!({"name":"escape","source":source})).await;
    server.abort();
    let _ = server.await;
    assert!(
        !added.ok,
        "dependency escaped compiler read confinement: {added:?}"
    );
    assert!(
        !format!("{added:?}").contains(secret),
        "compiler leaked host file in diagnostics"
    );
    assert!(app.store.resolve("escape")?.is_none());
    Ok(())
}
