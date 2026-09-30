//! Hermetic actions against the real macOS/Linux sandbox. Tools are `/bin/sh` and `/bin/bash`
//! with shell builtins only, so the runtime closure is the shell itself.
use loom_action::{Action, Input, Runner};
use loom_store::Store;
use std::{collections::BTreeMap, path::PathBuf};

fn runner() -> (Runner, Store, tempfile::TempDir) {
    let store = Store::memory().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let runner = Runner::new(store.clone(), scratch.path()).unwrap();
    (runner, store, scratch)
}

fn shell(script: &str, inputs: BTreeMap<String, Input>, outputs: &[&str]) -> Action {
    Action {
        tool: PathBuf::from("/bin/sh"),
        tool_identity: String::new(),
        runtime: vec![PathBuf::from("/bin/sh")],
        args: vec!["-c".into(), script.into()],
        env: BTreeMap::new(),
        inputs,
        outputs: outputs.iter().map(|name| name.to_string()).collect(),
        network: false,
    }
}

fn input(store: &Store, text: &str) -> Input {
    Input {
        hash: store.put("blob", text.as_bytes()).unwrap(),
        executable: false,
    }
}

const COPY: &str = "read line < in.txt; printf '%s!\\n' \"$line\" > out.txt";

#[tokio::test]
async fn the_second_identical_action_is_answered_from_the_store_without_running() {
    let (runner, store, _scratch) = runner();
    let inputs = BTreeMap::from([("in.txt".to_owned(), input(&store, "hello"))]);
    let action = shell(COPY, inputs, &["out.txt"]);

    let first = runner.run(&action).await.unwrap();
    assert!(!first.cached);
    assert_eq!(first.result.exit_code, 0, "{}", String::from_utf8_lossy(&runner.read(&first.result.stderr).unwrap()));
    assert_eq!(runner.read(&first.result.outputs["out.txt"].hash).unwrap(), b"hello!\n");

    let second = runner.run(&action).await.unwrap();
    assert!(second.cached, "the same key must not run again");
    assert_eq!(second.key, first.key);
    assert_eq!(second.result, first.result);
    assert_eq!(runner.stats().runs, 1);
    assert_eq!(runner.stats().hits, 1);
    assert!(second.elapsed < first.elapsed, "{:?} vs {:?}", second.elapsed, first.elapsed);
}

#[tokio::test]
async fn any_keyed_field_changing_is_a_different_action() {
    let (runner, store, _scratch) = runner();
    let base = shell(
        COPY,
        BTreeMap::from([("in.txt".to_owned(), input(&store, "hello"))]),
        &["out.txt"],
    );
    async fn key(runner: &Runner, action: &Action) -> String {
        runner.run(action).await.unwrap().key
    }
    let original = key(&runner, &base).await;

    let mut other_input = base.clone();
    other_input.inputs.insert("in.txt".into(), input(&store, "world"));
    let mut other_env = base.clone();
    other_env.env.insert("A".into(), "1".into());
    let mut other_args = base.clone();
    other_args.args[1] = format!("{COPY} # changed");
    let mut other_identity = base.clone();
    other_identity.tool_identity = "sh 2".into();
    let mut networked = base.clone();
    networked.network = true;
    for (name, changed) in [
        ("input", other_input),
        ("env", other_env),
        ("args", other_args),
        ("tool identity", other_identity),
        ("network", networked),
    ] {
        assert_ne!(key(&runner, &changed).await, original, "changing the {name} kept the key");
    }
    assert_eq!(key(&runner, &base).await, original, "and the original is stable");
    assert_eq!(runner.stats().runs, 6, "every distinct action ran once, the repeat did not");
}

#[tokio::test]
async fn the_tool_sees_only_what_the_action_declares() {
    let (runner, store, _scratch) = runner();
    // An input outside the declared tree, a real file of the user's, must be unreadable.
    let secret = std::env::var("HOME").unwrap() + "/.zshrc";
    let probe = if std::path::Path::new(&secret).exists() { secret } else { "/etc/hosts".into() };
    // Control: unsandboxed, the same read succeeds.
    assert!(std::process::Command::new("/bin/sh").args(["-c", &format!("read line < '{probe}'")]).status().unwrap().success());
    let leak = shell(
        &format!("read line < '{probe}' && printf '%s\\n' \"$line\" > out.txt"),
        BTreeMap::new(),
        &["out.txt"],
    );
    let outcome = runner.run(&leak).await;
    let failed = match &outcome {
        Err(_) => true,
        Ok(outcome) => outcome.result.exit_code != 0,
    };
    assert!(failed, "the sandbox let the tool read {probe}: {outcome:?}");
    assert_eq!(runner.stats().failures, 1);
    let again = runner.run(&leak).await;
    assert!(again.is_err() || !again.unwrap().cached, "a failure is never cached");
    assert_eq!(runner.stats().runs, 2);

    // Nothing is inherited from the caller's environment.
    let env = shell("printf '[%s][%s]' \"$HOME\" \"$LOOM_TEST\" > out.txt", BTreeMap::new(), &["out.txt"]);
    let outcome = runner.run(&env).await.unwrap();
    assert_eq!(runner.read(&outcome.result.outputs["out.txt"].hash).unwrap(), b"[][]");
    let mut given = env.clone();
    given.env.insert("LOOM_TEST".into(), "yes".into());
    let outcome = runner.run(&given).await.unwrap();
    assert_eq!(runner.read(&outcome.result.outputs["out.txt"].hash).unwrap(), b"[][yes]");
    let _ = store;
}

