use loom::{Continuation, Desc, Reply, Value};
use loom::serde_json::json;
use std::sync::atomic::{AtomicU32, Ordering};

#[loom::def(effects = ["sleep", "now", "bench.tick", "bench.drop", "all"])]
pub fn main(mode: String) -> Value {
    match mode.as_str() {
        "fake" => loom::handle(|_, _| Reply::Resume(json!(73)), || loom::abilities::sleep(2000).expect("fake sleep")).expect("handle"),
        "forward" => loom::handle(|_, _| Reply::Forward, || loom::abilities::now().expect("host now")).expect("handle"),
        "nested" => loom::handle(|_, _| Reply::Resume(json!(11)), || {
            let shadow = loom::handle(|_, _| Reply::Resume(json!(22)), || loom::abilities::sleep(2000).expect("shadow")).expect("inner");
            let forward = loom::handle(|_, _| Reply::Forward, || loom::abilities::sleep(2000).expect("forward")).expect("inner");
            let restored = loom::abilities::sleep(2000).expect("restored outer");
            json!([shadow, forward, restored])
        }).expect("outer"),
        "total-forward" => loom::handle_labels(["sleep"], |_, _| Reply::Forward, || loom::abilities::sleep(0).expect("total forward must fail")).expect("handle"),
        "snapshot" => {
            let entered = AtomicU32::new(0);
            loom::handle(|_, _| Reply::Resume(json!(31)), || loom::scope(|scope| {
                let first = scope.fork(|| {
                    while entered.load(Ordering::Acquire) == 0 {std::hint::spin_loop();}
                    loom::abilities::sleep(2000).expect("fork-time outer")
                }).expect("first fork");
                let second = loom::handle(|_, _| Reply::Resume(json!(47)), || {
                    entered.store(1, Ordering::Release);
                    let child = scope.fork(|| loom::abilities::sleep(2000).expect("fork-time inner")).expect("second fork");
                    child.join().expect("second join")
                }).expect("inner");
                json!([first.join().expect("first join"), second])
            }).expect("scope")).expect("outer")
        },
        "inherit" => loom::handle(|_, _| Reply::Resume(json!(91)), || loom::scope(|scope| {
            scope.fork(|| loom::abilities::sleep(2000).expect("child sleep")).expect("fork").join().expect("join")
        }).expect("scope")).expect("handle"),
        "trap" => loom::handle(|_, _| {loom::abilities::now().expect("callback entered witness"); panic!("handler trap control")}, || loom::abilities::sleep(2000).expect("sleep")).expect("handle"),
        "catch-drop" => {
            let error = loom::handle_labels(["bench.drop"], |_, k| {drop(k); Reply::Deferred}, || loom::perform::<Value>(Desc::new("bench.drop", Value::Null)).expect_err("drop must return error")).expect("handle");
            loom::abilities::sleep(0).expect("root still usable after drop");
            json!(error)
        },
        "drop" => loom::handle(|_, k| {drop(k); Reply::Deferred}, || loom::abilities::sleep(2000).expect("sleep")).expect("handle"),
        "abandon" => {
            let active = AtomicU32::new(0);
            loom::handle(|_, k| {k.abandon().expect("abandon"); Reply::Deferred}, || loom::scope(|scope| {
                let _spin = scope.fork(|| loop {active.fetch_add(1, Ordering::Relaxed); std::hint::spin_loop();}).expect("fork");
                while active.load(Ordering::Relaxed) == 0 {std::hint::spin_loop();}
                loom::abilities::sleep(2000).expect("abandoned performer")
            }).expect("scope")).expect("handle")
        },
        "deferred" => {
            struct Pending {args: Value, continuation: Continuation}
            let mut queue: Vec<Pending> = Vec::new();
            let actual = loom::handle_labels(["sleep"], move |op, continuation| {
                queue.push(Pending {args: op.args, continuation});
                if queue.len() == 2 {
                    for pending in queue.drain(..) {
                        let value: Value = loom::perform(Desc::new("sleep", pending.args)).expect("outer sleep");
                        pending.continuation.resume(value).expect("deferred resume");
                    }
                }
                Reply::Deferred
            }, || loom::scope(|scope| {
                let a = scope.fork(|| loom::abilities::sleep(1).expect("first")).expect("fork");
                let b = scope.fork(|| loom::abilities::sleep(2).expect("second")).expect("fork");
                vec![a.join().expect("join"), b.join().expect("join")]
            }).expect("scope")).expect("handle");
            let expected = loom::all([loom::abilities::sleep::desc(1), loom::abilities::sleep::desc(2)]).expect("host all");
            json!({"actual":actual,"expected":expected})
        },
        "perf" => loom::handle_labels(["bench.tick"], |_, _| Reply::Resume(json!(1)), || {
            let mut sum = 0_u64;
            for _ in 0..10_000 {sum += loom::perform::<u64>(Desc::new("bench.tick", Value::Null)).expect("tick");}
            json!(sum)
        }).expect("handle"),
        _ => panic!("unknown control"),
    }
}
