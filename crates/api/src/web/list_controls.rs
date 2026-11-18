//! Common query parameters and keyset cursor encoding for dashboard lists.
use super::{ApiError, ApiResult};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::future::Future;
use std::time::Duration;

const MAX_SEARCH_CHARS: usize = 200;
const MAX_CURSOR_BYTES: usize = 4096;
const CURSOR_VERSION: u8 = 1;

pub(super) async fn within_list_deadline<T>(
    label: &str,
    read: impl Future<Output = ApiResult<T>>,
) -> ApiResult<T> {
    // Cover authorization, pool checkout and all snapshots, including metadata.
    // Dropping the inner future signals database cancellation.
    tokio::time::timeout(Duration::from_secs(20), read)
        .await
        .map_err(|_| {
            ApiError(anyhow::anyhow!(
                "{label} request timed out. Please try again."
            ))
        })?
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Deserialize)]
pub struct ListQuery<S, O> {
    pub after: Option<String>,
    pub subject: Option<String>,
    pub actor: Option<String>,
    #[serde(alias = "status")]
    pub outcome: Option<O>,
    pub q: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub sort: Option<S>,
    pub dir: Option<SortDirection>,
}

pub struct ListControls<S, O, T = i64> {
    pub after: Option<String>,
    pub subject: Option<T>,
    pub actor: Option<i64>,
    pub outcome: Option<O>,
    pub q: Option<String>,
    pub from_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub sort: S,
    pub dir: SortDirection,
}

impl<S, O> ListQuery<S, O> {
    pub fn validate(
        self,
        default_sort: S,
        default_dir: SortDirection,
    ) -> ApiResult<ListControls<S, O>> {
        self.validate_with_subject(default_sort, default_dir, parse_user_id)
    }

    pub fn validate_with_subject<T>(
        self,
        default_sort: S,
        default_dir: SortDirection,
        subject: impl FnOnce(Option<&str>) -> ApiResult<Option<T>>,
    ) -> ApiResult<ListControls<S, O, T>> {
        let from = parse_date(self.from.as_deref())?;
        let to = parse_date(self.to.as_deref())?;
        if matches!((from, to), (Some(from), Some(to)) if from > to) {
            return Err(invalid("from date must be on or before to date"));
        }
        let q = bounded_list_text(self.q.as_deref())?;
        let midnight = |date: NaiveDate| {
            date.and_time(chrono::NaiveTime::MIN)
                .and_utc()
                .timestamp_millis()
        };
        Ok(ListControls {
            after: self.after,
            subject: subject(self.subject.as_deref())?,
            actor: parse_user_id(self.actor.as_deref())?,
            outcome: self.outcome,
            q,
            from_ms: from.map(midnight),
            until_ms: to.map(|date| midnight(date) + 86_400_000),
            sort: self.sort.unwrap_or(default_sort),
            dir: self.dir.unwrap_or(default_dir),
        })
    }
}

pub fn bounded_list_text(raw: Option<&str>) -> ApiResult<Option<String>> {
    let text = raw.map(str::trim).filter(|text| !text.is_empty());
    if text.is_some_and(|text| text.chars().count() > MAX_SEARCH_CHARS || text.contains('\0')) {
        return Err(invalid(
            "list search and text filters must be at most 200 characters without NUL",
        ));
    }
    Ok(text.map(str::to_owned))
}

pub fn parse_user_id(raw: Option<&str>) -> ApiResult<Option<i64>> {
    raw.filter(|raw| !raw.is_empty())
        .map(|raw| {
            raw.parse::<i64>()
                .ok()
                .filter(|id| *id > 0 && raw.bytes().all(|b| b.is_ascii_digit()))
                .ok_or_else(|| invalid("invalid user id filter"))
        })
        .transpose()
}

