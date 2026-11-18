use super::{decode_ban_row, enrolled_moderation_read};
use crate::db::{BanRow, Db, GuildEnrollment, SqlExecutor, Value, params, params_from_iter};
use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BanListSort {
    Subject,
    Id,
    Reason,
    Actor,
    Date,
    Status,
}

impl BanListSort {
    fn order(self) -> (&'static str, &'static str) {
        match self {
            Self::Subject => ("list_subject", "idx_bans_list_subject"),
            Self::Id => ("user_id", "idx_bans_guild"),
            Self::Reason => ("list_reason", "idx_bans_list_reason"),
            Self::Actor => ("list_moderator", "idx_bans_list_moderator"),
            Self::Date => ("list_date", "idx_bans_list_date"),
            Self::Status => ("active", "idx_bans_list_status"),
        }
    }

    fn is_text(self) -> bool {
        matches!(self, Self::Subject | Self::Reason | Self::Actor)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BanListStatus {
    Active,
    Unbanned,
}

pub enum BanListSubject {
    Id(i64),
    Name(String),
}

#[derive(Default)]
pub struct BanListFilters {
    pub subject: Option<BanListSubject>,
    pub actor: Option<i64>,
    pub status: Option<BanListStatus>,
    pub reason: Option<String>,
    pub q: Option<String>,
    pub from_ms: Option<i64>,
    pub until_ms: Option<i64>,
}

#[derive(Deserialize, Serialize)]
#[serde(untagged)]
pub enum BanListValue {
    Text(String),
    Number(i64),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BanListAnchor {
    pub value: BanListValue,
    pub id: i64,
}

impl BanListAnchor {
    pub fn matches_sort(&self, sort: BanListSort) -> bool {
        self.id > 0 && matches!(self.value, BanListValue::Text(_)) == sort.is_text()
    }
}

pub struct BanListRequest {
    pub filters: BanListFilters,
    pub sort: BanListSort,
    pub descending: bool,
    pub after: Option<BanListAnchor>,
    pub limit: i64,
}

#[derive(Serialize)]
pub struct BanListModerator {
    #[serde(serialize_with = "crate::ids::ser_i64_str")]
    pub id: i64,
    pub name: Option<String>,
}

pub struct BanListPage {
    pub rows: Vec<BanRow>,
    pub next: Option<BanListAnchor>,
    pub total: i64,
    pub moderators: Vec<BanListModerator>,
}

pub async fn page_enrolled_bans(
    db: &Db,
    enrollment: GuildEnrollment,
    request: BanListRequest,
) -> Result<BanListPage> {
    enrolled_moderation_read(db, enrollment, move |tx, guild| {
        list_page(tx, guild, request)
    })
    .await
}

fn list_page(conn: &impl SqlExecutor, guild: i64, request: BanListRequest) -> Result<BanListPage> {
    let (predicate, values) = filters_sql(guild, &request.filters);
    let total = conn.query_row(
        &format!("SELECT COUNT(*) FROM bans WHERE {predicate}"),
        params_from_iter(values.iter().cloned()),
        |r| Ok(r.get::<i64>(0)?),
    )?;
    let (sql, params) = page_sql(&request, &predicate, values);
    let mut pairs = conn
        .prepare(&sql)?
        .query_map(params_from_iter(params), |r| {
            let mut ban = decode_ban_row(r)?;
            ban.edited_reason = r.get(10)?;
            ban.reason_edited_by = r.get(11)?;
            ban.reason_edited_at = r.get(12)?;
            let value = if request.sort.is_text() {
                BanListValue::Text(r.get(13)?)
            } else {
                BanListValue::Number(r.get(13)?)
            };
            let id = ban.id;
            Ok((ban, BanListAnchor { value, id }))
        })?
        .collect::<Result<Vec<_>>>()?;

    let limit = request.limit.clamp(1, 200) as usize;
    let has_more = pairs.len() > limit;
    pairs.truncate(limit);
    let (rows, anchors): (Vec<_>, Vec<_>) = pairs.into_iter().unzip();
    let next = if has_more {
        anchors.into_iter().last()
    } else {
        None
    };
    let moderators = conn
        .prepare(
            "SELECT moderator_id, MAX(NULLIF(moderator_name, '')) FROM bans
             WHERE guild_id = ?1 AND moderator_id > 0 GROUP BY moderator_id",
        )?
        .query_map(params![guild], |r| {
            Ok(BanListModerator {
                id: r.get(0)?,
                name: r.get(1)?,
            })
        })?
        .collect::<Result<Vec<_>>>()?;
    Ok(BanListPage {
        rows,
        next,
        total,
        moderators,
    })
}

fn filters_sql(guild: i64, filters: &BanListFilters) -> (String, Vec<Value>) {
    let mut values = vec![Value::Integer(guild)];
    let mut predicate = String::from("guild_id = ?1");
    let mut bind = |value, condition: &str| {
        values.push(value);
        let param = format!("?{}", values.len());
        predicate.push_str(" AND ");
        predicate.push_str(&condition.replace("$", &param));
    };
    match &filters.subject {
        Some(BanListSubject::Id(id)) => bind(Value::Integer(*id), "user_id = $"),
        Some(BanListSubject::Name(name)) => bind(
            Value::Text(name.clone()),
            "(instr(lower(user_name), lower($)) > 0 OR instr(lower(user_display_name), lower($)) > 0)",
        ),
        None => {}
    }
    if let Some(actor) = filters.actor {
        bind(Value::Integer(actor), "moderator_id = $");
    }
    if let Some(status) = filters.status {
        bind(
            Value::Integer(i64::from(matches!(status, BanListStatus::Active))),
            "active = $",
        );
    }
    if let Some(reason) = &filters.reason {
        bind(
            Value::Text(reason.clone()),
            "instr(list_reason, lower($)) > 0",
        );
    }
    if let Some(q) = &filters.q {
        bind(
            Value::Text(q.clone()),
            "(instr(lower(user_name), lower($)) > 0 OR instr(lower(user_display_name), lower($)) > 0
              OR instr(CAST(user_id AS TEXT), $) > 0 OR instr(list_reason, lower($)) > 0
              OR instr(lower(reason), lower($)) > 0 OR instr(list_moderator, lower($)) > 0
              OR instr(CAST(moderator_id AS TEXT), $) > 0)",
        );
    }
    if let Some(from) = filters.from_ms {
        bind(Value::Integer(from), "list_date >= $");
    }
    if let Some(until) = filters.until_ms {
        bind(Value::Integer(until), "list_date < $");
    }
    (predicate, values)
}

fn page_sql(
    request: &BanListRequest,
    predicate: &str,
    mut values: Vec<Value>,
) -> (String, Vec<Value>) {
    let (key, index) = request.sort.order();
    let (dir, seek) = if request.descending {
        ("DESC", "<")
    } else {
        ("ASC", ">")
    };
    let mut sql = format!(
        "SELECT id, guild_id, user_id, moderator_id, reason, created_at, active, user_name,
                user_display_name, moderator_name, list_edited_reason, list_reason_edited_by,
                list_reason_edited_at, {key}
         FROM bans INDEXED BY {index} WHERE {predicate}"
    );
    if let Some(after) = &request.after {
        let value = match &after.value {
            BanListValue::Text(value) => Value::Text(value.clone()),
            BanListValue::Number(value) => Value::Integer(*value),
        };
        let key_arg = values.len() + 1;
        let id_arg = key_arg + 1;
        sql.push_str(&format!(" AND ({key}, id) {seek} (?{key_arg}, ?{id_arg})"));
        values.extend([value, Value::Integer(after.id)]);
    }
    values.push(Value::Integer(request.limit.clamp(1, 200) + 1));
    sql.push_str(&format!(
        " ORDER BY {key} {dir}, id {dir} LIMIT ?{}",
        values.len()
    ));
    (sql, values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ban_list_migration_backfills_the_latest_edit_for_the_same_guild() {
        let path = std::env::temp_dir().join(format!(
            "ban_list_upgrade_{:032x}.db",
            rand::random::<u128>()
        ));
        let conn = crate::db::Connection::open(&path).unwrap();
        let migration_budget = 16 * 1024 * 1024;
        for sql in [
            include_str!("../../../migrations/0001_init.sql"),
            include_str!("../../../migrations/0014_ban_display_identity.sql"),
            r#"INSERT INTO bans (id, guild_id, user_id, reason, created_at)
                 VALUES (7, 10, 123, 'original', '2026-01-01T00:00:00Z');
             INSERT INTO events (guild_id, event_type, actor_id, data, created_at) VALUES
                 (10, 'ban_reason_edit', 80, '{"ban_id":7,"reason":"old"}', '2026-01-02T00:00:00Z'),
                 (10, 'ban_reason_edit', 81, '{"ban_id":7,"reason":"latest"}', '2026-01-03T00:00:00Z'),
                 (11, 'ban_reason_edit', 82, '{"ban_id":7,"reason":"foreign"}', '2026-01-04T00:00:00Z');"#,
            include_str!("../../../migrations/0027_ban_list.sql"),
        ] {
            conn.execute_batch_bounded(sql, migration_budget).unwrap();
        }
        let page = list_page(
            &conn,
            10,
            BanListRequest {
                filters: BanListFilters {
                    reason: Some("latest".into()),
                    ..Default::default()
                },
                sort: BanListSort::Reason,
                descending: false,
                after: None,
                limit: 50,
            },
        )
        .unwrap();
        assert_eq!(page.total, 1);
        let row = &page.rows[0];
        assert_eq!(row.reason.as_deref(), Some("original"));
        assert_eq!(row.edited_reason.as_deref(), Some("latest"));
        assert_eq!(row.reason_edited_by, Some(81));
        assert_eq!(
            row.reason_edited_at.as_deref(),
            Some("2026-01-03T00:00:00Z")
        );
        drop(conn);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn ban_list_orders_use_guild_index_seeks() {
        let path =
            std::env::temp_dir().join(format!("ban_list_plans_{:032x}.db", rand::random::<u128>()));
        let db = Db::open(&path, 2).unwrap();
        db.with_conn_sync(|conn| {
            for sort in [
                BanListSort::Subject,
                BanListSort::Id,
                BanListSort::Reason,
                BanListSort::Actor,
                BanListSort::Date,
                BanListSort::Status,
            ] {
                for descending in [false, true] {
                    let filters = BanListFilters {
                        actor: Some(8),
                        q: Some("% literal".into()),
                        ..Default::default()
                    };
                    let (predicate, values) = filters_sql(10, &filters);
                    let mut request = BanListRequest {
                        filters,
                        sort,
                        descending,
                        after: None,
                        limit: 50,
                    };
                    let (sql, args) = page_sql(&request, &predicate, values.clone());
                    crate::db::tests::assert_index_plan(conn, &sql, params_from_iter(args), true);
                    request.after = Some(BanListAnchor {
                        value: if sort.is_text() {
                            BanListValue::Text("same".into())
                        } else {
                            BanListValue::Number(5)
                        },
                        id: 1,
                    });
                    let (sql, args) = page_sql(&request, &predicate, values);
                    crate::db::tests::assert_index_plan(conn, &sql, params_from_iter(args), true);
                }
            }
            Ok(())
        })
        .unwrap();
        drop(db);
        std::fs::remove_file(path).unwrap();
    }
}
