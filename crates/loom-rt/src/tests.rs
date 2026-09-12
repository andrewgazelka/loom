use super::*;

#[test]
fn resolve_self_rewrites_actor_spawn_but_leaves_actor_send_and_other_defs_alone() {
    let hash = "definition-hash";
    let mut spawn = json!({"op":"actor.spawn","args":{"def":"$self","state":0}});
    resolve_self(&mut spawn, hash);
    assert_eq!(spawn["args"]["def"], json!(hash));
    let mut spawn_other = json!({"op":"actor.spawn","args":{"def":"other-hash","state":0}});
    resolve_self(&mut spawn_other, hash);
    assert_eq!(spawn_other["args"]["def"], json!("other-hash"));
    let mut send = json!({"op":"actor.send","args":{"def":"$self"}});
    resolve_self(&mut send, hash);
    assert_eq!(
        send["args"]["def"],
        json!("$self"),
        "actor.send must not resolve $self"
    );
}
#[tokio::test]
async fn scoped_children_record_independently_and_replay_in_reverse_order() -> Result<()> {
    let runtime = Runtime::new(Store::memory()?)?;
    let execution = trace::ExecutionTrace::fresh("root");
    let effects = EffectContext {
        trace: Some(execution.clone()),
        ..Default::default()
    };
    let descriptor = json!({"op":"random"});
    let first = runtime.dispatch_root(descriptor.clone(), "root/spawn:0", 0, effects.clone());
    let second = runtime.dispatch_root(descriptor.clone(), "root/spawn:1", 0, effects);
    let outputs = futures::future::try_join_all([first, second]).await?;
    let values = outputs
        .iter()
        .map(EffectOutput::decode)
        .collect::<Result<Vec<_>>>()?;
    let result = EffectOutput::value(&json!(values));
    let bundle = execution.snapshot(Some(&result), true)?;
    assert_eq!(bundle.trace.entries.len(), 2);
    assert_eq!(bundle.trace.entries[0].key.scope, "root/spawn:0");
    assert_eq!(bundle.trace.entries[1].key.scope, "root/spawn:1");
    let replay = trace::ExecutionTrace::loaded(bundle)?;
    for index in [1, 0] {
        let output = runtime
            .dispatch_root(
                descriptor.clone(),
                &format!("root/spawn:{index}"),
                0,
                EffectContext {
                    trace: Some(replay.clone()),
                    ..Default::default()
                },
            )
            .await?;
        assert_eq!(output.decode()?, values[index]);
    }
    replay.snapshot(Some(&result), true)?;
    Ok(())
}
#[tokio::test]
async fn observational_effect_replays_across_runtime_restart() -> Result<()> {
    let store = Store::memory()?;
    let runtime = Runtime::new(store.clone())?;
    let descriptor = json!({"op":"random","args":{}});
    let first = runtime.perform(descriptor.clone(), "handler-1", 0).await?;
    drop(runtime);
    let runtime = Runtime::new(store)?;
    assert_eq!(
        first,
        runtime.perform(descriptor.clone(), "handler-1", 0).await?
    );
    assert_ne!(first, runtime.perform(descriptor, "handler-2", 0).await?);
    Ok(())
}
#[tokio::test]
async fn guest_cannot_claim_observation_is_hermetic() -> Result<()> {
    let runtime = Runtime::new(Store::memory()?)?;
    let descriptor = json!({"op":"random","args":{},"class":"hermetic"});
    assert_ne!(
        runtime.perform(descriptor.clone(), "a", 0).await?,
        runtime.perform(descriptor, "b", 0).await?
    );
    Ok(())
}
#[tokio::test]
async fn keyed_exec_runs_once_across_actors() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("count");
    let runtime = Runtime::new(Store::memory()?)?;
    let descriptor = json!({"op":"exec","args":{"program":"sh","args":["-c","printf x >> \"$1\"","loom",path],"key":"once"}});
    let results = futures::future::try_join_all([
        runtime.perform(descriptor.clone(), "actor-one", 0),
        runtime.perform(descriptor, "actor-two", 0),
    ])
    .await?;
    assert_eq!(results[0]["code"], json!(0));
    assert_eq!(results[0], results[1]);
    assert_eq!(std::fs::read_to_string(path)?, "x");
    Ok(())
}
#[tokio::test]
async fn exec_capture_preserves_actual_before_after_cas_bytes() -> Result<()> {
    let root = tempfile::tempdir()?;
    std::fs::write(root.path().join("note"), b"before\n")?;
    let runtime = Runtime::new(Store::memory()?)?;
    let mut command = tokio::process::Command::new("sh");
    command
        .current_dir(root.path())
        .args(["-c", "printf 'after\n' > note"]);
    let result = runtime
        .execute_command(command, vec!["note".into()])
        .await?;
    let changes = result["filesystem_changes"].as_array().context("changes")?;
    assert_eq!(changes.len(), 1, "{result}");
    assert_eq!(
        runtime
            .inner
            .store
            .get(changes[0]["before"].as_str().context("before CID")?)?,
        Some(b"before\n".to_vec())
    );
    assert_eq!(
        runtime
            .inner
            .store
            .get(changes[0]["after"].as_str().context("after CID")?)?,
        Some(b"after\n".to_vec())
    );
    Ok(())
}
#[tokio::test]
async fn policy_denies_dynamic_cached_and_delegated_requests() -> Result<()> {
    let runtime = Runtime::new(Store::memory()?)?;
    let desc = json!({"op":"cas.put","args":{"secret":42}});
    runtime.perform(desc.clone(), "warm", 0).await?;
    let denied = EffectContext::default().delegated("restricted", Some(&[]));
    let error = runtime
        .dispatch_root(desc.clone(), "denied", 0, denied.clone())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("not allowed"));
    let parent =
        EffectContext::default().delegated("parent", Some(&["sleep".into(), "call".into()]));
    let child = parent.delegated("child", Some(&["sleep".into(), "cas.put".into()]));
    assert!(!child.permits("cas.put"));
    assert!(child.permits("sleep"));
    assert!(
        !parent
            .delegated("unrestricted-child", None)
            .permits("cas.put")
    );
    assert!(
        runtime
            .dispatch_root(desc, "delegated", 0, child)
            .await
            .is_err()
    );
    Ok(())
}
#[tokio::test]
async fn cancellation_persists_an_explicit_cancelled_trace() -> Result<()> {
    let store = Store::memory()?;
    let runtime = Runtime::new(store.clone())?;
    assert!(
        tokio::time::timeout(
            Duration::from_millis(10),
            runtime.perform(
                json!({"op":"sleep","args":{"ms":5000}}),
                "cancelled-root",
                0
            )
        )
        .await
        .is_err()
    );
    let bundle = store
        .load_call_trace("cancelled-root")?
        .context("cancelled trace missing")?;
    assert!(matches!(
        bundle.trace.outcome,
        Some(loom_proto::TraceOutcome::Cancelled)
    ));
    assert_eq!(bundle.trace.entries.len(), 1);
    assert!(matches!(
        bundle.trace.entries[0].outcome,
        loom_proto::TraceOutcome::Cancelled
    ));
    Ok(())
}
#[tokio::test]
async fn effect_observations_follow_delegated_definition_and_exclude_denials() -> Result<()> {
    let runtime = Runtime::new(Store::memory()?)?;
    let execution = trace::ExecutionTrace::fresh("root");
    let parent = EffectContext {
        def_hash: Some("parent".into()),
        trace: Some(execution.clone()),
        ..EffectContext::default()
    };
    runtime
        .dispatch_root(
            json!({"op":"sleep","args":{"ms":0}}),
            "root",
            0,
            parent.clone(),
        )
        .await?;
    runtime
        .dispatch_root(
            json!({"op":"sleep","args":{"ms":0}}),
            "root/child",
            0,
            parent.delegated("child", None),
        )
        .await?;
    assert!(
        runtime
            .dispatch_root(
                json!({"op":"now"}),
                "root/denied",
                0,
                parent.delegated("denied", Some(&[]))
            )
            .await
            .is_err()
    );
    let bundle = execution.snapshot(None, true)?;
    assert_eq!(
        bundle.observations,
        vec![
            loom_proto::TraceObservation {
                definition_hash: "child".into(),
                op: "sleep".into()
            },
            loom_proto::TraceObservation {
                definition_hash: "parent".into(),
                op: "sleep".into()
            },
        ]
    );
    Ok(())
}
#[tokio::test]
async fn concurrent_execution_of_one_scope_publishes_one_observation() -> Result<()> {
    let store = Store::memory()?;
    let runtime = Runtime::new(store.clone())?;
    let descriptor = json!({"op":"random"});
    let outputs = futures::future::try_join_all(
        (0..8).map(|_| runtime.perform(descriptor.clone(), "same-scope", 0)),
    )
    .await?;
    assert!(outputs.iter().all(|output| output == &outputs[0]));
    let events = store.events(Some("system"), 0, 100)?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event["type"] == "call_completed")
            .count(),
        1
    );
    Ok(())
}
#[tokio::test]
async fn fresh_observations_do_not_accumulate_global_locks_or_effect_rows() -> Result<()> {
    let store = Store::memory()?;
    let runtime = Runtime::new(store.clone())?;
    for index in 0..8 {
        runtime
            .perform(json!({"op":"random"}), &format!("fresh-{index}"), 0)
            .await?;
    }
    assert!(runtime.inner.effect_locks.lock().unwrap().is_empty());
    let events = store.events(Some("system"), 0, 1000)?;
    assert_eq!(
        events
            .iter()
            .filter(|event| event.event["type"] == "call_completed")
            .count(),
        8
    );
    assert!(
        events
            .iter()
            .all(|event| event.event["type"] != "effect_recorded")
    );
    Ok(())
}
#[tokio::test]
async fn rejects_unadmitted_artifact_before_execution() -> Result<()> {
    let store = Store::memory()?;
    let artifact = store.put("component", b"\0asm\x0d\0\x01\0")?;
    let source = "rejected artifact fixture";
    let deps = Default::default();
    let hash = blake3::hash(&loom_proto::definition_identity(
        loom_proto::Lang::Rust,
        source,
        &deps,
        None,
    )?)
    .to_hex()
    .to_string();
    store.define(
        &loom_proto::Def {
            hash: hash.clone(),
            lang: loom_proto::Lang::Rust,
            component_hash: Some(artifact.clone()),
            sig: Default::default(),
            allowed_effects: None,
            observed_effects: Vec::new(),
        },
        None,
        source,
        &deps,
    )?;
    let runtime = Runtime::new(store)?;
    let error = runtime
        .call_def(&hash, json!([]))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains(&artifact), "{error}");
    assert!(error.contains(&hash), "{error}");
    assert!(
        error.contains("not an admitted core wasm module"),
        "{error}"
    );
    Ok(())
}
