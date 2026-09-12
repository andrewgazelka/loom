use super::*;

struct Answer {
    calls: AtomicU64,
}
impl RootHandler for Answer {
    fn handle<'a>(&'a self, _request: Request<'a>, _next: Next<'a>) -> HandlerFuture<'a> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::Relaxed);
            EffectOutput::value(&json!(41))
        })
    }
}
struct Increment;
impl RootHandler for Increment {
    fn handle<'a>(&'a self, request: Request<'a>, next: Next<'a>) -> HandlerFuture<'a> {
        Box::pin(async move {
            let output = next.run(request).await?.decode()?;
            EffectOutput::value(&json!(output.as_u64().context("integer required")? + 1))
        })
    }
}

#[tokio::test]
async fn recording_wraps_forwarding_and_replay_skips_downstream_handlers() -> Result<()> {
    let runtime = Runtime::new(Store::memory()?)?;
    let answer = Answer {
        calls: AtomicU64::new(0),
    };
    let increment = Increment;
    let handlers: [&dyn RootHandler; 3] = [&RECORDING, &increment, &answer];
    let execution = trace::ExecutionTrace::fresh("composed");
    let output = Next {
        handlers: &handlers,
    }
    .run(Request {
        runtime: &runtime,
        desc: json!({"op":"test.answer"}),
        scope: "composed",
        occurrence: 0,
        effects: EffectContext {
            trace: Some(execution.clone()),
            ..Default::default()
        },
    })
    .await?;
    assert_eq!(output.decode()?, json!(42));
    let result = Ok(output);
    let replay = trace::ExecutionTrace::loaded(execution.snapshot(Some(&result), true)?)?;
    let output = Next {
        handlers: &handlers,
    }
    .run(Request {
        runtime: &runtime,
        desc: json!({"op":"test.answer"}),
        scope: "composed",
        occurrence: 0,
        effects: EffectContext {
            trace: Some(replay),
            ..Default::default()
        },
    })
    .await?;
    assert_eq!(output.decode()?, json!(42));
    assert_eq!(answer.calls.load(Ordering::Relaxed), 1);
    Ok(())
}

#[tokio::test]
async fn recording_skips_permitted_call_but_records_denied_call() -> Result<()> {
    let runtime = Runtime::new(Store::memory()?)?;
    let answer = Answer {
        calls: AtomicU64::new(0),
    };
    let handlers: [&dyn RootHandler; 2] = [&RECORDING, &answer];
    for permitted in [true, false] {
        let execution = trace::ExecutionTrace::fresh("root");
        let labels = if permitted {
            vec!["call".into()]
        } else {
            vec![]
        };
        let outcome = Next {
            handlers: &handlers,
        }
        .run(Request {
            runtime: &runtime,
            desc: json!({"op":"call","args":{"def":"child","args":[]}}),
            scope: "root",
            occurrence: 0,
            effects: EffectContext {
                trace: Some(execution.clone()),
                ..EffectContext::default().delegated("parent", Some(&labels))
            },
        })
        .await;
        assert_eq!(outcome.is_ok(), permitted);
        let bundle = execution.snapshot(Some(&outcome), true)?;
        assert_eq!(bundle.trace.entries.len(), if permitted { 0 } else { 1 });
        assert!(bundle.observations.is_empty());
        if !permitted {
            assert!(outcome.unwrap_err().to_string().contains("not allowed"));
        }
    }
    assert_eq!(answer.calls.load(Ordering::Relaxed), 1);
    Ok(())
}

#[test]
fn inferred_rows_intersect_capabilities_instead_of_expanding_them() {
    let effects = EffectContext::default()
        .delegated("def", Some(&["sleep".into(), "fs.read".into()]))
        .with_inferred(&["sleep".into(), "exec".into()]);
    assert!(effects.permits("sleep"));
    assert!(!effects.permits("fs.read"));
    assert!(!effects.permits("exec"));
    let pure = EffectContext::default().with_inferred(&[]);
    assert!(!pure.permits("sleep"));
    let inferred = EffectContext::default().with_inferred(&["sleep".into()]);
    assert!(inferred.permits("sleep"));
    assert!(!inferred.permits("exec"));
}

#[tokio::test]
async fn removed_scheduling_combinators_are_unsupported_effects_at_root() -> Result<()> {
    for op in ["all", "race", "fork", "join", "spawn", "send"] {
        let runtime = Runtime::new(Store::memory()?)?;
        let error = runtime
            .perform(json!({"op": op, "args": {}}), "root", 0)
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), format!("unsupported effect: {op}"));
    }
    Ok(())
}
