CREATE TABLE IF NOT EXISTS conversation (
    id     INTEGER PRIMARY KEY,
    name   VARCHAR(255) NOT NULL UNIQUE
);

CREATE TABLE IF NOT EXISTS history (
   id             INTEGER PRIMARY KEY,
   conversation   INTEGER NOT NULL REFERENCES conversation(id),
   role           INTEGER NOT NULL,
   content        TEXT NOT NULL
);