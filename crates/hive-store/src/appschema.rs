//! Provisioning an app's storage from a manifest.
//!
//! The split is deliberate. The manifest crate derives a `SchemaPlan`, which is
//! data: printable, diffable, and testable without a database. This module is
//! the only thing that turns that data into statements, because the store is the
//! one crate in the platform that talks to the database.
//!
//! That is not tidiness. The grant predicate lives here, and a second crate
//! holding a file handle would be a second crate reaching the database without
//! any reason to know about grants ... which is precisely how the first hole
//! gets made (invariant 1, and D21's shape).
//!
//! An install's collections are tables named `<schema>__<collection>` in the
//! OWNER's file (D39), attached on the caller's connection and addressed
//! through the alias `attach_owner` returns. The schema name is the
//! install's, derived from the app and the owner, so two installs of one app
//! are two sets of tables (invariant 14); the file is the owner's, so two
//! owners of one app are two files.
//!
//! Nothing in here interpolates a string that came from a manifest without
//! having been through `parse_index` or the identifier check below. A manifest
//! is a file an AI writes; DDL built from one is the obvious injection surface,
//! and quoting at this end is the last line rather than the only one.

use hive_db::{Connection, query, quote_ident, quote_literal};
use hive_identity::Owner;
use hive_manifest::{CollectionPlan, Index, IndexMethod, SchemaPlan};

use crate::owners::{attach_owner, owner_table};
use crate::{Result, StoreError};

/// The bound a derived name must fit in. The engine has no limit of its own;
/// the manifest bounds its identifiers at this and a name derived from one is
/// held to the same number, so a change in the bound is one edit.
const MAX_IDENTIFIER: usize = 63;

/// Provisions an app's collection tables and their indexes. It is idempotent:
/// re-applying the same plan is how a manifest diff becomes a migration
/// (D3.3), so every statement is IF NOT EXISTS.
///
/// It runs inside the caller's transaction, so a failed install leaves nothing
/// behind, and it never commits ... registering an install and provisioning its
/// storage are one unit of work or they are tables nobody owns.
pub async fn apply_schema_plan(tx: &Connection, owner: Owner, plan: &SchemaPlan) -> Result<()> {
    check_ident(&plan.schema)?;
    for c in &plan.collections {
        check_ident(&c.name)?;
    }
    // After every name is checked and before any DDL: a refused plan attaches
    // nothing and creates no file.
    let alias = attach_owner(tx, owner).await?;
    for c in &plan.collections {
        apply_collection(tx, &alias, &plan.schema, c).await?;
    }
    Ok(())
}

/// Uninstall. Every table under the install's prefix goes, found in the
/// catalogue rather than taken from the plan, so a collection a later manifest
/// dropped is removed too. Per-install prefixes exist so that the blast radius
/// of a bad app is exactly this (D3.2).
pub async fn drop_schema_plan(tx: &Connection, owner: Owner, plan: &SchemaPlan) -> Result<()> {
    check_ident(&plan.schema)?;
    let alias = attach_owner(tx, owner).await?;
    let prefix = format!("{}__", plan.schema);
    // Virtual tables first: dropping one takes its shadow tables with it,
    // and a shadow table cannot be dropped on its own (D41 measurement 6).
    // The listing is taken once, so a shadow already gone is IF EXISTS.
    let rows = query(&format!(
        "SELECT name, sql FROM {}.sqlite_master
          WHERE type = 'table' AND substr(name, 1, length(?1)) = ?1",
        quote_ident(&alias)
    ))
    .bind(&prefix)
    .fetch_all(tx)
    .await
    .map_err(|e| StoreError::db(format!("list tables of {}", plan.schema), e))?;
    let mut tables: Vec<(String, bool)> = rows
        .iter()
        .map(|r| {
            let sql: Option<String> = r.get("sql");
            (r.get("name"), sql.as_deref().is_some_and(is_virtual))
        })
        .collect();
    tables.sort_by_key(|(_, virt)| !virt);
    for (t, _) in tables {
        query(&format!("DROP TABLE IF EXISTS {}", owner_table(&alias, &t)))
            .execute(tx)
            .await
            .map_err(|e| StoreError::db(format!("drop table {t}"), e))?;
    }
    Ok(())
}

