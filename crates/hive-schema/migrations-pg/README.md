# The Postgres migration set (D43)

The server store's migrations, in Postgres dialect, applied by
`hive_schema::migrate` to a `Db` opened with `Db::connect`. The SQLite set in
`../migrations/` is the same schema in the D38 §3 column shapes; the two are
one schema in two texts, and the suite that runs against both engines is what
holds them together (D43 §5).

Empty on purpose until phase 1's second slice ports migration one. While it
is empty, `migrate` on a Postgres `Db` refuses with `NoMigrations` rather than
creating `schema_migrations` and reporting nothing to do: a fixture that
"migrated" an empty database would pass every test that never reached a
table, which is the shape the gate rules warn about.

The override audit's table lives in this set rather than in a set of its own:
the second file (`../migrations-audit/`) exists because SQLite has one writer
per file and the audit row must survive the caller's rollback; on Postgres a
second connection on the same database does that, so `migrate_audit` checks
the table is here instead of applying anything.
