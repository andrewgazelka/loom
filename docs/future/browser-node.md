# Browser as a node

Phase 2 of [UI as a view actor](../ui-view-actor.md) moves ephemeral view actors into the browser tab. The browser already consumes the view's `tree` table; the placement of that actor changes.

The first gate is:

```sh
cargo check -p loom-actor --target wasm32-unknown-unknown
```

This lane has not run that command. Its compiler errors define the porting work, including native runtime, I/O, networking, and object-store dependencies. The browser node owns only ephemeral actors, uses the memory VFS, and speaks capability-authorized ingress operations over WebSocket. Server actors retain their existing durability and placement policy.

Templates run through the browser's WebAssembly engine. The shared-memory guest ABI uses atomics and requires cross-origin isolation headers; see [shared core ABI](../shared-core-abi.md). The HTTP host must provide those headers before a browser node can instantiate templates.

Completion requires the target gate, actual browser execution of a view over a remote source, replay and resnapshot after reconnect, and the same keyed DOM identity tests. The current server-side view and binding core remain the protocol reference.
