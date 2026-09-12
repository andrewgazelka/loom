trait Pause {
    fn pause(&self, milliseconds: u64);
}

struct Clock;

impl Pause for Clock {
    fn pause(&self, milliseconds: u64) {
        loom::sleep(milliseconds).unwrap();
    }
}

fn wait<T: Pause>(clock: T, milliseconds: u64) {
    clock.pause(milliseconds);
}

pub fn sleeper(milliseconds: u64) {
    wait(Clock, milliseconds);
}