#[tokio::test]
async fn writes_outside_the_scratch_directory_are_denied() {
    let (runner, _store, _scratch) = runner();
    let target = format!("/Volumes/Projects/tmp/loom-action-escape-{}", std::process::id());
    let _ = std::fs::remove_file(&target);
    let script = format!("printf x > '{target}'; printf y > out.txt");
    // Control: unsandboxed, the same script does write the target, so its absence below is the sandbox.
    let scratch = tempfile::tempdir().unwrap();
    assert!(std::process::Command::new("/bin/sh").args(["-c", &script]).current_dir(scratch.path()).status().unwrap().success());
    assert!(std::path::Path::new(&target).exists(), "the control script did not write the target");
    std::fs::remove_file(&target).unwrap();
    let action = shell(&script, BTreeMap::new(), &["out.txt"]);
    let _ = runner.run(&action).await;
    assert!(!std::path::Path::new(&target).exists(), "the tool wrote outside its scratch directory");
    // Positive control: the same script writing inside the scratch directory works.
    let inside = shell("printf y > out.txt", BTreeMap::new(), &["out.txt"]);
    assert_eq!(runner.run(&inside).await.unwrap().result.exit_code, 0);
}

#[tokio::test]
async fn a_declared_output_the_tool_did_not_write_is_an_error_not_a_cached_result() {
    let (runner, _store, _scratch) = runner();
    let action = shell("printf y > other.txt", BTreeMap::new(), &["out.txt"]);
    let error = runner.run(&action).await.unwrap_err().to_string();
    assert!(error.contains("out.txt"), "{error}");
    assert!(runner.run(&action).await.is_err());
    assert_eq!(runner.stats().hits, 0);
}

#[tokio::test]
async fn a_result_whose_blob_is_gone_is_a_miss() {
    let (runner, store, _scratch) = runner();
    let action = shell("printf y > out.txt", BTreeMap::new(), &["out.txt"]);
    let first = runner.run(&action).await.unwrap();
    let hash = first.result.outputs["out.txt"].hash.clone();
    store
        .with_connection(|c| Ok(c.execute("DELETE FROM cas WHERE hash=?", [&hash])?))
        .unwrap();
    let second = runner.run(&action).await.unwrap();
    assert!(!second.cached, "a recorded result with a missing output must not be served");
    assert_eq!(runner.stats().runs, 2);
}

#[tokio::test]
async fn the_network_is_denied_unless_the_action_grants_it() {
    // A loopback listener is the positive and negative control in one: the same script
    // connects with `network: true` and fails with `network: false`.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let accepting = std::thread::spawn(move || {
        for _ in 0..2 {
            let _ = listener.accept();
        }
    });
    let (runner, _store, _scratch) = runner();
    let script = format!("exec 3<>/dev/tcp/127.0.0.1/{port} && printf connected > out.txt");
    let mut action = shell(&script, BTreeMap::new(), &["out.txt"]);
    action.tool = PathBuf::from("/bin/bash");
    action.runtime = vec![PathBuf::from("/bin/bash")];
    action.network = true;
    let granted = runner.run(&action).await.unwrap();
    assert_eq!(granted.result.exit_code, 0, "{}", String::from_utf8_lossy(&runner.read(&granted.result.stderr).unwrap()));
    action.network = false;
    let denied = runner.run(&action).await;
    assert!(
        denied.is_err() || denied.unwrap().result.exit_code != 0,
        "the sandbox let a network-less action connect"
    );
    drop(accepting);
}

#[tokio::test]
async fn the_scratch_directory_token_is_substituted_but_does_not_change_the_key() {
    let (runner, _store, _scratch) = runner();
    let action = shell("printf '%s' \"$1\" > out.txt", BTreeMap::new(), &["out.txt"]);
    let mut action = action;
    action.args.extend(["sh".into(), "@ROOT@/x".into()]);
    let first = runner.run(&action).await.unwrap();
    let written = String::from_utf8(runner.read(&first.result.outputs["out.txt"].hash).unwrap()).unwrap();
    assert!(written.ends_with("/x") && !written.contains("@ROOT@"), "{written}");
    assert!(written.contains("action-"), "{written}");
    // Same action in a different runner (another scratch directory): same key.
    let (other, _store, _scratch) = self::runner();
    assert_eq!(other.run(&action).await.unwrap().key, first.key);
}
