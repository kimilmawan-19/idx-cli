use rusqlite::{Connection, params};

use crate::error::IdxError;
use crate::ownership::types::Entity;

pub fn fts_search(conn: &Connection, query: &str, limit: usize) -> Result<Vec<Entity>, IdxError> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }

    let limit_i64 =
        i64::try_from(limit).map_err(|e| IdxError::DatabaseError(format!("invalid limit: {e}")))?;

    // The index is rebuilt after every write that touches entities or
    // aliases; a count mismatch means it predates one (or a snapshot shipped
    // without it). A read-only database cannot rebuild: LIKE still answers.
    let fts_count: Option<i64> = conn
        .query_row("SELECT COUNT(*) FROM entity_fts", [], |row| row.get(0))
        .ok();
    let entity_count: i64 = conn
        .query_row("SELECT COUNT(*) FROM entities", [], |row| row.get(0))
        .map_err(|e| IdxError::DatabaseError(e.to_string()))?;
    if fts_count.is_some_and(|count| count != entity_count) {
        let _ = rebuild_fts(conn);
    }

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
             FROM entity_fts f
             JOIN entities e ON e.id = f.rowid
             WHERE entity_fts MATCH ?1
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
        .prepare(
            "SELECT id, canonical_name, entity_type, country
             FROM entities
             WHERE canonical_name LIKE ?1 ESCAPE '\\' COLLATE NOCASE
             ORDER BY canonical_name ASC
             LIMIT ?2",
        )
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

pub fn rebuild_fts(conn: &Connection) -> Result<(), IdxError> {
    conn.execute("DELETE FROM entity_fts", [])
        .map_err(|e| IdxError::DatabaseError(e.to_string()))?;

    conn.execute(
        "INSERT INTO entity_fts(rowid, canonical_name, aliases)
         SELECT e.id, e.canonical_name, COALESCE(a.aliases, '')
         FROM entities e
         LEFT JOIN (
             SELECT entity_id, GROUP_CONCAT(raw_name, ' ') AS aliases
             FROM entity_aliases
             GROUP BY entity_id
         ) a ON a.entity_id = e.id",
        [],
    )
    .map_err(|e| IdxError::DatabaseError(e.to_string()))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};

    use super::{fts_search, rebuild_fts};
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
        rebuild_fts(&conn).unwrap();

        let rows = fts_search(&conn, "custody", 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
    }

    #[test]
    fn stale_index_is_rebuilt_before_searching() {
        let conn = setup();
        let id = resolve_entity(&conn, "GOVERNMENT OF NORWAY", OwnershipSource::Ksei).unwrap();
        // No rebuild after the write: the count check catches it.
        let rows = fts_search(&conn, "norway", 10).unwrap();
        assert_eq!(rows.iter().map(|e| e.id).collect::<Vec<_>>(), vec![id]);
        let indexed: i64 = conn
            .query_row("SELECT COUNT(*) FROM entity_fts", [], |row| row.get(0))
            .unwrap();
        assert_eq!(indexed, 1);
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
