PRAGMA journal_mode=WAL;
PRAGMA foreign_keys=ON;
CREATE TABLE IF NOT EXISTS cas(hash TEXT PRIMARY KEY,kind TEXT NOT NULL,bytes BLOB NOT NULL,created_at INTEGER NOT NULL,codec INTEGER NOT NULL CHECK(codec IN (85,113)));
CREATE TABLE IF NOT EXISTS cas_codecs(hash TEXT NOT NULL REFERENCES cas(hash) ON DELETE CASCADE,codec INTEGER NOT NULL CHECK(codec IN (85,113)),PRIMARY KEY(hash,codec));
CREATE TABLE IF NOT EXISTS log(seq INTEGER PRIMARY KEY AUTOINCREMENT,actor TEXT NOT NULL,event_hash TEXT NOT NULL REFERENCES cas(hash),handler_seq INTEGER NOT NULL,ts INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS actor_log ON log(actor,seq);
CREATE TABLE IF NOT EXISTS defs(hash TEXT PRIMARY KEY,lang TEXT NOT NULL,name_hint TEXT,type_sig TEXT NOT NULL,component_hash TEXT,source_hash TEXT NOT NULL REFERENCES cas(hash));
CREATE TABLE IF NOT EXISTS def_deps(def_hash TEXT NOT NULL REFERENCES defs(hash),dep_hash TEXT NOT NULL,PRIMARY KEY(def_hash,dep_hash));
CREATE TABLE IF NOT EXISTS names(name TEXT NOT NULL,hash TEXT NOT NULL REFERENCES defs(hash),since_seq INTEGER NOT NULL REFERENCES log(seq),PRIMARY KEY(name,since_seq));
CREATE TABLE IF NOT EXISTS actors(id TEXT PRIMARY KEY,behavior_hash TEXT NOT NULL,lang TEXT NOT NULL,component_hash TEXT,last_seq INTEGER NOT NULL,created_seq INTEGER NOT NULL,parent TEXT);
CREATE TABLE IF NOT EXISTS snapshots(actor TEXT NOT NULL REFERENCES actors(id),fold_hash TEXT NOT NULL,seq INTEGER NOT NULL,state_hash TEXT NOT NULL REFERENCES cas(hash),PRIMARY KEY(actor,fold_hash,seq));
CREATE TABLE IF NOT EXISTS effect_results(desc_hash TEXT NOT NULL,scope TEXT NOT NULL,occurrence INTEGER NOT NULL,result_hash TEXT NOT NULL REFERENCES cas(hash),PRIMARY KEY(desc_hash,scope,occurrence));
CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,actor TEXT NOT NULL REFERENCES actors(id),owner TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS archive_segments(hash TEXT PRIMARY KEY REFERENCES cas(hash),first_seq INTEGER NOT NULL,last_seq INTEGER NOT NULL,event_count INTEGER NOT NULL);
DROP VIEW IF EXISTS effects;
DROP VIEW IF EXISTS messages;
DROP VIEW IF EXISTS events;
CREATE VIEW events AS
SELECT l.seq,l.actor,l.event_hash,l.handler_seq,l.ts,loom_json(c.bytes) AS bytes FROM log l JOIN cas c ON c.hash=l.event_hash WHERE NOT EXISTS(SELECT 1 FROM archive_segments a WHERE l.seq BETWEEN a.first_seq AND a.last_seq)
UNION ALL
SELECT json_extract(j.value,'$.seq'),json_extract(j.value,'$.actor'),NULL,json_extract(j.value,'$.handler_seq'),json_extract(j.value,'$.ts'),CAST(j.value -> '$.event' AS BLOB)
FROM archive_segments a JOIN cas c ON c.hash=a.hash JOIN json_each(loom_archive(c.bytes)) j;
CREATE VIEW messages AS SELECT * FROM events WHERE json_extract(bytes,'$.type')='message';
CREATE VIEW effects AS SELECT * FROM events WHERE json_extract(bytes,'$.type')='effect_recorded';
CREATE VIEW IF NOT EXISTS who_runs AS SELECT behavior_hash,id FROM actors;
CREATE VIEW IF NOT EXISTS name_history AS SELECT * FROM names;
CREATE VIEW IF NOT EXISTS deps AS SELECT * FROM def_deps;
CREATE TABLE IF NOT EXISTS inbox(actor TEXT NOT NULL REFERENCES actors(id),handler_seq INTEGER NOT NULL REFERENCES log(seq),msg TEXT NOT NULL,PRIMARY KEY(actor,handler_seq));
CREATE TABLE IF NOT EXISTS archive_entries(event_hash TEXT PRIMARY KEY,archive_hash TEXT NOT NULL REFERENCES cas(hash));
CREATE TABLE IF NOT EXISTS message_keys(key TEXT PRIMARY KEY,actor TEXT NOT NULL REFERENCES actors(id),handler_seq INTEGER NOT NULL REFERENCES log(seq),msg_hash TEXT NOT NULL REFERENCES cas(hash));
