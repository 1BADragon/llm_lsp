-- Durable facts the reviewer learned about the project, fed into later
-- review and doc prompts. `path` and `symbol` are '' when not applicable
-- (not NULL, so the UNIQUE constraint de-duplicates them).
CREATE TABLE notes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT, -- never reused: ids appear in prompts and the notes view
    scope       TEXT    NOT NULL, -- 'project' | 'file' | 'symbol'
    path        TEXT    NOT NULL DEFAULT '', -- relative to the project root
    symbol      TEXT    NOT NULL DEFAULT '', -- qualified name
    text        TEXT    NOT NULL,
    norm        TEXT    NOT NULL, -- normalized text, for de-duplication
    source      TEXT    NOT NULL, -- the symbol whose review produced it
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    UNIQUE (scope, path, symbol, norm)
);