fn parse_date(raw: Option<&str>) -> ApiResult<Option<NaiveDate>> {
    raw.filter(|raw| !raw.is_empty())
        .map(|raw| {
            NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                .ok()
                .filter(|date| raw.len() == 10 && date.format("%Y-%m-%d").to_string() == raw)
                .ok_or_else(|| invalid("date filters must use YYYY-MM-DD"))
        })
        .transpose()
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor<S, A> {
    version: u8,
    list: String,
    sort: S,
    dir: SortDirection,
    anchor: A,
}

// Callers treat the token as opaque. List-specific anchors contain the ordered
// value and a stable tiebreak so they survive deletion of the boundary row.
pub fn encode_cursor<S: Serialize, A: Serialize>(
    list: &str,
    sort: S,
    dir: SortDirection,
    anchor: A,
) -> ApiResult<String> {
    let json = serde_json::to_vec(&Cursor {
        version: CURSOR_VERSION,
        list: list.to_owned(),
        sort,
        dir,
        anchor,
    })?;
    let encoded = URL_SAFE_NO_PAD.encode(json);
    if encoded.len() > MAX_CURSOR_BYTES {
        return Err(invalid("list cursor is too long"));
    }
    Ok(encoded)
}

pub fn decode_cursor<S: DeserializeOwned + PartialEq, A: DeserializeOwned>(
    raw: &str,
    list: &str,
    sort: S,
    dir: SortDirection,
) -> ApiResult<A> {
    let bad = || invalid("invalid list cursor");
    if raw.len() > MAX_CURSOR_BYTES {
        return Err(bad());
    }
    let json = URL_SAFE_NO_PAD.decode(raw).map_err(|_| bad())?;
    let cursor: Cursor<S, A> = serde_json::from_slice(&json).map_err(|_| bad())?;
    if cursor.version != CURSOR_VERSION
        || cursor.list != list
        || cursor.sort != sort
        || cursor.dir != dir
    {
        return Err(bad());
    }
    Ok(cursor.anchor)
}

fn invalid(message: &'static str) -> ApiError {
    ApiError(anyhow::anyhow!(message))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Query;

    #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(rename_all = "snake_case")]
    enum TestSort {
        Date,
        Reason,
    }

    fn query(raw: &str) -> ApiResult<ListControls<TestSort, String>> {
        let Query(query) = Query::<ListQuery<TestSort, String>>::try_from_uri(
            &format!("/?{raw}").parse().unwrap(),
        )
        .map_err(|error| ApiError(error.into()))?;
        query.validate(TestSort::Date, SortDirection::Desc)
    }

    #[test]
    fn list_controls_validate_shared_parameters() {
        let default = query("").unwrap_or_else(|_| panic!("default query"));
        assert_eq!(
            (default.sort, default.dir),
            (TestSort::Date, SortDirection::Desc)
        );
        let controls = query("subject=9223372036854775807&actor=9&status=active&q=+literal%25+&from=2026-01-02&to=2026-01-02&sort=reason&dir=asc")
            .unwrap_or_else(|_| panic!("combined query"));
        assert_eq!(
            (controls.subject, controls.actor),
            (Some(i64::MAX), Some(9))
        );
        assert_eq!(controls.outcome.as_deref(), Some("active"));
        assert_eq!(controls.q.as_deref(), Some("literal%"));
        assert_eq!(
            (controls.from_ms, controls.until_ms),
            (Some(1_767_312_000_000), Some(1_767_398_400_000))
        );
        assert_eq!(
            (controls.sort, controls.dir),
            (TestSort::Reason, SortDirection::Asc)
        );
        for raw in [
            "subject=0",
            "actor=%2B1",
            "from=2026-2-01",
            "to=2026-02-29",
            "from=2026-10-02&to=2026-10-01",
            "sort=elsewhere",
            "dir=sideways",
            "status=active&outcome=active",
        ] {
            assert!(query(raw).is_err(), "{raw}");
        }
        assert!(query(&format!("q={}", "x".repeat(201))).is_err());
    }

    #[test]
    fn list_controls_cursor_checks_list_sort_direction_and_anchor_type() {
        let token = encode_cursor("example", TestSort::Reason, SortDirection::Asc, ("#%é", 17))
            .unwrap_or_else(|_| panic!("encode cursor"));
        let decode = |raw, list, sort, dir| decode_cursor::<_, (String, i64)>(raw, list, sort, dir);
        let anchor = decode(&token, "example", TestSort::Reason, SortDirection::Asc)
            .unwrap_or_else(|_| panic!("decode cursor"));
        assert_eq!(anchor, ("#%é".into(), 17));
        assert!(decode(&token, "other", TestSort::Reason, SortDirection::Asc).is_err());
        assert!(decode(&token, "example", TestSort::Date, SortDirection::Asc).is_err());
        assert!(decode(&token, "example", TestSort::Reason, SortDirection::Desc).is_err());
        assert!(decode("%FF", "example", TestSort::Reason, SortDirection::Asc).is_err());
        let reason = "🌐".repeat(512);
        let token = encode_cursor(
            "example",
            TestSort::Reason,
            SortDirection::Asc,
            (&reason, 17),
        )
        .unwrap_or_else(|_| panic!("encode a full-length reason"));
        let anchor = decode(&token, "example", TestSort::Reason, SortDirection::Asc)
            .unwrap_or_else(|_| panic!("decode a full-length reason"));
        assert_eq!(anchor, (reason, 17));
        assert!(
            decode_cursor::<_, i64>(&token, "example", TestSort::Reason, SortDirection::Asc)
                .is_err()
        );
    }
}
