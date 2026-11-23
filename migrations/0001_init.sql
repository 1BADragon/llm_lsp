-- Generated docs, keyed by the exact text of the symbol they describe.
-- Changing the prompt bumps prompt_version, so old docs are regenerated
-- rather than served.
CREATE TABLE docs (
    lang            TEXT    NOT NULL,
    body_hash       TEXT    NOT NULL,
    prompt_version  INTEGER NOT NULL,
    qualified_name  TEXT    NOT NULL,
    doc             TEXT    NOT NULL,
    model           TEXT    NOT NULL,
    created_at      INTEGER NOT NULL, -- unix seconds
    PRIMARY KEY (lang, body_hash, prompt_version)
);

-- One row per `claude` invocation, for cost and latency accounting.
CREATE TABLE runs (
    id           INTEGER PRIMARY KEY,
    kind         TEXT    NOT NULL, -- 'doc', later 'review'
    subject      TEXT    NOT NULL, -- e.g. the qualified name
    model        TEXT    NOT NULL,
    started_at   INTEGER NOT NULL, -- unix seconds
    duration_ms  INTEGER NOT NULL,
    cost_usd     REAL,
    ok           INTEGER NOT NULL, -- 0/1
    error        TEXT
);
