# S3 / MinIO live test

The S3 backend (`AmazonS3Builder` with ETag conditional put) compiles and is exercised only through the local conditional store adapter. Run the two-node takeover test against a real MinIO started by `nix run .#minio` (to add), with credentials from env.

Done when: `lease_fences_stale_owner` and `takeover_resumes_at_cursor` pass against MinIO on hydra and on a Linux CI host; the doc records the observed conditional-put latency.
