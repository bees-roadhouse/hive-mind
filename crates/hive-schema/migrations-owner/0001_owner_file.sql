-- An owner's file (D39): the collection tables an install provisions, and
-- this one table, which says whose file it is.
--
-- The file's NAME is keyed on the owner; this row carries the same dimension
-- inside the file, so a file copied or renamed under another owner's name is
-- refused at the attach rather than served as that owner's documents
-- (invariant 14). hive-store writes the row when it creates the file and
-- checks it every time it attaches.
--
-- Everything else in this file is created by apply_schema_plan from a
-- manifest, never by a migration: the platform owns the shape of a collection
-- table and provisions it per install.
CREATE TABLE owner_file (
    -- One row, by construction.
    lock       INTEGER PRIMARY KEY CHECK (lock = 1),
    kind       TEXT NOT NULL CHECK (kind IN ('user', 'org')),
    id         TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
