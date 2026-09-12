# CI

No CI configuration exists in the repository. Needed: a Linux job (io_uring build, `cargo test --workspace`, clippy, fmt, nix eval with `--builders ''`, docs site build) and the three ignored loom-rt tests with their fixture environment (`LOOM_SCAN_FIXTURE`, `LOOM_SHARED_TEST_MODULE`, `LOOM_HANDLER_BENCH_MODULE`).

Done when: one CI run prints the same `test result:` lines as the local gate on hydra, plus the three previously ignored tests.
