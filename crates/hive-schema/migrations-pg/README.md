# The Postgres migration set (D43)

The server store's migrations, in Postgres dialect, applied by
`hive_schema::migrate` to a `Db` opened with `Db::connect`. The SQLite set in
`../migrations/` is the same schema in the D38 §3 column shapes; the two are
one schema in two texts, version for version, and the suite that runs against
both engines is what holds them together (D43 §5).

Editing one means editing the other, and the trigger messages are the SAME
static strings in both, on purpose: a test that asserts which rule refused a
write asserts one string on either engine.

What differs, and why:

- Types: `uuid`, `timestamptz`, `jsonb`, `boolean`, identity columns, and
  regular expressions where SQLite spelled an alphabet with `GLOB`.
- Triggers are plpgsql functions; `acting_kind()` is one function where the
  SQLite text inlines the expression twice.
- `grants_identity_uq` uses `NULLS NOT DISTINCT` (Postgres 15+) where SQLite
  coalesces to `''`.
- The override audit's table is in `0001` here rather than in a set of its
  own: the second file (`../migrations-audit/`) exists because SQLite has one
  writer per file and the audit row must survive the caller's rollback; on
  Postgres a second connection on the same database does that, so
  `migrate_audit` checks the table is here instead of applying anything.
- There is no owner file (`../migrations-owner/`): the org's database is the
  tenant (D43 §5), and D43's phase 6 removes that layer from SQLite too.
- Collection tables are a schema per install (D43 §2) where SQLite used a
  table-name prefix in the owner's file; `hive-store` provisions them from
  manifests, never a migration.