/// Creates one collection's table, its updated_at trigger and its indexes. The
/// table shape is the same for every collection and is not the app's to choose.
async fn apply_collection(
    tx: &Connection,
    alias: &str,
    schema: &str,
    c: &CollectionPlan,
) -> Result<()> {
    check_ident(&c.name)?;
    let table = table_name(alias, schema, &c.name);
    // Inside a trigger body a qualified name is refused by the engine (D39
    // measurement 3); the trigger lives in the owner's file, so the bare
    // name resolves there.
    let bare = quote_ident(&format!("{schema}__{}", c.name));

    // There is deliberately no owner pair and no author here.
    //
    // A document's ownership lives on its `entities` row, which is also what a
    // grant is written against and what the predicate resolves through. A copy
    // on this table would be a second place to read ownership from, and the
    // only thing stopping a query filtering on the cheaper copy would be a
    // comment ... which is attention rather than intent.
    //
    // The id IS the entity's id, and there is deliberately no foreign key
    // saying so either: an app's tables have to be provisionable without the
    // platform's in reach (phase 2 puts them in another file). What keeps the
    // two rows together is that one transaction writes both and one removes
    // both.
    //
    // trust IS duplicated from `entities`, and that is not the same case: it
    // travels with the row (invariant 3), it is read by the layer serving the
    // document, and nothing authorizes on it.
    query(&format!(
        "CREATE TABLE IF NOT EXISTS {table} (
            id          TEXT PRIMARY KEY,
            doc         TEXT NOT NULL DEFAULT '{{}}' CHECK (json_valid(doc)),
            trust       TEXT NOT NULL DEFAULT 'trusted' CHECK (trust IN ('trusted', 'untrusted')),
            tainted_by  TEXT,
            created_at  INTEGER NOT NULL DEFAULT {now},
            updated_at  INTEGER NOT NULL DEFAULT {now}
        )",
        now = hive_db::NOW_SQL
    ))
    .execute(tx)
    .await
    .map_err(|e| StoreError::db(format!("create table {schema}__{}", c.name), e))?;

    // updated_at IS maintained by a trigger, and this is the one place the
    // project's usual "no triggers" instinct does not apply. That instinct
    // comes from D21: a trigger cannot enforce what the writer supplies, because
    // a trigger has no credential in scope. Entirely correct, and it says
    // nothing about this column, because the clock is not a fact the writer
    // supplies ... it is a clock read, identical whoever is asking. SQLite
    // cannot rewrite NEW in a BEFORE trigger, so this fires AFTER and only when
    // the writer left the column alone.
    let trigger = derived_ident(&format!("{schema}__{}", c.name), "_touch")?;
    let trigger = format!("{}.{trigger}", quote_ident(alias));
    query(&format!("DROP TRIGGER IF EXISTS {trigger}"))
        .execute(tx)
        .await
        .map_err(|e| StoreError::db(format!("drop touch trigger on {schema}__{}", c.name), e))?;
    query(&format!(
        "CREATE TRIGGER {trigger} AFTER UPDATE OF doc, trust, tainted_by ON {bare}
         WHEN NEW.updated_at IS OLD.updated_at
         BEGIN
             UPDATE {bare} SET updated_at = {now} WHERE id = NEW.id;
         END",
        now = hive_db::NOW_SQL
    ))
    .execute(tx)
    .await
    .map_err(|e| StoreError::db(format!("create touch trigger on {schema}__{}", c.name), e))?;

    for (i, idx) in c.indexes.iter().enumerate() {
        apply_index(tx, alias, schema, &c.name, &bare, i, idx).await?;
    }
    Ok(())
}

