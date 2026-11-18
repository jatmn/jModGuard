//! Indexed keyset queries for the live moderation case list.
use super::cases::{CASE_SUMMARY_SQL, parse_case_summary};
use super::tenant_logs::verify_scope;
use super::*;
use crate::db_compat::{Value, params_from_iter};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaseSort {
    Date,
    Case,
    Action,
    Subject,
    Actor,
    Outcome,
}

impl CaseSort {
    fn column_and_index(self) -> (&'static str, &'static str) {
        match self {
            Self::Date => ("list_date", "guild_cases_list_date"),
            Self::Case => ("case_id", "guild_cases_visible"),
            Self::Action => ("list_action", "guild_cases_list_action"),
            Self::Subject => ("list_subject", "guild_cases_list_subject"),
            Self::Actor => ("list_actor", "guild_cases_list_actor"),
            Self::Outcome => ("list_outcome", "guild_cases_list_outcome"),
        }
    }

    fn accepts(self, position: &CaseListPosition) -> bool {
        use CaseSortValue::{Number, Text};
        match (self, &position.value) {
            (Self::Date | Self::Subject, Number(value)) => *value > 0,
            (Self::Actor, Number(value)) => *value >= 0,
            (Self::Case, Text(value)) => value == position.case_id.as_str(),
            (Self::Action, Text(value)) => {
                serde_json::from_value::<CaseAction>(serde_json::Value::String(value.clone()))
                    .is_ok()
            }
            (Self::Outcome, Text(value)) => {
                serde_json::from_value::<CaseOutcome>(serde_json::Value::String(value.clone()))
                    .is_ok()
            }
            _ => false,
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum CaseSortValue {
    Number(i64),
    Text(String),
}

impl TryFrom<Value> for CaseSortValue {
    type Error = CaseError;

    fn try_from(value: Value) -> std::result::Result<Self, Self::Error> {
        match value {
            Value::Integer(value) => Ok(Self::Number(value)),
            Value::Text(value) => Ok(Self::Text(value)),
            _ => Err(CaseError::Invalid),
        }
    }
}

impl From<CaseSortValue> for Value {
    fn from(value: CaseSortValue) -> Self {
        match value {
            CaseSortValue::Number(value) => Self::Integer(value),
            CaseSortValue::Text(value) => Self::Text(value),
        }
    }
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CaseListPosition {
    value: CaseSortValue,
    case_id: CaseId,
}

impl CaseListPosition {
    pub fn from_case_id(case_id: CaseId) -> Self {
        Self {
            value: CaseSortValue::Text(case_id.as_str().to_owned()),
            case_id,
        }
    }
}

pub struct CaseListQuery {
    pub filter: CaseFilter,
    pub search: Option<String>,
    pub from_ms: Option<i64>,
    pub until_ms: Option<i64>,
    pub sort: CaseSort,
    pub descending: bool,
    pub after: Option<CaseListPosition>,
}

pub struct CaseListPage {
    pub entries: Vec<(CaseId, CaseSummary)>,
    pub next: Option<CaseListPosition>,
}

impl CaseListQuery {
    pub(super) fn statement(
        self,
        scope: &GuildDataScope,
        count: u32,
        limits: CaseLimits,
    ) -> Result<(String, Vec<Value>)> {
        let (column, index) = self.sort.column_and_index();
        let (direction, cmp) = if self.descending {
            ("DESC", "<")
        } else {
            ("ASC", ">")
        };
        let mut sql = format!(
            "SELECT case_id, {CASE_SUMMARY_SQL}, {column}
             FROM guild_cases INDEXED BY {index}
             WHERE guild_id=?1 AND enrollment_id=?2 AND deleted=0"
        );
        let mut params = vec![
            Value::Integer(scope.guild_id()),
            Value::Text(scope.enrollment.id.as_str().to_owned()),
            Value::Integer(i64::from(limits.max_record_bytes)),
        ];
        if let Some(after) = self.after {
            anyhow::ensure!(self.sort.accepts(&after), CaseError::Invalid);
            if self.sort == CaseSort::Case {
                sql += &format!(" AND case_id {cmp} ?");
            } else {
                sql += &format!(" AND ({column},case_id) {cmp} (?,?)");
                params.push(after.value.into());
            }
            params.push(Value::Text(after.case_id.into()));
        }

        let [action, outcome, subject, actor] = self.filter.sql_values()?;
        for (predicate, value) in [
            ("list_action = ?", action.map(Value::Text)),
            ("list_outcome = ?", outcome.map(Value::Text)),
            (
                "list_subject = CAST(? AS INTEGER)",
                subject.map(Value::Text),
            ),
            ("list_actor = CAST(? AS INTEGER)", actor.map(Value::Text)),
            ("list_date >= ?", self.from_ms.map(Value::Integer)),
            ("list_date < ?", self.until_ms.map(Value::Integer)),
        ] {
            if let Some(value) = value {
                sql += " AND ";
                sql += predicate;
                params.push(value);
            }
        }
        if let Some(search) = self.search {
            let slot = params.len() + 1;
            // A literal substring search over case metadata, without note or evidence reads.
            sql += &format!(
                " AND (instr(lower(COALESCE(json_extract(payload,'$.reason'),'')),lower(?{slot})) > 0
                     OR instr(case_id,lower(?{slot})) > 0
                     OR instr(list_action,lower(?{slot})) > 0
                     OR instr(list_outcome,lower(?{slot})) > 0
                     OR instr(CAST(list_subject AS TEXT),?{slot}) > 0
                     OR instr(CAST(NULLIF(list_actor,0) AS TEXT),?{slot}) > 0)"
            );
            params.push(Value::Text(search));
        }
        sql += &format!(" ORDER BY {column} {direction}");
        if self.sort != CaseSort::Case {
            sql += &format!(", case_id {direction}");
        }
        sql += " LIMIT ?";
        params.push(Value::Integer(i64::from(count) + 1));
        Ok((sql, params))
    }
}

impl GuildDataStore {
    /// Read list entries and the next seek position from the same scoped primary snapshot.
    pub async fn list_cases(
        &self,
        scope: GuildDataScope,
        query: CaseListQuery,
        count: u32,
        limits: CaseLimits,
    ) -> Result<CaseListPage> {
        self.check_target(&scope.placement)?;
        limits.page(count)?;
        let (sql, params) = query.statement(&scope, count, limits)?;
        let expected = self.expected.clone();
        self.db
            .with_conn(move |conn| {
                let tx = conn.transaction_primary_snapshot()?;
                verify_scope(&tx, &expected, &scope, true)?;
                let mut stmt = tx.prepare(&sql)?;
                let mut rows = stmt
                    .query_map(params_from_iter(params), |row| {
                        Ok((
                            row.get::<String>(0)?,
                            row.get::<Option<String>>(1)?,
                            row.get_value(2)?,
                        ))
                    })?
                    .collect::<Result<Vec<_>>>()?;
                let has_next = rows.len() > count as usize;
                if has_next {
                    rows.pop();
                }
                let next = rows
                    .last()
                    .filter(|_| has_next)
                    .map(|(id, _, key)| {
                        Ok::<_, anyhow::Error>(CaseListPosition {
                            value: CaseSortValue::try_from(key.clone())?,
                            case_id: CaseId::try_from(id.clone())?,
                        })
                    })
                    .transpose()?;
                let entries = rows
                    .into_iter()
                    .map(|(id, payload, _)| {
                        Ok((CaseId::try_from(id)?, parse_case_summary(payload)?))
                    })
                    .collect::<Result<_>>()?;
                drop(stmt);
                tx.finish_read()?;
                Ok(CaseListPage { entries, next })
            })
            .await
    }
}
