use loom_proto::{
    CallTrace, TraceEntry, TraceKey, TraceOutcome, decode_call_trace, encode_call_trace,
};

fn trace() -> CallTrace {
    let scope = "call:012345678901234567890123456789".to_owned();
    CallTrace {
        version: 1,
        definition_hash: Some("11".repeat(32)),
        args_hash: Some("22".repeat(32)),
        scope: scope.clone(),
        entries: (0..257)
            .map(|occurrence| TraceEntry {
                key: TraceKey {
                    scope: format!("{scope}/all:0/call:{occurrence}"),
                    occurrence,
                },
                descriptor_hash: format!("{occurrence:064x}"),
                outcome: TraceOutcome::Success {
                    result_hash: "33".repeat(32),
                },
            })
            .collect(),
        outcome: Some(TraceOutcome::Success {
            result_hash: "44".repeat(32),
        }),
    }
}
#[test]
fn compact_trace_roundtrips_under_32_kib() {
    let mut trace = trace();
    trace
        .entries
        .sort_by(|left, right| left.key.cmp(&right.key));
    let bytes = encode_call_trace(&trace).unwrap();
    assert!(bytes.len() <= 32 * 1024, "{} bytes", bytes.len());
    assert_eq!(decode_call_trace(&bytes).unwrap(), trace);
}
#[test]
fn trace_order_is_canonical_and_invalid_keys_are_rejected() {
    let mut trace = trace();
    let bytes = encode_call_trace(&trace).unwrap();
    trace.entries.reverse();
    assert_eq!(encode_call_trace(&trace).unwrap(), bytes);
    trace.entries.push(trace.entries[0].clone());
    assert!(encode_call_trace(&trace).is_err());
    trace.entries.pop();
    trace.entries[0].key.scope = "outside".into();
    assert!(encode_call_trace(&trace).is_err());
    assert!(decode_call_trace(&bytes[..bytes.len() - 1]).is_err());
}
#[test]
fn failure_and_partial_trace_outcomes_are_preserved() {
    let mut trace = trace();
    trace.entries.truncate(2);
    trace.entries[0].outcome = TraceOutcome::Error {
        message: "failed".into(),
    };
    trace.entries[1].outcome = TraceOutcome::Cancelled;
    trace.outcome = None;
    assert_eq!(
        decode_call_trace(&encode_call_trace(&trace).unwrap()).unwrap(),
        trace
    );
}
#[test]
fn trace_metadata_and_error_limits_are_admission_checks() {
    let mut data = trace();
    data.entries[0].outcome = TraceOutcome::Error {
        message: "x".repeat(loom_proto::TRACE_MAX_ERROR_BYTES + 1),
    };
    assert!(
        encode_call_trace(&data)
            .unwrap_err()
            .contains("error limit")
    );
    let mut data = trace();
    data.entries[0].key.scope = "x".repeat(loom_proto::TRACE_MAX_SCOPE_BYTES + 1);
    assert!(
        encode_call_trace(&data)
            .unwrap_err()
            .contains("scope limit")
    );
    assert!(
        decode_call_trace(&vec![0; loom_proto::TRACE_MAX_METADATA_BYTES + 1])
            .unwrap_err()
            .contains("encoded byte limit")
    );
}
