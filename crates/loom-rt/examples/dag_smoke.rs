mod support;
use anyhow::{Context, Result, ensure};
use loom_proto::{DAG_CBOR_CODEC, Lang, RAW_CODEC, Value};
use loom_rt::Runtime;
use loom_store::Store;
use serde_json::json;

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let rust_path = args.next().context("Rust DAG component required")?;
    let ts_path = args.next().context("TS DAG component required")?;
    let store = Store::memory()?;
    let rust = support::register(&store, Lang::Rust, rust_path, "Rust DAG fixture")?;
    let ts = support::register(&store, Lang::Ts, ts_path, "TS DAG fixture")?;
    let runtime = Runtime::new(store.clone())?;
    let value = json!({"meaning":42,"nested":[true,null,"content"]});
    let descriptor = json!({"op":"cas.put","args":value});
    let reference = runtime.perform(descriptor.clone(), "dag", 0).await?;
    ensure!(
        reference.as_object().context("reference object")?.len() == 1,
        "reference has metadata fields"
    );
    let cid = reference["$ref"]
        .as_str()
        .context("CID reference required")?;
    ensure!(
        store.codec(cid)? == Some(DAG_CBOR_CODEC),
        "structured CAS codec must be DAG-CBOR"
    );
    let encoded = loom_proto::encode(&reference).map_err(anyhow::Error::msg)?;
    ensure!(
        encoded.starts_with(&[0xd8, 0x2a]),
        "reference lacks CBOR tag 42"
    );
    let descriptor_hash =
        blake3::hash(&loom_proto::encode(&descriptor).map_err(anyhow::Error::msg)?)
            .to_hex()
            .to_string();
    ensure!(
        store.get_value::<Value>(&descriptor_hash)? == Some(descriptor),
        "effect was not persisted as canonical DAG-CBOR"
    );
    for direction in [
        Direction {
            name: "Rust to TS",
            caller: rust.clone(),
            target: ts.clone(),
        },
        Direction {
            name: "TS to Rust",
            caller: ts,
            target: rust,
        },
    ] {
        let result = runtime
            .call_def(&direction.caller, json!([reference, direction.target]))
            .await?;
        ensure!(
            result == json!({"reference":reference,"value":value,"echo":reference}),
            "{} reference exchange mismatch: {result}",
            direction.name
        );
        println!(
            "{} native tag-42 reference exchange and CAS dereference pass",
            direction.name
        );
    }
    let nested = json!({"link":reference});
    let nested_ref = runtime
        .perform(json!({"op":"cas.put","args":nested}), "dag", 1)
        .await?;
    let nested_cid = nested_ref["$ref"].as_str().context("nested CID missing")?;
    let dereferenced = runtime
        .perform(json!({"op":"cas.get","args":{"hash":nested_cid}}), "dag", 2)
        .await?;
    ensure!(
        dereferenced == nested,
        "nested reference failed CAS roundtrip"
    );

    let directory = tempfile::tempdir()?;
    std::fs::write(directory.path().join("file"), b"raw bytes")?;
    let machine = runtime.create_machine(directory.path())?;
    let tree_ref = runtime
        .perform(
            json!({"op":"fs.snapshot","args":{"machine":machine.id,"path":"/"}}),
            "dag",
            3,
        )
        .await?;
    let tree_cid = tree_ref["$ref"].as_str().context("tree CID missing")?;
    let tree: Value = store.get_value(tree_cid)?.context("tree missing")?;
    ensure!(
        store.codec(tree_cid)? == Some(DAG_CBOR_CODEC),
        "tree must use DAG-CBOR"
    );
    let file_ref = &tree["entries"][0]["reference"];
    let file_cid = file_ref["$ref"]
        .as_str()
        .context("tree file reference missing")?;
    ensure!(
        store.codec(file_cid)? == Some(RAW_CODEC),
        "file must use raw codec"
    );
    ensure!(
        store.get(file_cid)?.as_deref() == Some(b"raw bytes".as_slice()),
        "raw file reference did not resolve"
    );
    println!("nested CAS references and DAG tree to raw-blob links pass");
    Ok(())
}
struct Direction {
    name: &'static str,
    caller: String,
    target: String,
}
