PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS cas(hash TEXT PRIMARY KEY,kind TEXT NOT NULL,bytes BLOB NOT NULL,created_at INTEGER NOT NULL,codec INTEGER NOT NULL CHECK(codec IN (85,113)));
CREATE TABLE IF NOT EXISTS cas_codecs(hash TEXT NOT NULL REFERENCES cas(hash) ON DELETE CASCADE,codec INTEGER NOT NULL CHECK(codec IN (85,113)),PRIMARY KEY(hash,codec));
CREATE TABLE IF NOT EXISTS definition_records(seq INTEGER PRIMARY KEY AUTOINCREMENT,event_hash TEXT NOT NULL REFERENCES cas(hash),ts INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS defs(hash TEXT PRIMARY KEY,lang TEXT NOT NULL,name_hint TEXT,type_sig TEXT NOT NULL,component_hash TEXT,source_hash TEXT NOT NULL REFERENCES cas(hash),allowed_effects TEXT);
CREATE TABLE IF NOT EXISTS def_deps(def_hash TEXT NOT NULL REFERENCES defs(hash),dep_hash TEXT NOT NULL,PRIMARY KEY(def_hash,dep_hash));
CREATE TABLE IF NOT EXISTS names(name TEXT NOT NULL,hash TEXT NOT NULL REFERENCES defs(hash),since_seq INTEGER NOT NULL REFERENCES definition_records(seq),PRIMARY KEY(name,since_seq));
CREATE TABLE IF NOT EXISTS effect_results(desc_hash TEXT NOT NULL,scope TEXT NOT NULL,occurrence INTEGER NOT NULL,result_hash TEXT NOT NULL REFERENCES cas(hash),PRIMARY KEY(desc_hash,scope,occurrence));
DROP VIEW IF EXISTS effects;
DROP VIEW IF EXISTS trace_effects;
DROP VIEW IF EXISTS definition_events;
CREATE VIEW definition_events AS SELECT seq,event_hash,ts,loom_json(c.bytes) AS bytes FROM definition_records JOIN cas c ON c.hash=event_hash;

CREATE VIEW IF NOT EXISTS name_history AS SELECT * FROM names;
CREATE VIEW IF NOT EXISTS deps AS SELECT * FROM def_deps;
CREATE TABLE IF NOT EXISTS def_effects(def_hash TEXT NOT NULL,op TEXT NOT NULL,PRIMARY KEY(def_hash,op));

CREATE TABLE IF NOT EXISTS call_traces(scope TEXT PRIMARY KEY,trace_hash TEXT NOT NULL REFERENCES cas(hash),completed INTEGER NOT NULL CHECK(completed IN (0,1)),last_seq INTEGER NOT NULL REFERENCES definition_records(seq));

CREATE VIEW trace_effects AS
SELECT t.last_seq AS seq,t.trace_hash AS event_hash,l.ts,
CAST(json_object('type','effect_completed','scope',json_extract(j.value,'$.key.scope'),'occurrence',json_extract(j.value,'$.key.occurrence'),'desc_hash',json_extract(j.value,'$.descriptor_hash'),'result_hash',json_extract(j.value,'$.outcome.result_hash'),'error',json_extract(j.value,'$.outcome.message'),'status',json_extract(j.value,'$.outcome.status'),'op',json_extract(loom_json(d.bytes),'$.op'),'trace_hash',t.trace_hash,'definition_hash',json_extract(loom_trace_json(c.bytes),'$.definition_hash')) AS BLOB) AS bytes
FROM call_traces t JOIN cas c ON c.hash=t.trace_hash JOIN definition_events l ON l.seq=t.last_seq
JOIN json_each(loom_trace_json(c.bytes),'$.entries') j
LEFT JOIN cas d ON d.hash=json_extract(j.value,'$.descriptor_hash');
CREATE VIEW effects AS SELECT * FROM definition_events WHERE json_extract(bytes,'$.type')='effect_recorded'
UNION ALL SELECT * FROM trace_effects;
CREATE TABLE IF NOT EXISTS machine_roots(id TEXT PRIMARY KEY,root TEXT NOT NULL,identity TEXT NOT NULL);
