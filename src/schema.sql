CREATE TABLE IF NOT EXISTS personalities (
    id     INTEGER PRIMARY KEY,
    name   VARCHAR(255) NOT NULL UNIQUE,
    prompt TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS history (
   id             INTEGER PRIMARY KEY,
   personality    INTEGER NOT NULL REFERENCES personality(id),
   role           INTEGER NOT NULL,
   content        TEXT NOT NULL
);