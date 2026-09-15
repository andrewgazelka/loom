// Desired configuration survives a host restart; an incarnation's capability does not.
const LOOM_SCHEMA = "CREATE TABLE config(value TEXT); CREATE TABLE resource(cap TEXT); CREATE TABLE events(kind TEXT, body TEXT);";

async function startConfiguredVm() {
  const rows = await loom.sql('SELECT value FROM config');
  if (rows.length === 0) return;
  const config = JSON.parse(rows[0].value);
  const resource = await loom.vms.spawn({
    image: config.image,
    command: '/bin/sh',
    args: ['/work/probe.sh', String(config.port)],
    cwd: '/work',
    env: {LOOM_VM_PROBE: 'guest-env-雪', PATH: '/bin'},
    network: 'none',
    limits: {memoryMb: 256, cpus: 1, rootfsMb: 64},
    ttlMs: 120000,
    subscriber: await loom.actors.self(),
  });
  await loom.sql('DELETE FROM resource');
  await loom.sql('INSERT INTO resource VALUES (?)', [JSON.stringify(resource)]);
}

const main = loom.actor({
  async onStart() {
    await loom.sql("INSERT INTO events VALUES ('start', '')");
    await startConfiguredVm();
  },
  async onMessage(message: {type?: string, image?: {$ref: string}, port?: number, value?: string} | null) {
    if (message?.type === 'configure') {
      const prior = await loom.sql('SELECT value FROM config');
      if (prior.length !== 0) throw new Error('VM already configured');
      await loom.sql('INSERT INTO config VALUES (?)', [JSON.stringify({image:message.image, port:message.port})]);
      const raw = await loom.cas.put([0, 1, 127, 255]);
      const document = await loom.cas.putJson({purpose:'vm-poc', raw});
      await (await loom.actors.named('cas-receiver')).send({raw, document});
      await startConfiguredVm();
    } else if (message?.type === 'write') {
      const rows = await loom.sql('SELECT cap FROM resource');
      await loom.processes.get(JSON.parse(rows[0].cap)).write(message.value);
      await loom.sql("INSERT INTO events VALUES ('input', ?)", [message.value]);
    } else if (message?.type) {
      await loom.sql('INSERT INTO events VALUES (?, ?)', [message.type, JSON.stringify(message)]);
    }
  },
  async onStop(reason: string) {
    const rows = await loom.sql('SELECT cap FROM resource');
    if (rows.length) await loom.processes.get(JSON.parse(rows[0].cap)).cancel();
    await loom.sql("INSERT INTO events VALUES ('stop', ?)", [reason]);
  },
});
