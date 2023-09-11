
CREATE TABLE IF NOT EXISTS conversation (
    id         INTEGER PRIMARY KEY,
    name       VARCHAR(255) NOT NULL UNIQUE,
    max_tokens INTEGER NOT NULL DEFAULT 256,
    model      TEXT NOT NULL DEFAULT 'gpt-3.5-turbo',
    prompt     TEXT
);

CREATE TABLE IF NOT EXISTS history (
   id           INTEGER PRIMARY KEY,
   conversation INTEGER NOT NULL REFERENCES conversation(id),
   message      TEXT NOT NULL
);