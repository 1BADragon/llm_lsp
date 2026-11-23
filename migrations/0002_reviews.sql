-- Review results, keyed like docs: by the exact text of the reviewed
-- function. `findings` is a JSON array of review::Finding (often empty).
CREATE TABLE reviews (
    lang            TEXT    NOT NULL,
    body_hash       TEXT    NOT NULL,
    prompt_version  INTEGER NOT NULL,
    qualified_name  TEXT    NOT NULL,
    findings        TEXT    NOT NULL,
    model           TEXT    NOT NULL,
    created_at      INTEGER NOT NULL, -- unix seconds
    PRIMARY KEY (lang, body_hash, prompt_version)
);

-- Findings I rejected. review::Finding::fingerprint survives rewording and
-- reformatting, so the same finding doesn't come back.
CREATE TABLE dismissals (
    qualified_name  TEXT    NOT NULL,
    fingerprint     TEXT    NOT NULL,
    created_at      INTEGER NOT NULL,
    PRIMARY KEY (qualified_name, fingerprint)
);

CREATE INDEX runs_by_kind_and_time ON runs (kind, started_at);
