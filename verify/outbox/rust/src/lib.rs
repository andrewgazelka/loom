//! Pure core of outbox delivery, written in the subset Aeneas translates to Lean.
//!
//! Loom delivers a committed outbox row by injecting it into the receiver under
//! its key, then marking the row delivered in a second transaction. A crash
//! between the two leaves the row undelivered, so the pump sends it again; the
//! receiver's `applied` receipts make the second injection a no-op. `reset`
//! starts a new incarnation and must carry the receipts over, or a redelivered
//! row is handled twice.
//!
//! No IO, no async, no traits: the shell loads rows, calls `step`, and persists
//! the result in one transaction. `lean/` proves what `step` guarantees.

pub struct Msg {
    pub key: u64,
    pub payload: u64,
}

pub struct Receiver {
    /// Idempotency receipts; survive reset.
    pub applied: Vec<u64>,
    /// Keys whose handler ran. External effects, so reset cannot take them back.
    pub handled: Vec<u64>,
    pub generation: u64,
}

pub struct World {
    pub outbox: Vec<Msg>,
    /// Parallel to `outbox`.
    pub delivered: Vec<bool>,
    pub receiver: Receiver,
}

pub enum Event {
    /// Deliver outbox row `i` if it is not yet marked delivered.
    Pump(usize),
    /// Mark row `i` delivered once the receiver holds its receipt.
    Ack(usize),
    /// New receiver incarnation.
    Reset,
}

pub fn contains(keys: &Vec<u64>, key: u64) -> bool {
    let mut i = 0;
    while i < keys.len() {
        if keys[i] == key {
            return true;
        }
        i += 1;
    }
    false
}

pub fn inject(r: &mut Receiver, key: u64) {
    if contains(&r.applied, key) {
        return;
    }
    r.applied.push(key);
    r.handled.push(key);
}

pub fn reset(r: &mut Receiver) {
    r.generation += 1;
}

/// The planted bug: a reset that forgets the receipts. `lean/` shows a trace
/// that makes it handle one key twice.
pub fn reset_forgetting_receipts(r: &mut Receiver) {
    r.applied = Vec::new();
    r.generation += 1;
}

pub fn step(w: &mut World, e: Event) {
    match e {
        Event::Pump(i) => {
            if i < w.outbox.len() && i < w.delivered.len() && !w.delivered[i] {
                let key = w.outbox[i].key;
                inject(&mut w.receiver, key);
            }
        }
        Event::Ack(i) => {
            if i < w.outbox.len() && i < w.delivered.len() {
                let key = w.outbox[i].key;
                if contains(&w.receiver.applied, key) {
                    w.delivered[i] = true;
                }
            }
        }
        Event::Reset => reset(&mut w.receiver),
    }
}
