//! Explicit bounds for audited core and guild-store ordinary writes.
//!
//! These helpers cover audited single-row inserts/updates and deleting or
//! clearing triggers in both schemas. Callers must pass the maximum affected
//! row count; snapshot payloads and schema changes use their own tighter bounds.

use super::*;
use fallible_iterator::FallibleIterator;
use libsql::params::{IntoParams, Params};
use libsql_sqlite3_parser::{
    ast::{Cmd, Stmt},
    lexer::sql::Parser,
};

pub(super) trait BoundedWrites: SqlExecutor {
    /// Count matching rows under the same primary write lock. In particular,
    /// a lifetime total of ban/action rows must not consume a one-row budget.
    fn execute_matching(&self, sql: &str, params: impl IntoParams + Send) -> Result<usize> {
        anyhow::ensure!(
            self.is_write_transaction(),
            "matching write requires a primary transaction"
        );
        let params = params.into_params()?;
        let mut parser = Parser::new(sql.as_bytes());
        let Some(Cmd::Stmt(Stmt::Update {
            with: None,
            tbl_name,
            from: None,
            where_clause,
            ..
        })) = parser.next()?
        else {
            anyhow::bail!("matching budget requires a simple UPDATE");
        };
        anyhow::ensure!(
            parser.next()?.is_none() && tbl_name.db_name.is_none() && tbl_name.alias.is_none(),
            "unsupported matching write"
        );
        let mut predicate = where_clause
            .map(|value| value.to_string())
            .unwrap_or_else(|| "1".into());
        // Preserve bind slots that only occurred in SET. All callers use
        // explicitly numbered parameters, so this does not renumber WHERE.
        match &params {
            Params::Positional(values) if !values.is_empty() => {
                let last = values.len();
                predicate = format!("({predicate}) AND (?{last} IS ?{last})");
            }
            Params::Named(_) => anyhow::bail!("named parameters require an explicit write bound"),
            _ => {}
        }
        let rows: i64 = self.query_row(
            &format!("SELECT COUNT(*) FROM {} WHERE {predicate}", tbl_name.name),
            params.clone(),
            |r| Ok(r.get(0)?),
        )?;
        self.execute_rows(sql, params, u64::try_from(rows)?)
    }

    fn execute_rows(&self, sql: &str, params: impl IntoParams + Send, rows: u64) -> Result<usize> {
        let params = params.into_params()?;
        let growth = core_row_bound(self, sql, &params, rows)?;
        self.execute_bounded(sql, params, growth)
    }

    fn query_row_write<T, P, F>(&self, sql: &str, params: P, map: F) -> Result<T>
    where
        T: Send,
        P: IntoParams + Send,
        F: FnOnce(&crate::db_compat::Row) -> Result<T> + Send,
    {
        let params = params.into_params()?;
        let growth = core_row_bound(self, sql, &params, 1)?;
        self.query_row_bounded(sql, params, growth, map)
    }

    /// Audited deletion triggers only delete or clear existing fields. Reserve
    /// temporary B-tree balancing space; do not treat reclamation as free.
    fn execute_reclaim(&self, sql: &str, params: impl IntoParams + Send) -> Result<usize> {
        let page_size = self.capacity_page_size()?;
        anyhow::ensure!(
            (512..=65536).contains(&page_size),
            "unknown database page size"
        );
        self.execute_reclaim_bounded(sql, params, page_size * 1024)
    }

