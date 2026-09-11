use std::sync::atomic::{AtomicU32, Ordering};

#[loom::def(effects = ["now"])]
pub fn main() -> loom::Value {
    let borrowed = AtomicU32::new(0);
    loom::handle(["bench.borrow"], |effect, _| {
        if effect.args.as_bool() == Some(true) {
            return loom::Reply::Resume(loom::Value::Null);
        }
        // Report through an ordinary recorded root effect. The native control
        // never guesses SDK closure layout or adds a diagnostic guest import.
        let address = &borrowed as *const AtomicU32 as usize;
        let _: loom::Value = loom::perform("now",
            loom::serde_json::json!({"borrow_address": address})).expect("witness");
        loop {
            borrowed.fetch_add(1, Ordering::SeqCst);
            std::hint::spin_loop();
        }
    }, || {
        let _: loom::Value = loom::perform("bench.borrow", loom::Value::Bool(true)).expect("warm handler cache");
        loom::scope(|scope| {
            let job = scope.spawn(|| {
                loom::perform::<loom::Value>("bench.borrow", loom::Value::Bool(false)).expect("borrowed callback")
            }).expect("child");
            job.join().expect("child result")
        })
    }).expect("handler")
}
