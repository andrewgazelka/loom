//! Explicit runtime admission must override packaged defaults as one typed set.
use super::Args;
use clap::Parser;
use std::path::PathBuf;

#[test]
fn explicit_vm_runtime_survives_packaged_defaults() {
    let mut args = Args::try_parse_from([
        "loomd",
        "--token",
        "test-token",
        "--vm-runner",
        "/trusted/runner",
        "--vm-library",
        "/trusted/libkrun.so",
        "--vm-bwrap",
        "/trusted/bwrap",
        "--vm-runtime-root",
        "/trusted/runtime-a",
        "--vm-runtime-root",
        "/trusted/runtime-b",
    ])
    .unwrap();
    args.vm_defaults().unwrap();
    assert_eq!(args.vm_runner, Some(PathBuf::from("/trusted/runner")));
    assert_eq!(args.vm_library, Some(PathBuf::from("/trusted/libkrun.so")));
    assert_eq!(args.vm_bwrap, Some(PathBuf::from("/trusted/bwrap")));
    assert_eq!(
        args.vm_runtime_root,
        vec![
            PathBuf::from("/trusted/runtime-a"),
            PathBuf::from("/trusted/runtime-b")
        ]
    );
}

#[test]
fn vm_runtime_root_list_uses_path_separator_at_cli_boundary() {
    let mut args = Args::try_parse_from([
        "loomd",
        "--token",
        "test-token",
        "--vm-runner",
        "/trusted/runner",
        "--vm-library",
        "/trusted/libkrun.so",
        "--vm-bwrap",
        "/trusted/bwrap",
        "--vm-runtime-root",
        "/trusted/runtime-a:/trusted/runtime-b",
    ])
    .unwrap();
    args.vm_defaults().unwrap();
    assert_eq!(
        args.vm_runtime_root,
        vec![
            PathBuf::from("/trusted/runtime-a"),
            PathBuf::from("/trusted/runtime-b")
        ]
    );
}

#[test]
fn incomplete_vm_configuration_requires_available_packaged_defaults() {
    // Start with explicit values so this test does not mutate or depend on the
    // runtime environment that other daemon tests share.
    let mut args = Args::try_parse_from([
        "loomd",
        "--token",
        "test-token",
        "--vm-runner",
        "/trusted/runner",
        "--vm-library",
        "/trusted/library",
        "--vm-bwrap",
        "/trusted/bwrap",
        "--vm-runtime-root",
        "/trusted/runtime",
    ])
    .unwrap();
    args.vm_library = None;
    args.vm_bwrap = None;
    args.vm_runtime_root.clear();
    let result = args.vm_defaults();
    if option_env!("LOOM_VM_LIBRARY").is_none()
        || option_env!("LOOM_VM_BWRAP").is_none()
        || option_env!("LOOM_VM_RUNTIME_ROOTS_FILE").is_none()
    {
        assert!(
            result.is_err(),
            "partial configuration enabled VM execution"
        );
    } else {
        // Packaged builds are allowed to fill missing options. Their closure
        // file must exist and contain roots before VM configuration succeeds.
        result.unwrap();
        assert!(!args.vm_runtime_root.is_empty());
        assert!(args.vm_library.is_some() && args.vm_bwrap.is_some());
    }
    assert_eq!(args.vm_runner, Some(PathBuf::from("/trusted/runner")));
}