async fn apply_index(
    tx: &Connection,
    alias: &str,
    schema: &str,
    collection: &str,
    table: &str,
    ordinal: usize,
    idx: &Index,
) -> Result<()> {
    let expr = doc_path(idx)?;
    let base = format!("{schema}__{collection}");
    let fail = |what: &str, e: hive_db::Error| {
        StoreError::db(
            format!("{what} {} index on {schema}.{collection}", idx.method),
            e,
        )
    };
    match idx.method {
        // An expression index over the JSON path: what an equality or a range
        // on that path uses.
        IndexMethod::BTree | IndexMethod::Gin => {
            // The index name is derived rather than taken from the manifest,
            // so two apps cannot argue about it and an app cannot name one
            // after something that already exists. It is created in the
            // owner's file, where the table is; `table` is the bare name,
            // which the engine resolves there. There is no inverted index
            // in the engine: a gin index asked for containment on an array,
            // the query path decides containment in the host (see
            // `appdata`), and what is indexed here is the array's text,
            // which serves equality and nothing more.
            let name = derived_ident(&base, &format!("_{}_{ordinal}_idx", idx.method))?;
            let name = format!("{}.{name}", quote_ident(alias));
            query(&format!(
                "CREATE INDEX IF NOT EXISTS {name} ON {table} ({expr})"
            ))
            .execute(tx)
            .await
            .map_err(|e| fail("create", e))?;
        }
        // Full text: an FTS5 table beside the collection, kept in step by
        // triggers that mirror the text at the path (D41 §1). A document with
        // nothing at the path is not in it.
        IndexMethod::Fts => {
            let vt = virtual_table_name(&base, ordinal, "fts")?;
            let qvt = owner_table(alias, &vt);
            let bare = quote_ident(&vt);
            query(&format!(
                "CREATE VIRTUAL TABLE IF NOT EXISTS {qvt} USING fts5(id UNINDEXED, body)"
            ))
            .execute(tx)
            .await
            .map_err(|e| fail("create", e))?;
            // Inside a trigger the row is NEW, not the table.
            let expr = in_trigger(&expr);
            let value = format!("CAST(({expr}) AS TEXT)");
            mirror_triggers(
                tx,
                alias,
                &vt,
                table,
                &format!("INSERT INTO {bare} (id, body) VALUES (NEW.id, {value});"),
                &format!("DELETE FROM {bare} WHERE id = OLD.id;"),
                &format!("({expr}) IS NOT NULL"),
            )
            .await
            .map_err(|e| fail("mirror", e))?;
        }
        // Semantic recall: a vec0 table beside the collection, keyed by the
        // document id, with the manifest's dimension. The engine refuses a
        // vector of any other size at the insert.
        IndexMethod::Vector => {
            let vt = virtual_table_name(&base, ordinal, "vec")?;
            let qvt = owner_table(alias, &vt);
            let bare = quote_ident(&vt);
            query(&format!(
                "CREATE VIRTUAL TABLE IF NOT EXISTS {qvt} USING vec0(id TEXT PRIMARY KEY, embedding float[{}])",
                idx.dim
            ))
            .execute(tx)
            .await
            .map_err(|e| fail("create", e))?;
            // `->` rather than `->>` for the array: the mirror wants the JSON
            // text of the array, which vec_f32 parses.
            let json = in_trigger(&doc_json_path(idx)?);
            mirror_triggers(
                tx,
                alias,
                &vt,
                table,
                &format!("INSERT INTO {bare} (id, embedding) VALUES (NEW.id, vec_f32({json}));"),
                &format!("DELETE FROM {bare} WHERE id = OLD.id;"),
                &format!("({json}) IS NOT NULL"),
            )
            .await
            .map_err(|e| fail("mirror", e))?;
        }
    }
    Ok(())
}

