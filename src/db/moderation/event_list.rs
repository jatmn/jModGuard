use super::{EVENT_COLUMNS, EventRow, decode_event_row, enrolled_moderation_read, text_key_digest};
use crate::db::{
    DASHBOARD_LIST_PAGE_LIMIT, Db, GuildEnrollment, OptionalExtension, SqlExecutor, Value, params,
    params_from_iter,
};
use anyhow::{Context as _, Result};
use ring::digest::SHA256_OUTPUT_LEN;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EventListSort {
    Date,
    Event,
    User,
    Actor,
}

impl EventListSort {
    fn key(self) -> &'static str {
        match self {
            Self::Date => "list_date",
            Self::Event => "event_type",
            // Missing IDs come before real snowflakes in ascending order.
            Self::User => "COALESCE(user_id, -1)",
            Self::Actor => "COALESCE(actor_id, -1)",
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
enum EventListValue {
    TextDigest([u8; SHA256_OUTPUT_LEN]),
    Number(i64),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventListAnchor {
    value: EventListValue,
    id: i64,
}

/// Readable `data` text the Audit Log search matches, as its Details show it.
const EVENT_SEARCH_TEXT_KEYS: &[&str] = &["reason", "previous_reason", "rule", "command"];

/// Audit Log filters. A person filter holds every user ID it matched, so a
/// name the member directory resolves and a typed ID filter the same way.
#[derive(Default)]
pub struct EventListFilters {
    pub event_type: Option<String>,
    pub subject: Option<Vec<i64>>,
    pub actor: Option<Vec<i64>>,
    pub q: Option<String>,
    /// Users whose directory names match `q`.
    pub q_people: Vec<i64>,
    pub from_ms: Option<i64>,
    pub until_ms: Option<i64>,
}

pub struct EventListRequest {
    pub filters: EventListFilters,
    pub sort: EventListSort,
    pub descending: bool,
    pub after: Option<EventListAnchor>,
    pub limit: i64,
}

pub struct EventListPage {
    pub rows: Vec<EventRow>,
    pub next: Option<EventListAnchor>,
}

/// Seek in the Audit Log within the same primary snapshot as the enrollment check.
pub async fn page_enrolled_events(
    db: &Db,
    enrollment: GuildEnrollment,
    request: EventListRequest,
) -> Result<EventListPage> {
    enrolled_moderation_read(db, enrollment, move |tx, guild| {
        event_page(tx, guild, request)
    })
    .await
}

fn event_page(
    conn: &impl SqlExecutor,
    guild: i64,
    request: EventListRequest,
) -> Result<EventListPage> {
    let filters = &request.filters;
    // A person filter that matched nobody matches no event.
    if [&filters.subject, &filters.actor]
        .into_iter()
        .any(|ids| ids.as_ref().is_some_and(Vec::is_empty))
    {
        return Ok(EventListPage {
            rows: Vec::new(),
            next: None,
        });
    }
    let limit = request.limit.clamp(1, i64::from(DASHBOARD_LIST_PAGE_LIMIT));
    let (sql, args) = event_page_sql(conn, guild, &request, limit)?;
    let mut boundary = None;
    let mut seen = 0;
    let mut rows = conn
        .prepare(&sql)?
        .query_map(params_from_iter(args), |r| {
            let row = decode_event_row(r)?;
            seen += 1;
            if seen == limit {
                let value = if request.sort == EventListSort::Event {
                    EventListValue::TextDigest(text_key_digest(&row.event_type))
                } else {
                    EventListValue::Number(r.get(7)?)
                };
                boundary = Some(EventListAnchor { value, id: row.id });
            }
            Ok(row)
        })?
        .collect::<Result<Vec<_>>>()?;
    let next = if rows.len() > limit as usize {
        boundary
    } else {
        None
    };
    rows.truncate(limit as usize);
    Ok(EventListPage { rows, next })
}

fn event_page_sql(
    conn: &impl SqlExecutor,
    guild: i64,
    request: &EventListRequest,
    limit: i64,
) -> Result<(String, Vec<Value>)> {
    let key = request.sort.key();
    let id_key = if request.sort == EventListSort::Date {
        "list_id"
    } else {
        "id"
    };
    let (dir, seek) = if request.descending {
        ("DESC", "<")
    } else {
        ("ASC", ">")
    };
    let mut sql = format!("SELECT {EVENT_COLUMNS}, {key} FROM events");
    if request.filters.subject.is_some() {
        // Keep sparse and absent user lookups selective.
        sql.push_str(" INDEXED BY idx_events_user");
    } else if request.sort == EventListSort::Date && request.filters.event_type.is_some() {
        // Sort only the matching type rows instead of scanning the guild in date order.
        sql.push_str(" INDEXED BY idx_events_guild_type_id");
    }
    let mut args = filters_sql(&mut sql, guild, request)?;
    if let Some(anchor) = &request.after {
        anyhow::ensure!(anchor.id > 0, "invalid list cursor");
        let value = match (request.sort, &anchor.value) {
            (EventListSort::Event, EventListValue::TextDigest(expected)) => {
                // Keep legacy event types of any length out of the bounded cursor.
                let event_type: String = conn
                    .query_row(
                        "SELECT event_type FROM events WHERE guild_id = ?1 AND id = ?2",
                        params![guild, anchor.id],
                        |r| Ok(r.get(0)?),
                    )
                    .optional()?
                    .context("invalid list cursor")?;
                anyhow::ensure!(
                    text_key_digest(&event_type) == *expected,
                    "invalid list cursor"
                );
                Value::Text(event_type)
            }
            (
                EventListSort::Date | EventListSort::User | EventListSort::Actor,
                EventListValue::Number(number),
            ) => Value::Integer(*number),
            _ => anyhow::bail!("invalid list cursor"),
        };
        sql.push_str(&format!(" AND ({key}, {id_key}) {seek} (?, ?)"));
        args.extend([value, Value::Integer(anchor.id)]);
    }
    sql.push_str(&format!(" ORDER BY {key} {dir}, {id_key} {dir} LIMIT ?"));
    args.push((limit + 1).into());
    Ok((sql, args))
}

fn filters_sql(sql: &mut String, guild: i64, request: &EventListRequest) -> Result<Vec<Value>> {
    let filters = &request.filters;
    sql.push_str(" WHERE guild_id = ?");
    let mut args = vec![Value::Integer(guild)];
    let ids = |ids: &[i64]| serde_json::to_string(ids).map(Value::Text);
    if let Some(event_type) = &filters.event_type {
        sql.push_str(" AND event_type = ?");
        args.push(Value::Text(event_type.clone()));
    }
    if let Some(subject) = &filters.subject {
        sql.push_str(" AND user_id IN (SELECT value FROM json_each(?))");
        args.push(ids(subject)?);
    }
    if let Some(actor) = &filters.actor {
        sql.push_str(" AND actor_id IN (SELECT value FROM json_each(?))");
        args.push(ids(actor)?);
    }
    if let Some(q) = &filters.q {
        // Each search parameter is used more than once, so number them after
        // the ones already bound.
        let (q_arg, people_arg) = (args.len() + 1, args.len() + 2);
        args.extend([Value::Text(q.clone()), ids(&filters.q_people)?]);
        // Match the readable parts only: IDs, the event name, reasons and
        // directory names. Internal payload fields are not searched.
        let text = EVENT_SEARCH_TEXT_KEYS
            .iter()
            .map(|key| {
                format!(
                    " OR instr(lower(CASE WHEN json_valid(data) THEN json_extract(data, '$.{key}') END), lower(?{q_arg})) > 0"
                )
            })
            .collect::<String>();
        sql.push_str(&format!(
            " AND (instr(lower(event_type), lower(?{q_arg})) > 0
                OR instr(CAST(user_id AS TEXT), ?{q_arg}) > 0
                OR instr(CAST(actor_id AS TEXT), ?{q_arg}) > 0{text}
                OR user_id IN (SELECT value FROM json_each(?{people_arg}))
                OR actor_id IN (SELECT value FROM json_each(?{people_arg})))"
        ));
    }
    let (mut from_ms, mut until_ms) = (filters.from_ms, filters.until_ms);
    // SQLite can choose a separate date constraint as the index start and
    // skip the ID part of the seek. Drop only a start bound implied by the cursor.
    if request.sort == EventListSort::Date
        && let Some(EventListAnchor {
            value: EventListValue::Number(anchor_date),
            ..
        }) = &request.after
    {
        if request.descending {
            until_ms = until_ms.filter(|until| until <= anchor_date);
        } else {
            from_ms = from_ms.filter(|from| from > anchor_date);
        }
    }
    let date = EventListSort::Date.key();
    if let Some(from) = from_ms {
        sql.push_str(&format!(" AND {date} >= ?"));
        args.push(Value::Integer(from));
    }
    if let Some(until) = until_ms {
        sql.push_str(&format!(" AND {date} < ?"));
        args.push(Value::Integer(until));
    }
    Ok(args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Connection, MIGRATIONS, migrate_conn};

    fn plan(conn: &Connection, request: &EventListRequest) -> Result<Vec<String>> {
        let (sql, args) = event_page_sql(conn, 42, request, request.limit)?;
        conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?
            .query_map(params_from_iter(args), |row| Ok(row.get(3)?))?
            .collect()
    }

    #[test]
    fn audit_date_migration_preserves_order_and_indexes_first_and_cursor_pages() -> Result<()> {
        let mut conn = Connection::open(":memory:")?;
        for &(version, sql) in MIGRATIONS.iter().take_while(|(version, _)| *version <= 29) {
            let tx = conn.transaction_primary_write()?;
            tx.execute_batch_bounded(sql, 1024 * 1024)?;
            tx.pragma_update(None, "user_version", version)?;
            tx.commit()?;
        }
        conn.execute_batch_bounded(
            "INSERT INTO events (id, guild_id, event_type, user_id, created_at) VALUES
             (1, 42, 'test', 99, '2026-01-01T00:00:00Z'),
             (2, 42, 'test', 99, '2025-12-31T19:00:00-05:00'),
             (3, 42, 'test', 1, 'invalid'),
             (4, 42, 'test', 1, 'NoW' || char(0) || 'legacy'),
             (5, 42, 'test', 1, 'SuBsEc'),
             (6, 42, 'test', 1, '1969-12-31T23:59:59Z'),
             (7, 42, 'test', 1, 'SubSecond'),
             (8, 42, 'test', 99, '2026-01-01T00:00:00.001Z'),
             (9, 43, 'test', 99, '2026-01-01T00:00:00Z')",
            1024 * 1024,
        )?;
        migrate_conn(&mut conn)?;

        let asc = [6, 3, 4, 5, 7, 1, 2, 8];
        for descending in [false, true] {
            let mut expected = asc;
            if descending {
                expected.reverse();
            }
            let mut after = None;
            let pages = expected.as_chunks::<2>().0;
            for (index, ids) in pages.iter().enumerate() {
                let request = EventListRequest {
                    filters: EventListFilters::default(),
                    sort: EventListSort::Date,
                    descending,
                    after,
                    limit: 2,
                };
                let plan = plan(&conn, &request)?;
                let bounds = if descending {
                    "(list_date,list_id)<(?,?)"
                } else {
                    "(list_date,list_id)>(?,?)"
                };
                assert!(
                    plan.iter().any(|step| {
                        step.contains("SEARCH events USING INDEX idx_events_list_date")
                            && (index == 0 || step.contains(bounds))
                    }),
                    "{plan:?}"
                );
                assert!(
                    !plan.iter().any(|step| step.contains("TEMP B-TREE")),
                    "{plan:?}"
                );
                let page = event_page(&conn, 42, request)?;
                assert_eq!(page.rows.iter().map(|row| row.id).collect::<Vec<_>>(), ids);
                after = page.next;
                assert_eq!(after.is_some(), index + 1 < pages.len());
            }
        }

        let request = EventListRequest {
            filters: EventListFilters {
                event_type: Some("test".into()),
                subject: Some(vec![99]),
                from_ms: Some(1_767_225_600_000),
                until_ms: Some(1_767_225_600_001),
                ..EventListFilters::default()
            },
            sort: EventListSort::Date,
            descending: false,
            after: None,
            limit: 10,
        };
        let plan = plan(&conn, &request)?;
        assert!(
            plan.iter()
                .any(|step| step.contains("USING INDEX idx_events_user (user_id=?)")),
            "{plan:?}"
        );
        let page = event_page(&conn, 42, request)?;
        assert_eq!(
            page.rows.iter().map(|row| row.id).collect::<Vec<_>>(),
            [1, 2]
        );
        Ok(())
    }

    #[test]
    fn audit_date_pages_seek_through_ties_and_find_sparse_types() -> Result<()> {
        let mut conn = Connection::open(":memory:")?;
        migrate_conn(&mut conn)?;
        conn.execute_batch_bounded(
            "WITH RECURSIVE ids(id) AS (
                 SELECT 1 UNION ALL SELECT id + 1 FROM ids WHERE id < 10000
             )
             INSERT INTO events (id, guild_id, event_type, created_at)
             SELECT id, 42, CASE WHEN id IN (2, 5000, 9999) THEN 'rare' ELSE 'common' END, 'invalid'
             FROM ids",
            8 * 1024 * 1024,
        )?;

        for (descending, cursor, ids, bound) in [
            (false, 9990, [9991, 9992], "(list_date,list_id)>(?,?)"),
            (true, 10, [9, 8], "(list_date,list_id)<(?,?)"),
        ] {
            let page_request = |from_ms, until_ms| EventListRequest {
                filters: EventListFilters {
                    from_ms: Some(from_ms),
                    until_ms: Some(until_ms),
                    ..EventListFilters::default()
                },
                sort: EventListSort::Date,
                descending,
                after: Some(EventListAnchor {
                    value: EventListValue::Number(0),
                    id: cursor,
                }),
                limit: 2,
            };
            let request = page_request(0, 1);
            let steps = plan(&conn, &request)?;
            assert!(
                steps
                    .iter()
                    .any(|step| step.contains("idx_events_list_date") && step.contains(bound)),
                "{steps:?}"
            );
            let page = event_page(&conn, 42, request)?;
            assert_eq!(page.rows.iter().map(|row| row.id).collect::<Vec<_>>(), ids);

            let outside = if descending {
                page_request(-1, 0)
            } else {
                page_request(1, 2)
            };
            let page = event_page(&conn, 42, outside)?;
            assert!(page.rows.is_empty());
            assert!(page.next.is_none());
        }

        for (event_type, ids) in [("rare", &[9999, 5000, 2][..]), ("absent", &[][..])] {
            let page_request = EventListRequest {
                filters: EventListFilters {
                    event_type: Some(event_type.into()),
                    ..EventListFilters::default()
                },
                sort: EventListSort::Date,
                descending: true,
                after: None,
                limit: 5,
            };
            let steps = plan(&conn, &page_request)?;
            assert!(
                steps
                    .iter()
                    .any(|step| step
                        .contains("idx_events_guild_type_id (guild_id=? AND event_type=?)")),
                "{steps:?}"
            );
            let page = event_page(&conn, 42, page_request)?;
            assert_eq!(page.rows.iter().map(|row| row.id).collect::<Vec<_>>(), ids);
        }
        Ok(())
    }
}
