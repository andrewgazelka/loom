use super::*;
use crate::Store;

#[test]
fn pending_reader_waits_until_a_full_queue_accepts_the_effect() -> Result<()> {
    let store = Store::memory()?;
    let connection = store.connection.lock().unwrap();
    let bytes = encode(&Value::Null)?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    // One batch waits on SQLite; the bounded channel is then completely full.
    for _ in 0..4096 + 8192 {
        store.recording.send(Record::Value {
            kind: "desc".into(),
            bytes: bytes.clone(),
            hash: hash.clone(),
        })?;
    }
    let (published, publication) = mpsc::channel();
    let (observed, observation) = mpsc::channel();
    let (completed, completion) = mpsc::channel();
    thread::scope(|scope| -> Result<()> {
        let producer = scope.spawn(|| -> Result<()> {
            let writer = &store.recording;
            let _submission = writer.publication()?;
            let key = EffectKey {
                desc_hash: "full".into(),
                scope: "scope".into(),
                occurrence: 0,
            };
            writer
                .shared
                .pending
                .lock()
                .unwrap()
                .insert(key.clone(), Value::Null);
            published.send(())?;
            writer.send(Record::Effect { key, bytes, hash })
        });
        publication.recv()?;
        let reader = scope.spawn(|| -> Result<()> {
            let value = store.effect_get("full", "scope", 0)?;
            observed.send(())?;
            store.flush()?;
            completed.send(value)?;
            Ok(())
        });
        let early = observation.recv_timeout(Duration::from_millis(20));
        drop(connection);
        producer.join().expect("producer panicked")?;
        reader.join().expect("reader panicked")?;
        assert!(matches!(early, Err(mpsc::RecvTimeoutError::Timeout)));
        assert_eq!(completion.recv()?, Some(Value::Null));
        Ok(())
    })?;
    assert_eq!(store.effect_get("full", "scope", 0)?, Some(Value::Null));
    Ok(())
}