/// The three triggers that keep a virtual table in step with its collection
/// table: insert, update (as delete then insert) and delete. Recreated on
/// every apply, so a changed path is a changed mirror.
async fn mirror_triggers(
    tx: &Connection,
    alias: &str,
    vt: &str,
    table: &str,
    insert: &str,
    delete: &str,
    present: &str,
) -> hive_db::Result<()> {
    let q = quote_ident(alias);
    for suffix in ["_ai", "_au", "_ad"] {
        let name = quote_ident(&format!("{vt}{suffix}"));
        query(&format!("DROP TRIGGER IF EXISTS {q}.{name}"))
            .execute(tx)
            .await?;
        // The insert mirrors only when the path has a value: the WHEN clause
        // carries that for an insert, and an update guards its own insert
        // inside the body after removing the old row, so a value that went
        // away leaves no mirror row behind.
        let stmt = match suffix {
            "_ai" => format!(
                "CREATE TRIGGER {q}.{name} AFTER INSERT ON {table} WHEN {present} BEGIN {insert} END"
            ),
            "_au" => format!(
                "CREATE TRIGGER {q}.{name} AFTER UPDATE OF doc ON {table} BEGIN {delete} {} END",
                guarded_insert(insert, present)
            ),
            _ => format!("CREATE TRIGGER {q}.{name} AFTER DELETE ON {table} BEGIN {delete} END"),
        };
        query(&stmt).execute(tx).await?;
    }
    Ok(())
}

/// `INSERT ... VALUES (...)` becomes `INSERT ... SELECT ... WHERE present`,
/// so an update that removes the value removes the mirror row and adds none.
fn guarded_insert(insert: &str, present: &str) -> String {
    let (head, values) = insert
        .split_once(" VALUES ")
        .expect("mirror insert has a VALUES clause");
    let values = values.trim_end_matches(';').trim();
    let values = values
        .strip_prefix('(')
        .and_then(|v| v.strip_suffix(')'))
        .expect("mirror values are parenthesised");
    format!("{head} SELECT {values} WHERE {present};")
}

/// The name of an index's virtual table: `<schema>__<collection>_<n>_<kind>`.
fn virtual_table_name(base: &str, ordinal: usize, kind: &str) -> Result<String> {
    let name = format!("{base}_{ordinal}_{kind}");
    if name.len() > MAX_IDENTIFIER * 2 + 2 {
        return Err(StoreError::UnsafeIdentifier(format!(
            "{name:?} is {} characters, past the bound derived names are held to",
            name.len()
        )));
    }
    Ok(name)
}

/// Whether a table under an install's prefix is one of the virtual tables
/// an index provisions, by its sql: those are dropped first, by name, and
/// take their shadow tables with them (D41 measurement 6).
pub(crate) fn is_virtual(sql: &str) -> bool {
    sql.trim_start()
        .get(..21)
        .is_some_and(|s| s.eq_ignore_ascii_case("CREATE VIRTUAL TABLE "))
}

/// The qualified, quoted table name for a collection in the owner's file:
/// `"<alias>"."<schema>__<collection>"`.
pub(crate) fn table_name(alias: &str, schema: &str, collection: &str) -> String {
    owner_table(alias, &format!("{schema}__{collection}"))
}

/// Builds the JSON accessor for an index path. The `->` and `->>` operators
/// are the engine's own (SQLite 3.38 onward), with the same meaning they had
/// on Postgres: `->` keeps JSON along the way, `->>` yields the SQL value at
/// the last hop.
///
/// Each segment is a literal inside the expression, so each one is quoted as a
/// string literal rather than concatenated raw. `parse_index` has already
/// restricted segments to `[a-z][a-z0-9_]*`, so there is nothing to escape ...
/// which is exactly why the check below is cheap enough to keep.
fn doc_path(idx: &Index) -> Result<String> {
    if idx.path.is_empty() {
        return Err(StoreError::UnsafeIdentifier("index with no path".into()));
    }
    for seg in &idx.path {
        check_ident(seg)?;
    }
    // ->> yields the value at the last hop, -> yields JSON along the way.
    // btree and fts want the value; gin over a tag array wants the JSON.
    let mut expr = String::from("doc");
    let last = idx.path.len() - 1;
    for (i, seg) in idx.path.iter().enumerate() {
        let op = if idx.method != IndexMethod::Gin && i == last {
            " ->> "
        } else {
            " -> "
        };
        expr.push_str(op);
        expr.push_str(&quote_literal(seg));
    }
    Ok(expr)
}

