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
            // Match Bans' UTC millisecond order, including timestamps with offsets.
            Self::Date => {
                "COALESCE(CAST(ROUND((julianday(NULLIF(created_at COLLATE NOCASE, 'now')) - 2440587.5) * 86400000) AS INTEGER), 0)"
            }
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
    let key = request.sort.key();
    let (dir, seek) = if request.descending {
        ("DESC", "<")
    } else {
        ("ASC", ">")
    };
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
    let mut sql = format!("SELECT {EVENT_COLUMNS}, {key} FROM events");
    let mut args = filters_sql(&mut sql, guild, filters)?;
    if let Some(anchor) = request.after {
        anyhow::ensure!(anchor.id > 0, "invalid list cursor");
        let value = match (request.sort, anchor.value) {
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
                    text_key_digest(&event_type) == expected,
                    "invalid list cursor"
                );
                Value::Text(event_type)
            }
            (
                EventListSort::Date | EventListSort::User | EventListSort::Actor,
                EventListValue::Number(number),
            ) => Value::Integer(number),
            _ => anyhow::bail!("invalid list cursor"),
        };
        sql.push_str(&format!(" AND ({key}, id) {seek} (?, ?)"));
        args.extend([value, Value::Integer(anchor.id)]);
    }
    let limit = request.limit.clamp(1, i64::from(DASHBOARD_LIST_PAGE_LIMIT));
    sql.push_str(&format!(" ORDER BY {key} {dir}, id {dir} LIMIT ?"));
    args.push((limit + 1).into());

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

fn filters_sql(sql: &mut String, guild: i64, filters: &EventListFilters) -> Result<Vec<Value>> {
    // Keep sparse/absent-user lookups selective, as the unsorted read does.
    if filters.subject.is_some() {
        sql.push_str(" INDEXED BY idx_events_user");
    }
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
    let date = EventListSort::Date.key();
    if let Some(from) = filters.from_ms {
        sql.push_str(&format!(" AND {date} >= ?"));
        args.push(Value::Integer(from));
    }
    if let Some(until) = filters.until_ms {
        sql.push_str(&format!(" AND {date} < ?"));
        args.push(Value::Integer(until));
    }
    Ok(args)
}
