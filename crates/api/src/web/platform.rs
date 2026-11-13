//! Owner-only platform analytics: fleet-wide series for one range (7, 30 or
//! 90 days) plus member directory totals. Directory totals probe each active
//! guild's store within a bounded request budget; unavailable stores are
//! reported as unknown instead of failing the whole read, and directories
//! whose coverage is not established make the totals partial.
use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tower_sessions::Session;

use super::state::ApiState;
use super::usage::authorize;
use crate::db;

type Reply = Result<Json<Value>, (StatusCode, Json<Value>)>;

/// One guild store probe may wait this long before it counts as unavailable.
const STORE_PROBE: Duration = Duration::from_secs(4);
/// The whole member-total enumeration stops after this budget.
const MEMBER_BUDGET: Duration = Duration::from_secs(20);

fn error(status: StatusCode, message: &'static str) -> (StatusCode, Json<Value>) {
    (status, Json(json!({"error": message})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformQuery {
    days: Option<i64>,
}

pub async fn analytics(
    State(app): State<Arc<ApiState>>,
    session: Session,
    headers: HeaderMap,
    Query(query): Query<PlatformQuery>,
) -> Reply {
    authorize(&app, &session, &headers).await?;
    let days = query.days.unwrap_or(db::ANALYTICS_DEFAULT_WINDOW_DAYS);
    if !db::ANALYTICS_RANGES.contains(&days) {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "analytics range must be 7, 30 or 90 days",
        ));
    }
    let core = db::platform_analytics(&app.db, days).await.map_err(|_| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "platform analytics unavailable",
        )
    })?;
    let mut value = json!(core);
    value["members"] = json!(member_totals(&app).await);
    Ok(Json(value))
}

/// Sums the member directory across active guilds. `by_guild` maps each
/// counted guild's decimal-string ID to its current count and directory
/// coverage; an active guild missing from it was unavailable.
pub(super) async fn member_totals(app: &Arc<ApiState>) -> Value {
    let began = Instant::now();
    let enrollments = match db::list_active_guild_enrollments(&app.db).await {
        Ok(rows) => rows,
        Err(_) => return json!({"status": "unavailable"}),
    };
    let servers = enrollments.len() as i64;
    let mut current = 0i64;
    let mut banned = 0i64;
    let mut known = 0i64;
    let mut counted = 0i64;
    let mut unavailable = 0i64;
    let mut incomplete = 0i64;
    let mut by_guild = Map::new();
    for enrollment in enrollments {
        let guild_id = enrollment.guild_id.to_string();
        // Clamp each probe to the remaining budget so the last probe cannot
        // run past it.
        let remaining = MEMBER_BUDGET.saturating_sub(began.elapsed());
        if remaining.is_zero() {
            unavailable += 1;
            continue;
        }
        let probe = tokio::time::timeout(STORE_PROBE.min(remaining), async {
            let (store, scope) = app.tenant.live(enrollment.clone()).await?;
            store.directory_totals(scope).await
        })
        .await;
        match probe {
            Ok(Ok(totals)) => {
                current += totals.current;
                banned += totals.banned;
                known += totals.known;
                counted += 1;
                if !totals.coverage.is_complete() {
                    incomplete += 1;
                }
                by_guild.insert(
                    guild_id,
                    json!({"current": totals.current, "coverage": totals.coverage}),
                );
            }
            _ => unavailable += 1,
        }
    }
    if counted == 0 && unavailable > 0 {
        return json!({
            "status": "unavailable",
            "servers": servers,
            "guilds_unavailable": unavailable,
        });
    }
    json!({
        "status": if unavailable == 0 && incomplete == 0 { "complete" } else { "partial" },
        "servers": servers,
        "current": current,
        "banned": banned,
        "known": known,
        "guilds_counted": counted,
        "guilds_unavailable": unavailable,
        "guilds_incomplete": incomplete,
        "by_guild": by_guild,
    })
}
