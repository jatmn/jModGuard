use super::{
    EVENT_COLUMNS, EventRow, decode_event_row, enrolled_moderation_read, event_filters,
    text_key_digest,
};
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

pub struct EventListRequest {
    pub event_type: Option<String>,
    pub user_id: Option<i64>,
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
    let mut sql = format!("SELECT {EVENT_COLUMNS}, {key}");
    let mut args = event_filters(
        &mut sql,
        guild,
        request.event_type.as_deref(),
        request.user_id,
    );
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