/// A `doc`-rooted accessor as a trigger body sees it: the row being
/// written is `NEW`, and a bare column name there is the table's.
fn in_trigger(expr: &str) -> String {
    expr.replacen("doc", "NEW.doc", 1)
}

/// The accessor that yields the JSON at the path rather than the SQL value
/// at its last hop: what `vec_f32` wants for an array.
fn doc_json_path(idx: &Index) -> Result<String> {
    if idx.path.is_empty() {
        return Err(StoreError::UnsafeIdentifier("index with no path".into()));
    }
    let mut expr = String::from("doc");
    for seg in &idx.path {
        check_ident(seg)?;
        expr.push_str(" -> ");
        expr.push_str(&quote_literal(seg));
    }
    Ok(expr)
}

/// The same shape the manifest's validation enforces, duplicated on purpose:
/// this is the check at the POINT OF USE, and a check that trusts an earlier one
/// is a check that stops running the day somebody adds a second caller that
/// skipped it.
pub(crate) fn check_ident(s: &str) -> Result<()> {
    let ok = !s.is_empty()
        && s.len() <= MAX_IDENTIFIER
        && s.chars()
            .enumerate()
            .all(|(i, c)| c.is_ascii_lowercase() || (i > 0 && (c.is_ascii_digit() || c == '_')));
    if !ok {
        return Err(StoreError::UnsafeIdentifier(format!("{s:?}")));
    }
    Ok(())
}

/// Builds an identifier the platform derives from a manifest name, and REFUSES
/// one that would not fit the bound rather than letting it grow without one.
///
/// The bound is kept from the Postgres port on purpose. There, truncation was
/// the dangerous half: an over-long index name collapsed onto the collection
/// name and IF NOT EXISTS turned the collision into a NOTICE nobody saw. The
/// engine here does not truncate, and the bound stays because a name nobody
/// can read in a catalogue listing is its own hazard and the manifest already
/// holds its names to it.
fn derived_ident(base: &str, suffix: &str) -> Result<String> {
    let name = format!("{base}{suffix}");
    if name.len() > MAX_IDENTIFIER * 2 + 2 {
        return Err(StoreError::UnsafeIdentifier(format!(
            "{name:?} is {} characters, past the bound derived names are held to",
            name.len()
        )));
    }
    Ok(quote_ident(&name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idents_are_bounded_and_lowercase() {
        assert!(check_ident("entries").is_ok());
        assert!(check_ident("e_2").is_ok());
        assert!(check_ident("").is_err());
        assert!(check_ident("Entries").is_err());
        assert!(check_ident("_x").is_err());
        assert!(check_ident("x\"; drop").is_err());
        assert!(check_ident(&"a".repeat(64)).is_err());
    }

    #[test]
    fn derived_names_refuse_to_grow_past_the_bound() {
        // A schema and a collection at the bound each, joined, plus a suffix.
        let base = format!("{}__{}", "s".repeat(63), "c".repeat(63));
        assert!(derived_ident(&base, "_touch").is_err());
        assert_eq!(derived_ident("c", "_touch").unwrap(), "\"c_touch\"");
        assert_eq!(
            table_name("o_user_ab", "app_1", "entries"),
            "\"o_user_ab\".\"app_1__entries\""
        );
    }

    #[test]
    fn doc_paths_quote_every_segment() {
        let idx = hive_manifest::parse_index("btree(author.name)").unwrap();
        assert_eq!(doc_path(&idx).unwrap(), "doc -> 'author' ->> 'name'");
        let gin = hive_manifest::parse_index("gin(tags)").unwrap();
        assert_eq!(doc_path(&gin).unwrap(), "doc -> 'tags'");
    }
}
