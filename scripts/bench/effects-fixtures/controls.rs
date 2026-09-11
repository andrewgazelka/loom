use loom::{Continuation, Reply, Value};
use loom::serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};

#[loom::def(effects = ["sleep", "now", "bench.tick", "bench.drop"])]
pub fn main(mode: String) -> Value {
    match mode.as_str() {
        "fake" => loom::handle_any(|_, _| Reply::Resume(json!(73)), || loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 2000})).expect("fake sleep")).expect("handle"),
        "forward" => loom::handle_any(|_, _| Reply::Forward, || loom::now().expect("host now")).expect("handle"),
        "nested" => loom::handle_any(|_, _| Reply::Resume(json!(11)), || {
            let shadow = loom::handle_any(|_, _| Reply::Resume(json!(22)), || loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 2000})).expect("shadow")).expect("inner");
            let forward = loom::handle_any(|_, _| Reply::Forward, || loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 2000})).expect("forward")).expect("inner");
            let restored = loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 2000})).expect("restored outer");
            json!([shadow, forward, restored])
        }).expect("outer"),
        "total-forward" => loom::handle(["sleep"], |_, _| Reply::Forward, || loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 0})).expect("total forward must fail")).expect("handle"),
        "snapshot" => {
            let entered = AtomicU32::new(0);
            loom::handle_any(|_, _| Reply::Resume(json!(31)), || loom::scope(|scope| {
                let first = scope.spawn(|| {
                    while entered.load(Ordering::Acquire) == 0 {std::hint::spin_loop();}
                    loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 2000})).expect("spawn-time outer")
                }).expect("first spawn");
                let second = loom::handle_any(|_, _| Reply::Resume(json!(47)), || {
                    entered.store(1, Ordering::Release);
                    let child = scope.spawn(|| loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 2000})).expect("spawn-time inner")).expect("second spawn");
                    child.join().expect("second join")
                }).expect("inner");
                json!([first.join().expect("first join"), second])
            })).expect("outer")
        },
        "inherit" => loom::handle_any(|_, _| Reply::Resume(json!(91)), || loom::scope(|scope| {
            scope.spawn(|| loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 2000})).expect("child sleep")).expect("spawn child").join().expect("child result")
        })).expect("handle"),
        "trap" => loom::handle_any(|_, _| {loom::now().expect("callback entered witness"); panic!("handler trap control")}, || loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 2000})).expect("sleep")).expect("handle"),
        "catch-drop" => {
            let error = loom::handle(["bench.drop"], |_, k| {drop(k); Reply::Deferred}, || loom::perform::<Value>("bench.drop", Value::Null).expect_err("drop must return error")).expect("handle");
            loom::sleep(0).expect("root still usable after drop");
            json!(error)
        },
        "drop" => loom::handle_any(|_, k| {drop(k); Reply::Deferred}, || loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 2000})).expect("sleep")).expect("handle"),
        "abandon" => {
            let active = AtomicU32::new(0);
            loom::handle_any(|_, k| {k.abandon().expect("abandon"); Reply::Deferred}, || loom::scope(|scope| {
                let _spin = scope.spawn(|| loop {active.fetch_add(1, Ordering::Relaxed); std::hint::spin_loop();}).expect("spawn child");
                while active.load(Ordering::Relaxed) == 0 {std::hint::spin_loop();}
                loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 2000})).expect("abandoned performer")
            })).expect("handle")
        },
        "deferred" => {
            struct Pending {args: Value, continuation: Continuation}
            let mut queue: Vec<Pending> = Vec::new();
            let actual = loom::handle(["sleep"], move |effect, continuation| {
                queue.push(Pending {args: effect.args, continuation});
                if queue.len() == 2 {
                    for pending in queue.drain(..) {
                        let value: Value = loom::perform("sleep", pending.args).expect("outer sleep");
                        pending.continuation.resume(value).expect("deferred resume");
                    }
                }
                Reply::Deferred
            }, || loom::scope(|scope| {
                let a = scope.spawn(|| loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 1})).expect("first")).expect("spawn child");
                let b = scope.spawn(|| loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 2})).expect("second")).expect("spawn child");
                vec![a.join().expect("child result"), b.join().expect("child result")]
            })).expect("handle");
            let expected = loom::scope(|scope| {
                let first = scope.spawn(|| loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 1})).expect("first sleep")).expect("spawn child");
                let second = scope.spawn(|| loom::perform::<loom::Value>("sleep", loom::serde_json::json!({"ms": 2})).expect("second sleep")).expect("spawn child");
                vec![first.join().expect("first result"), second.join().expect("second result")]
            });
            json!({"actual":actual,"expected":expected})
        },
        "perf" => loom::handle(["bench.tick"], |_, _| Reply::Resume(json!(1)), || {
            let mut sum = 0_u64;
            for _ in 0..10_000 {sum += loom::perform::<u64>("bench.tick", Value::Null).expect("tick");}
            json!(sum)
        }).expect("handle"),
        _ => panic!("unknown control"),
    }
}
