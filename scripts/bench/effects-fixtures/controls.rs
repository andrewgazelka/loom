use loom::{Continuation, Reply, Value};

use std::sync::atomic::{AtomicU32, Ordering};

pub fn main(mode: String) -> Value {
    match mode.as_str() {
        "fake" => loom::handle_any(|_, _| Reply::Resume(loom::serde_json::to_value(73).expect("encode value")), || loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(2000).expect("encode value")); loom::Value::Object(map) }).expect("fake sleep")).expect("handle"),
        "forward" => loom::handle_any(|_, _| Reply::Forward, || loom::now().expect("host now")).expect("handle"),
        "nested" => loom::handle_any(|_, _| Reply::Resume(loom::serde_json::to_value(11).expect("encode value")), || {
            let shadow = loom::handle_any(|_, _| Reply::Resume(loom::serde_json::to_value(22).expect("encode value")), || loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(2000).expect("encode value")); loom::Value::Object(map) }).expect("shadow")).expect("inner");
            let forward = loom::handle_any(|_, _| Reply::Forward, || loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(2000).expect("encode value")); loom::Value::Object(map) }).expect("forward")).expect("inner");
            let restored = loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(2000).expect("encode value")); loom::Value::Object(map) }).expect("restored outer");
            loom::Value::Array(Vec::from([loom::serde_json::to_value(shadow).expect("encode value"),loom::serde_json::to_value(forward).expect("encode value"),loom::serde_json::to_value(restored).expect("encode value")]))
        }).expect("outer"),
        "total-forward" => loom::handle(["sleep"], |_, _| Reply::Forward, || loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(0).expect("encode value")); loom::Value::Object(map) }).expect("total forward must fail")).expect("handle"),
        "snapshot" => {
            let entered = AtomicU32::new(0);
            loom::handle_any(|_, _| Reply::Resume(loom::serde_json::to_value(31).expect("encode value")), || loom::scope(|scope| {
                let first = scope.spawn(|| {
                    while entered.load(Ordering::Acquire) == 0 {std::hint::spin_loop();}
                    loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(2000).expect("encode value")); loom::Value::Object(map) }).expect("spawn-time outer")
                }).expect("first spawn");
                let second = loom::handle_any(|_, _| Reply::Resume(loom::serde_json::to_value(47).expect("encode value")), || {
                    entered.store(1, Ordering::Release);
                    let child = scope.spawn(|| loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(2000).expect("encode value")); loom::Value::Object(map) }).expect("spawn-time inner")).expect("second spawn");
                    child.join().expect("second join")
                }).expect("inner");
                loom::Value::Array(Vec::from([loom::serde_json::to_value(first.join().expect("first join")).expect("encode value"),loom::serde_json::to_value(second).expect("encode value")]))
            })).expect("outer")
        },
        "inherit" => loom::handle_any(|_, _| Reply::Resume(loom::serde_json::to_value(91).expect("encode value")), || loom::scope(|scope| {
            scope.spawn(|| loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(2000).expect("encode value")); loom::Value::Object(map) }).expect("child sleep")).expect("spawn child").join().expect("child result")
        })).expect("handle"),
        "trap" => loom::handle_any(|_, _| {loom::now().expect("callback entered witness"); std::panic::panic_any("handler trap control")}, || loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(2000).expect("encode value")); loom::Value::Object(map) }).expect("sleep")).expect("handle"),
        "catch-drop" => {
            let error = loom::handle(["bench.drop"], |_, k| {drop(k); Reply::Deferred}, || loom::perform::<Value>("bench.drop", Value::Null).expect_err("drop must return error")).expect("handle");
            loom::sleep(0).expect("root still usable after drop");
            loom::serde_json::to_value(error).expect("encode value")
        },
        "drop" => loom::handle_any(|_, k| {drop(k); Reply::Deferred}, || loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(2000).expect("encode value")); loom::Value::Object(map) }).expect("sleep")).expect("handle"),
        "abandon" => {
            let active = AtomicU32::new(0);
            loom::handle_any(|_, k| {k.abandon().expect("abandon"); Reply::Deferred}, || loom::scope(|scope| {
                let _spin = scope.spawn(|| loop {active.fetch_add(1, Ordering::Relaxed); std::hint::spin_loop();}).expect("spawn child");
                while active.load(Ordering::Relaxed) == 0 {std::hint::spin_loop();}
                loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(2000).expect("encode value")); loom::Value::Object(map) }).expect("abandoned performer")
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
                let a = scope.spawn(|| loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(1).expect("encode value")); loom::Value::Object(map) }).expect("first")).expect("spawn child");
                let b = scope.spawn(|| loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(2).expect("encode value")); loom::Value::Object(map) }).expect("second")).expect("spawn child");
                Vec::from([a.join().expect("child result"), b.join().expect("child result")])
            })).expect("handle");
            let expected = loom::scope(|scope| {
                let first = scope.spawn(|| loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(1).expect("encode value")); loom::Value::Object(map) }).expect("first sleep")).expect("spawn child");
                let second = scope.spawn(|| loom::perform::<loom::Value>("sleep", { let mut map=loom::serde_json::Map::new(); map.insert("ms".into(), loom::serde_json::to_value(2).expect("encode value")); loom::Value::Object(map) }).expect("second sleep")).expect("spawn child");
                Vec::from([first.join().expect("first result"), second.join().expect("second result")])
            });
            { let mut map=loom::serde_json::Map::new(); map.insert("actual".into(), loom::serde_json::to_value(actual).expect("encode value"));map.insert("expected".into(), loom::serde_json::to_value(expected).expect("encode value")); loom::Value::Object(map) }
        },
        "perf" => loom::handle(["bench.tick"], |_, _| Reply::Resume(loom::serde_json::to_value(1).expect("encode value")), || {
            let mut sum = 0_u64;
            for _ in 0..10_000 {sum += loom::perform::<u64>("bench.tick", Value::Null).expect("tick");}
            loom::serde_json::to_value(sum).expect("encode value")
        }).expect("handle"),
        _ => std::panic::panic_any("unknown control"),
    }
}
