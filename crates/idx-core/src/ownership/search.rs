use rusqlite::{Connection, params};

use crate::error::IdxError;
use crate::ownership::db::sql_not_aggregate;
use crate::ownership::types::Entity;

pub fn fts_search(conn: &Connection, query: &str, limit: usize) -> Result<Vec<Entity>, IdxError> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }

    let limit_i64 =
        i64::try_from(limit).map_err(|e| IdxError::DatabaseError(format!("invalid limit: {e}")))?;

    // The index lives in the connection's temp schema and is rebuilt per
    // search from entities and aliases: always current, and the ownership
    // database itself is never written (read-only and snapshot databases
    // stay byte-identical). The v1 `entity_fts` table could never be read.
    let _ = rebuild_fts(conn);

    if let Ok(rows) = fts_query(conn, &fts_phrase(q), limit_i64)
        && !rows.is_empty()
    {
        return Ok(rows);
    }

    like_query(conn, q, limit_i64)
}

/// User text as one FTS5 phrase, so quotes, `AND`, `*` or parentheses in a
/// name are matched literally instead of parsed as query syntax.
fn fts_phrase(query: &str) -> String {
    format!("\"{}\"", query.replace('"', "\"\""))
}

/// User text as a literal LIKE substring pattern (with `ESCAPE '\'`).
pub(crate) fn like_pattern(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len() + 2);
    escaped.push('%');
    for ch in query.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped.push('%');
    escaped
}

fn fts_query(conn: &Connection, query: &str, limit: i64) -> Result<Vec<Entity>, IdxError> {
    let mut stmt = conn
        .prepare(
            "SELECT e.id, e.canonical_name, e.entity_type, e.country
             FROM temp.entity_search f
             JOIN entities e ON e.id = f.rowid
             WHERE entity_search MATCH ?1
             ORDER BY rank
             LIMIT ?2",
        )
        .map_err(|e| IdxError::DatabaseError(e.to_string()))?;

    let rows = stmt
        .query_map(params![query, limit], |row| {
            Ok(Entity {
                id: row.get(0)?,
                canonical_name: row.get(1)?,
                entity_type: row.get(2)?,
                country: row.get(3)?,
            })
        })
        .map_err(|e| IdxError::DatabaseError(e.to_string()))?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| IdxError::DatabaseError(e.to_string()))?);
    }
    Ok(out)
}

fn like_query(conn: &Connection, query: &str, limit: i64) -> Result<Vec<Entity>, IdxError> {
    let mut stmt = conn
        .prepare(&format!(
            "SELECT id, canonical_name, entity_type, country
                 FROM entities
                 WHERE canonical_name LIKE ?1 ESCAPE '\\' COLLATE NOCASE AND {}
                 ORDER BY canonical_name ASC
                 LIMIT ?2",
            sql_not_aggregate("canonical_name")
        ))
        .map_err(|e| IdxError::DatabaseError(e.to_string()))?;

    let like = like_pattern(query);
    let rows = stmt
        .query_map(params![like, limit], |row| {
            Ok(Entity {
                id: row.get(0)?,
                canonical_name: row.get(1)?,
                entity_type: row.get(2)?,
                country: row.get(3)?,
            })
        })
        .map_err(|e| IdxError::DatabaseError(e.to_string()))?;

    let mut out = Vec::new();
    for row in rows {
        out.push(row.map_err(|e| IdxError::DatabaseError(e.to_string()))?);
    }
    Ok(out)
}

