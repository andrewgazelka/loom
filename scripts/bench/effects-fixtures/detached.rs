pub fn main(mode: u32) -> u32 {
    match mode {
        0 => {
            // Expected: returns 7 without waiting 10 seconds. The host cancels
            // the detached task at entry return, with no unfinished-scope error.
            let handle = loom::spawn(|| loom::sleep(10_000)).expect("spawn");
            drop(handle);
            7
        }
        1 => {
            // Expected: 42. A different fiber in the same execution may join.
            let handle = loom::spawn(|| 42).expect("spawn");
            loom::scope(|scope| {
                scope.spawn(move || handle.join().expect("detached join"))
                    .expect("scoped spawn").join().expect("scoped join")
            })
        }
        2 => {
            // Expected: 1, with the full "shared job ..." diagnostic available
            // as Err and the caller still able to return successfully.
            let handle = loom::spawn(|| std::panic::panic_any("detached trap")).expect("spawn");
            u32::from(handle.join().unwrap_err().contains("shared job"))
        }
        _ => {
            // Expected: 9 even if the detached trap happens before entry return.
            drop(loom::spawn(|| std::panic::panic_any("unjoined detached trap")).expect("spawn"));
            loom::sleep(10).expect("caller survives");
            9
        }
    }
}
