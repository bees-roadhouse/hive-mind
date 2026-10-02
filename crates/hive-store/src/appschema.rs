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
//! one file (D38 phase 1). The schema name is the install's, derived from the
//! app and the owner, so two installs of one app are two sets of tables
//! (invariant 14). Phase 2 moves them to the owner's file; the naming is what
//! stays.
//!
//! Nothing in here interpolates a string that came from a manifest without
//! having been through `parse_index` or the identifier check below. A manifest
//! is a file an AI writes; DDL built from one is the obvious injection surface,
//! and quoting at this end is the last line rather than the only one.

use hive_db::{Connection, query, quote_ident, quote_literal};
use hive_manifest::{CollectionPlan, Index, IndexMethod, SchemaPlan};

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
pub async fn apply_schema_plan(tx: &Connection, plan: &SchemaPlan) -> Result<()> {
    check_ident(&plan.schema)?;
    for c in &plan.collections {
        apply_collection(tx, &plan.schema, c).await?;
    }
    Ok(())
}

/// Uninstall. Every table under the install's prefix goes, found in the
/// catalogue rather than taken from the plan, so a collection a later manifest
/// dropped is removed too. Per-install prefixes exist so that the blast radius
/// of a bad app is exactly this (D3.2).
pub async fn drop_schema_plan(tx: &Connection, plan: &SchemaPlan) -> Result<()> {
    check_ident(&plan.schema)?;
    let prefix = format!("{}__", plan.schema);
    let tables: Vec<String> = query(
        "SELECT name FROM sqlite_master
          WHERE type = 'table' AND substr(name, 1, length(?1)) = ?1",
    )
    .bind(&prefix)
    .fetch_scalars(tx)
    .await
    .map_err(|e| StoreError::db(format!("list tables of {}", plan.schema), e))?;
    for t in tables {
        query(&format!("DROP TABLE IF EXISTS {}", quote_ident(&t)))
            .execute(tx)
            .await
            .map_err(|e| StoreError::db(format!("drop table {t}"), e))?;
    }
    Ok(())
}

/// Creates one collection's table, its updated_at trigger and its indexes. The
/// table shape is the same for every collection and is not the app's to choose.
async fn apply_collection(tx: &Connection, schema: &str, c: &CollectionPlan) -> Result<()> {
    check_ident(&c.name)?;
    let table = table_name(schema, &c.name);

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
    query(&format!("DROP TRIGGER IF EXISTS {trigger}"))
        .execute(tx)
        .await
        .map_err(|e| StoreError::db(format!("drop touch trigger on {schema}__{}", c.name), e))?;
    query(&format!(
        "CREATE TRIGGER {trigger} AFTER UPDATE OF doc, trust, tainted_by ON {table}
         WHEN NEW.updated_at IS OLD.updated_at
         BEGIN
             UPDATE {table} SET updated_at = {now} WHERE id = NEW.id;
         END",
        now = hive_db::NOW_SQL
    ))
    .execute(tx)
    .await
    .map_err(|e| StoreError::db(format!("create touch trigger on {schema}__{}", c.name), e))?;

    for (i, idx) in c.indexes.iter().enumerate() {
        apply_index(tx, schema, &c.name, &table, i, idx).await?;
    }
    Ok(())
}

async fn apply_index(
    tx: &Connection,
    schema: &str,
    collection: &str,
    table: &str,
    ordinal: usize,
    idx: &Index,
) -> Result<()> {
    let expr = doc_path(idx)?;
    // The index name is derived rather than taken from the manifest, so two
    // apps cannot argue about it and an app cannot name one after something
    // that already exists.
    let name = derived_ident(
        &format!("{schema}__{collection}"),
        &format!("_{}_{ordinal}_idx", idx.method),
    )?;
    let stmt = match idx.method {
        // An expression index over the JSON path: what an equality or a range
        // on that path uses.
        IndexMethod::BTree => format!("CREATE INDEX IF NOT EXISTS {name} ON {table} ({expr})"),
        // There is no inverted index in the engine. A gin index asked for
        // containment on an array; the query path decides containment in the
        // host (see `appdata`), so what is indexed here is the array's text,
        // which serves equality and nothing more. Honest, and named as such.
        IndexMethod::Gin => format!("CREATE INDEX IF NOT EXISTS {name} ON {table} ({expr})"),
        // Full text proper is an FTS5 virtual table kept in step with the
        // document, and that shape (tokenizer, which paths, how a write
        // updates it) is phase-2 work nobody has chosen yet. Until then the
        // path is indexed as text, which serves equality and prefix lookups
        // and is declared here as exactly that rather than as search: nothing
        // in the platform queries full text yet, so there is no query path to
        // quietly fall back to a scan.
        IndexMethod::Fts => format!("CREATE INDEX IF NOT EXISTS {name} ON {table} ({expr})"),
        // Vector wants a typed F32_BLOB column with a dimension, and the
        // manifest has no way to declare one (D38 open items). Refused for the
        // same reason as full text.
        IndexMethod::Vector => {
            return Err(StoreError::NotImplemented(format!(
                "vector indexes need a typed column with a declared dimension ({schema}.{collection}: {idx})"
            )));
        }
    };
    query(&stmt).execute(tx).await.map_err(|e| {
        StoreError::db(
            format!("create {} index on {schema}.{collection}", idx.method),
            e,
        )
    })?;
    Ok(())
}

/// The quoted table name for a collection: `"<schema>__<collection>"`.
pub(crate) fn table_name(schema: &str, collection: &str) -> String {
    quote_ident(&format!("{schema}__{collection}"))
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
        assert_eq!(table_name("app_1", "entries"), "\"app_1__entries\"");
    }

    #[test]
    fn doc_paths_quote_every_segment() {
        let idx = hive_manifest::parse_index("btree(author.name)").unwrap();
        assert_eq!(doc_path(&idx).unwrap(), "doc -> 'author' ->> 'name'");
        let gin = hive_manifest::parse_index("gin(tags)").unwrap();
        assert_eq!(doc_path(&gin).unwrap(), "doc -> 'tags'");
    }
}