    fn execute_copy(
        &self,
        sql: &str,
        destination: i64,
        source: i64,
        table: &str,
        key: &str,
        value: &str,
    ) -> Result<usize> {
        anyhow::ensure!(
            self.is_write_transaction(),
            "copy write requires a primary transaction"
        );
        for identifier in [table, key, value] {
            anyhow::ensure!(
                identifier
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "invalid copy budget identifier"
            );
        }
        // The source and destination are in this same primary transaction, so
        // the measured source cannot grow between admission and INSERT SELECT.
        let (rows, bytes): (i64, i64) = self.query_row(
            &format!("SELECT COUNT(*),COALESCE(SUM(length(CAST({key} AS BLOB))+length(CAST({value} AS BLOB))),0) FROM {table} WHERE guild_id=?1"),
            params![source], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let growth = core_row_bound(self, sql, &Params::None, u64::try_from(rows)?)?
            .checked_add(
                u64::try_from(bytes)?
                    .checked_mul(32)
                    .ok_or_else(|| anyhow::anyhow!("copy budget overflow"))?,
            )
            .ok_or_else(|| anyhow::anyhow!("copy budget overflow"))?;
        self.execute_bounded(sql, params![destination, source], growth)
    }
}

impl<T: SqlExecutor> BoundedWrites for T {}

/// Member-directory mutations use the same bounded admission as other guild writes.
pub(crate) trait MemberSqlWrites: BoundedWrites {
    fn member_execute(&self, sql: &str, params: impl IntoParams + Send) -> Result<usize> {
        let head = sql.trim_start();
        let upper = head.to_ascii_uppercase();
        if upper.starts_with("UPDATE") {
            self.execute_matching(sql, params)
        } else if upper.starts_with("DELETE") {
            self.execute_rows(sql, params, 1)
        } else if upper.starts_with("INSERT") && upper.contains("SELECT") {
            self.execute_rows(sql, params, 64)
        } else {
            self.execute_rows(sql, params, 1)
        }
    }
}

impl<T: SqlExecutor> MemberSqlWrites for T {}

fn core_row_bound(
    conn: &(impl SqlExecutor + ?Sized),
    sql: &str,
    params: &Params,
    rows: u64,
) -> Result<u64> {
    let value_size = |value: &Value| -> u64 {
        match value {
            Value::Text(v) => v.len() as u64,
            Value::Blob(v) => v.len() as u64,
            _ => 16,
        }
    };
    let payload: u64 = match params {
        Params::None => 0,
        Params::Positional(values) => values.iter().map(value_size).sum(),
        Params::Named(values) => values.iter().map(|(_, value)| value_size(value)).sum(),
    };
    let page_size = conn.capacity_page_size()?;
    anyhow::ensure!(
        (512..=65536).contains(&page_size),
        "unknown database page size"
    );
    // Include literals, the bounded scalar/identifier fields retained by partial
    // updates, record encoding, up to eight table/index/trigger copies, and
    // worst-case B-tree split paths. Ignore reusable pages for admission.
    let per_row = payload
        .checked_add(sql.len() as u64)
        .and_then(|n| n.checked_add(65_536))
        .and_then(|n| n.checked_mul(32))
        .and_then(|n| n.checked_add(page_size * 512))
        .ok_or_else(|| anyhow::anyhow!("database growth bound overflow"))?;
    per_row
        .checked_mul(rows.max(1))
        .ok_or_else(|| anyhow::anyhow!("database growth bound overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_write_preserves_bind_slots_and_primary_scope() {
        let path =
            std::env::temp_dir().join(format!("write-budget-{:032x}.db", rand::random::<u128>()));
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch_bounded("CREATE TABLE rows(id INTEGER PRIMARY KEY, value TEXT); INSERT INTO rows VALUES(1,'old'),(2,'old'),(3,'old');", 8 * 1024 * 1024).unwrap();
        let sql = "UPDATE rows SET value=?1 WHERE id=?2";
        assert!(conn.execute_matching(sql, params!["outside", 1]).is_err());
        let tx = conn.transaction_immediate().unwrap();
        assert_eq!(tx.execute_matching(sql, params!["changed", 2]).unwrap(), 1);
        assert_eq!(
            tx.execute_matching(
                "UPDATE rows SET value=?1 WHERE id IN (SELECT id FROM rows WHERE id<>?2 LIMIT 1)",
                params!["limited", 2]
            )
            .unwrap(),
            1
        );
        tx.commit().unwrap();
        assert_eq!(
            conn.query_row("SELECT value FROM rows WHERE id=2", (), |r| Ok(
                r.get::<String>(0)?
            ))
            .unwrap(),
            "changed"
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM rows WHERE value='limited'", (), |r| {
                Ok(r.get::<i64>(0)?)
            })
            .unwrap(),
            1
        );
        drop(conn);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }
}
