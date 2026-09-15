// The smoke test loads this file and substitutes only the configured image.
// Keep desired configuration separate from the capability for this activation.
const LOOM_SCHEMA = "CREATE TABLE config(image TEXT); INSERT INTO config VALUES ('busybox@sha256:9db7b59979c38555a39def84a31fb98b5296952f9e3afd4f6f11f05b07adfab0'); CREATE TABLE resource(cap TEXT); CREATE TABLE events(kind TEXT, body TEXT);";

const main = loom.actor({
  async onStart() {
    const config = await loom.sql('SELECT image FROM config');
    const resource = await loom.containers.spawn({
      image: config[0].image,
      command: '/bin/sh',
      args: ['-c', 'printf "ready\\n"; cat'],
      network: 'none',
      ttlMs: 120000,
      subscriber: await loom.actors.self(),
    });
    await loom.sql('DELETE FROM resource');
    await loom.sql('INSERT INTO resource VALUES (?)', [JSON.stringify(resource)]);
    await loom.sql("INSERT INTO events VALUES ('start', '')");
  },
  async onMessage(message: {type?: string, value?: string} | null) {
    if (message?.type === 'write') {
      const rows = await loom.sql('SELECT cap FROM resource');
      await loom.processes.get(JSON.parse(rows[0].cap)).write(message.value);
      await loom.sql("INSERT INTO events VALUES ('input', ?)", [message.value]);
    } else if (message?.type) {
      await loom.sql('INSERT INTO events VALUES (?, ?)', [message.type, JSON.stringify(message)]);
    }
  },
  async onStop(reason: string) {
    const rows = await loom.sql('SELECT cap FROM resource');
    await loom.processes.get(JSON.parse(rows[0].cap)).cancel();
    await loom.sql("INSERT INTO events VALUES ('stop', ?)", [reason]);
  },
});
