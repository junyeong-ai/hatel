-- A ledger the hatel 0.25.0 binary wrote (emit, emit-batch with context, a plugin Kind with a
-- redacted field, pending deliveries, a receiver projection), dumped with `sqlite3 .dump`. The
-- repository path is replaced by /work/repo. 0.25.0 stamps it `PRAGMA user_version = 1`.
PRAGMA foreign_keys=OFF;
BEGIN TRANSACTION;
CREATE TABLE records (
    ts TEXT NOT NULL, kind TEXT NOT NULL, schema_version INTEGER NOT NULL, payload TEXT NOT NULL,
    metadata TEXT NOT NULL, occurred_seconds INTEGER NOT NULL, occurred_nanos INTEGER NOT NULL
);
INSERT INTO records VALUES('2026-10-20T01:02:03.123456789Z','check',1,'{"duration_s":1.25,"exit_code":0,"name":"lint","outcome":"passed","suite":"quality"}','{"record_id":"271040e6-e274-4a15-9604-fa07003410d2","observed_at":"2026-11-06T23:34:02.43615Z","project":{"key":"/work/repo","label":"repo"},"source":"fixture","run_id":"run-1","correlation_id":"c-1","kind_version":1,"schema_id":"v2:671374808bb4b543da3d29b273633f1cac619b9a1d9c37cb6a1b50ed6f842d78"}',1792458123,123456789);
INSERT INTO records VALUES('2026-11-06T23:34:02.468576Z','check',1,'{"exit_code":1,"name":"test","outcome":"failed"}','{"record_id":"a9555889-fcf3-40d7-9167-c33b471dcbe2","observed_at":"2026-11-06T23:34:02.468599Z","kind_version":1,"schema_id":"v2:671374808bb4b543da3d29b273633f1cac619b9a1d9c37cb6a1b50ed6f842d78"}',1794008042,468576000);
INSERT INTO records VALUES('2026-11-06T23:34:02.489304Z','team.deploy',1,'{"actor":"71b278f3dc434447","duration_s":12.5,"outcome":"ok","service":"api"}','{"record_id":"7e5827e5-3718-4a5b-bd19-b2b98532a4e7","observed_at":"2026-11-06T23:34:02.489325Z","project":{"key":"/work/repo","label":"repo"},"kind_version":1,"schema_id":"v2:d40191e68af322bcb20fa4370a2e58586947c5e2135ed7a59e619cce5d8cd984"}',1794008042,489304000);
INSERT INTO records VALUES('2026-11-06T23:34:02.504505Z','check',1,'{"duration_s":30,"name":"build","outcome":"passed"}','{"record_id":"4f988e9a-ce6f-44bb-bada-483b03ad44e4","observed_at":"2026-11-06T23:34:02.504528Z","project":{"key":"/work/repo","label":"repo"},"run_id":"run-2","kind_version":1,"schema_id":"v2:671374808bb4b543da3d29b273633f1cac619b9a1d9c37cb6a1b50ed6f842d78"}',1794008042,504505000);
INSERT INTO records VALUES('2026-11-06T23:34:02.509099Z','team.deploy',1,'{"outcome":"failed","service":"web"}','{"record_id":"4b889008-4a4b-4161-a68c-fabb8d9da4b3","observed_at":"2026-11-06T23:34:02.509107Z","project":{"key":"/work/repo","label":"repo"},"source":"ci","kind_version":1,"schema_id":"v2:d40191e68af322bcb20fa4370a2e58586947c5e2135ed7a59e619cce5d8cd984"}',1794008042,509099000);
INSERT INTO records VALUES('2026-10-21T00:00:00Z','claude.api_request',1,'{"cost_usd":0.02,"input_tokens":123,"model":"model","request_id":"request","session_id":"session"}','{"projection":true,"record_id":"claude.api_request:8684c9860ad1cb36e047e331eb6958c2928f0880b2577ba90d2c9d89da526f31","observed_at":"2026-11-06T23:34:02.637061Z","project":{"key":"vcs:https://example.com/team/repo","label":"repo"},"source":"claude_code.otlp","kind_version":1,"schema_id":"v2:0a444892c35e581435c6b6e1379f98f79ca0a554e990dd2fbf3b00b372235e64"}',1792540800,0);
CREATE TABLE delivery_queues (
    destination TEXT PRIMARY KEY, pending_records INTEGER NOT NULL DEFAULT 0,
    pending_bytes INTEGER NOT NULL DEFAULT 0
);
INSERT INTO delivery_queues VALUES('http://127.0.0.1:9',5,2480);
CREATE TABLE deliveries (
    record_id TEXT NOT NULL, destination TEXT NOT NULL,
    status TEXT NOT NULL CHECK(status IN ('pending','delivered','failed','expired','cancelled','capacity')),
    created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL,
    next_attempt_at INTEGER NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
    size_bytes INTEGER NOT NULL, last_error TEXT,
    PRIMARY KEY(record_id,destination)
);
INSERT INTO deliveries VALUES('271040e6-e274-4a15-9604-fa07003410d2','http://127.0.0.1:9','pending',1794008042,1794008042,1794008047,1,593,'network failure or timeout; acknowledgement unknown');
INSERT INTO deliveries VALUES('a9555889-fcf3-40d7-9167-c33b471dcbe2','http://127.0.0.1:9','pending',1794008042,1794008042,1794008047,1,336,'network failure or timeout; acknowledgement unknown');
INSERT INTO deliveries VALUES('7e5827e5-3718-4a5b-bd19-b2b98532a4e7','http://127.0.0.1:9','pending',1794008042,1794008042,1794008047,1,531,'network failure or timeout; acknowledgement unknown');
INSERT INTO deliveries VALUES('4f988e9a-ce6f-44bb-bada-483b03ad44e4','http://127.0.0.1:9','pending',1794008042,1794008042,1794008047,1,516,'network failure or timeout; acknowledgement unknown');
INSERT INTO deliveries VALUES('4b889008-4a4b-4161-a68c-fabb8d9da4b3','http://127.0.0.1:9','pending',1794008042,1794008042,1794008047,1,504,'network failure or timeout; acknowledgement unknown');
CREATE TABLE contracts (
    schema_id TEXT PRIMARY KEY, kind TEXT NOT NULL, definition TEXT NOT NULL
);
INSERT INTO contracts VALUES('v2:671374808bb4b543da3d29b273633f1cac619b9a1d9c37cb6a1b50ed6f842d78','check','{"name":"check","mode":"event","fields":{"duration_s":{"type":"number","required":false,"unit":"s"},"exit_code":{"type":"integer","required":false},"name":{"type":"string","required":true},"outcome":{"type":"string","required":true},"signal":{"type":"string","required":false},"suite":{"type":"string","required":false}},"version":1,"group_key":"name","redact":[],"measures":["duration_s"],"identity":null}');
INSERT INTO contracts VALUES('v2:d40191e68af322bcb20fa4370a2e58586947c5e2135ed7a59e619cce5d8cd984','team.deploy','{"name":"team.deploy","mode":"event","fields":{"actor":{"type":"string","required":false},"duration_s":{"type":"number","required":false,"unit":"s"},"outcome":{"type":"string","required":true},"service":{"type":"string","required":true}},"version":1,"group_key":"service","redact":["actor"],"measures":["duration_s"],"identity":null}');
INSERT INTO contracts VALUES('v2:0a444892c35e581435c6b6e1379f98f79ca0a554e990dd2fbf3b00b372235e64','claude.api_request','{"name":"claude.api_request","mode":"event","fields":{"agent":{"type":"string","required":false},"cache_creation_tokens":{"type":"integer","required":false,"unit":"tokens"},"cache_read_tokens":{"type":"integer","required":false,"unit":"tokens"},"cost_usd":{"type":"number","required":false,"unit":"USD"},"duration_ms":{"type":"number","required":false,"unit":"ms"},"input_tokens":{"type":"integer","required":false,"unit":"tokens"},"model":{"type":"string","required":false},"output_tokens":{"type":"integer","required":false,"unit":"tokens"},"query_source":{"type":"string","required":false},"request_id":{"type":"string","required":false},"session_id":{"type":"string","required":true},"skill":{"type":"string","required":false}},"version":1,"group_key":"model","redact":[],"measures":["cost_usd","duration_ms","input_tokens","output_tokens","cache_read_tokens","cache_creation_tokens"],"identity":"request_id"}');
CREATE UNIQUE INDEX idx_records_id ON records(json_extract(metadata,'$.record_id')) WHERE json_extract(metadata,'$.record_id') IS NOT NULL;
CREATE INDEX idx_records_kind_time ON records(kind,occurred_seconds,occurred_nanos);
CREATE INDEX idx_records_time ON records(occurred_seconds,occurred_nanos);
CREATE INDEX idx_deliveries_due ON deliveries(destination,status,next_attempt_at);
CREATE INDEX idx_contracts_kind ON contracts(kind);
COMMIT;