/// (Re)build the connection-local search index over entity names and
/// aliases. Aggregate pseudo-holders (entities, or aliases attached to a real
/// entity) are left out.
pub fn rebuild_fts(conn: &Connection) -> Result<(), IdxError> {
    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS temp.entity_search USING fts5(
             canonical_name,
             aliases,
             tokenize='trigram'
         );
         DELETE FROM temp.entity_search;
         INSERT INTO temp.entity_search(rowid, canonical_name, aliases)
         SELECT e.id, e.canonical_name, COALESCE(a.aliases, '')
         FROM main.entities e
         LEFT JOIN (
             SELECT entity_id, GROUP_CONCAT(raw_name, ' ') AS aliases
             FROM main.entity_aliases
             WHERE {alias_ok}
             GROUP BY entity_id
         ) a ON a.entity_id = e.id
         WHERE {entity_ok};",
        alias_ok = sql_not_aggregate("raw_name"),
        entity_ok = sql_not_aggregate("e.canonical_name"),
    ))
    .map_err(|e| IdxError::DatabaseError(e.to_string()))
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};

    use super::fts_search;
    use crate::ownership::db::ensure_schema;
    use crate::ownership::entities::resolve_entity;
    use crate::ownership::types::OwnershipSource;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        ensure_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn search_matches_aliases_through_the_index() {
        let conn = setup();
        let id = resolve_entity(&conn, "PT BANK CENTRAL ASIA TBK", OwnershipSource::Ksei).unwrap();
        conn.execute(
            "INSERT INTO entity_aliases (entity_id, raw_name, source, confidence, method)
             VALUES (?1, 'BCA CUSTODY ACCOUNT', 'ksei', 1.0, 'manual')",
            params![id],
        )
        .unwrap();

        let rows = fts_search(&conn, "custody", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
    }

    #[test]
    fn search_sees_new_entities_and_leaves_the_database_unwritten() {
        let conn = setup();
        let id = resolve_entity(&conn, "GOVERNMENT OF NORWAY", OwnershipSource::Ksei).unwrap();
        let changes_before = conn.total_changes();
        let rows = fts_search(&conn, "norway", 10).unwrap();
        assert_eq!(rows.iter().map(|e| e.id).collect::<Vec<_>>(), vec![id]);
        let main_tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM main.sqlite_master WHERE name LIKE 'entity_search%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(main_tables, 0);
        // Only the temp index was written.
        assert!(conn.total_changes() >= changes_before);
    }

    #[test]
    fn search_works_on_a_read_only_database() {
        let path =
            std::env::temp_dir().join(format!("idx-search-readonly-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let conn = Connection::open(&path).unwrap();
            ensure_schema(&conn).unwrap();
            resolve_entity(&conn, "BANK CENTRAL ASIA", OwnershipSource::Ksei).unwrap();
            let _: String = conn
                .query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))
                .unwrap();
        }
        let before = std::fs::read(&path).unwrap();
        {
            let conn =
                Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .unwrap();
            assert_eq!(fts_search(&conn, "central", 10).unwrap().len(), 1);
        }
        assert_eq!(std::fs::read(&path).unwrap(), before);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn search_hides_aggregate_entities_and_aliases() {
        let conn = setup();
        // What an older archive import left behind: one entity per bucket, and
        // an alias of that kind attached to a real entity by a manual mapping.
        resolve_entity(&conn, "KSEI AGGREGATE LOCAL CP", OwnershipSource::Ksei).unwrap();
        let real =
            resolve_entity(&conn, "PT BANK CENTRAL ASIA TBK", OwnershipSource::Ksei).unwrap();
        conn.execute(
            "INSERT INTO entity_aliases (entity_id, raw_name, source, confidence, method)
             VALUES (?1, 'KSEI AGGREGATE FOREIGN MF', 'bing', 1.0, 'manual')",
            params![real],
        )
        .unwrap();

        for query in ["aggregate", "local cp", "foreign mf"] {
            assert!(fts_search(&conn, query, 10).unwrap().is_empty(), "{query}");
        }
        let found = fts_search(&conn, "central", 10).unwrap();
        assert_eq!(found.iter().map(|e| e.id).collect::<Vec<_>>(), vec![real]);
    }

    #[test]
    fn query_syntax_and_like_wildcards_are_literal() {
        let conn = setup();
        resolve_entity(&conn, "O'NEIL AND PARTNERS", OwnershipSource::Ksei).unwrap();
        resolve_entity(&conn, "ANOTHER HOLDER", OwnershipSource::Ksei).unwrap();

        assert_eq!(fts_search(&conn, "NEIL AND", 10).unwrap().len(), 1);
        assert!(fts_search(&conn, "\"(*", 10).unwrap().is_empty());
        assert!(fts_search(&conn, "%", 10).unwrap().is_empty());
        assert!(fts_search(&conn, "_", 10).unwrap().is_empty());
    }
}
