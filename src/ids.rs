//! Discord snowflake ID serde helpers.
//!
//! JavaScript `Number` / JSON numbers cannot represent snowflakes above
//! `Number.MAX_SAFE_INTEGER` (2^53−1). Wire format for WebUI I/O is **strings**;
//! historical DB JSON may still store bare integers — accept both.

use serde::de::{self, Deserializer, SeqAccess, Visitor};
use serde::ser::{SerializeSeq, Serializer};
use serde::{Deserialize, Serialize};
use std::fmt;

/// Deserialize a single Discord snowflake from a JSON string or integer.
pub fn de_u64<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct U64Visitor;
    impl<'de> Visitor<'de> for U64Visitor {
        type Value = u64;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a Discord snowflake as a string or integer")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<u64, E> {
            Ok(v)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<u64, E> {
            u64::try_from(v).map_err(E::custom)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<u64, E> {
            v.trim().parse().map_err(E::custom)
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<u64, E> {
            self.visit_str(&v)
        }
    }
    deserializer.deserialize_any(U64Visitor)
}

/// Deserialize an `i64` snowflake (channel IDs in DB) from string or integer.
pub fn de_i64<'de, D>(deserializer: D) -> Result<i64, D::Error>
where
    D: Deserializer<'de>,
{
    struct I64Visitor;
    impl<'de> Visitor<'de> for I64Visitor {
        type Value = i64;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("a Discord snowflake as a string or integer")
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<i64, E> {
            i64::try_from(v).map_err(E::custom)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<i64, E> {
            Ok(v)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<i64, E> {
            v.trim().parse().map_err(E::custom)
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<i64, E> {
            self.visit_str(&v)
        }
    }
    deserializer.deserialize_any(I64Visitor)
}

/// Serialize a snowflake as a decimal string (safe for JS `JSON.parse`).
pub fn ser_u64_str<S>(v: &u64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&v.to_string())
}

/// Serialize an `i64` snowflake as a decimal string.
pub fn ser_i64_str<S>(v: &i64, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.serialize_str(&v.to_string())
}

/// Serialize `Option<i64>` snowflakes as string or JSON null.
pub fn ser_opt_i64_str<S>(v: &Option<i64>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    match v {
        Some(id) => serializer.serialize_some(&id.to_string()),
        None => serializer.serialize_none(),
    }
}

/// Deserialize `Option<i64>` from null, string, or integer.
pub fn de_opt_i64<'de, D>(deserializer: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    struct OptVisitor;
    impl<'de> Visitor<'de> for OptVisitor {
        type Value = Option<i64>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("null or a Discord snowflake as a string or integer")
        }

        fn visit_unit<E: de::Error>(self) -> Result<Option<i64>, E> {
            Ok(None)
        }

        fn visit_none<E: de::Error>(self) -> Result<Option<i64>, E> {
            Ok(None)
        }

        fn visit_some<D2: Deserializer<'de>>(
            self,
            deserializer: D2,
        ) -> Result<Option<i64>, D2::Error> {
            de_i64(deserializer).map(Some)
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Option<i64>, E> {
            i64::try_from(v).map(Some).map_err(E::custom)
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Option<i64>, E> {
            Ok(Some(v))
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Option<i64>, E> {
            v.trim().parse().map(Some).map_err(E::custom)
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Option<i64>, E> {
            self.visit_str(&v)
        }
    }
    deserializer.deserialize_any(OptVisitor)
}

/// Deserialize `Option<u64>` from null, string, or integer (query/JSON).
pub fn de_opt_u64<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    struct OptVisitor;
    impl<'de> Visitor<'de> for OptVisitor {
        type Value = Option<u64>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("null or a Discord snowflake as a string or integer")
        }

        fn visit_unit<E: de::Error>(self) -> Result<Option<u64>, E> {
            Ok(None)
        }

        fn visit_none<E: de::Error>(self) -> Result<Option<u64>, E> {
            Ok(None)
        }

        fn visit_some<D2: Deserializer<'de>>(
            self,
            deserializer: D2,
        ) -> Result<Option<u64>, D2::Error> {
            de_u64(deserializer).map(Some)
        }

        fn visit_u64<E: de::Error>(self, v: u64) -> Result<Option<u64>, E> {
            Ok(Some(v))
        }

        fn visit_i64<E: de::Error>(self, v: i64) -> Result<Option<u64>, E> {
            u64::try_from(v).map(Some).map_err(E::custom)
        }

        fn visit_str<E: de::Error>(self, v: &str) -> Result<Option<u64>, E> {
            let t = v.trim();
            if t.is_empty() {
                return Ok(None);
            }
            t.parse().map(Some).map_err(E::custom)
        }

        fn visit_string<E: de::Error>(self, v: String) -> Result<Option<u64>, E> {
            self.visit_str(&v)
        }
    }
    deserializer.deserialize_any(OptVisitor)
}

/// Deserialize `Vec<u64>` where each element may be a string or integer.
pub fn de_u64_vec<'de, D>(deserializer: D) -> Result<Vec<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    struct VecVisitor;
    impl<'de> Visitor<'de> for VecVisitor {
        type Value = Vec<u64>;

        fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.write_str("an array of Discord snowflakes (string or integer)")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Vec<u64>, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(v) = seq.next_element::<SnowflakeU64>()? {
                out.push(v.0);
            }
            Ok(out)
        }
    }
    deserializer.deserialize_seq(VecVisitor)
}

/// Serialize `Vec<u64>` as an array of decimal strings.
pub fn ser_u64_vec_str<S>(v: &[u64], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut seq = serializer.serialize_seq(Some(v.len()))?;
    for id in v {
        seq.serialize_element(&id.to_string())?;
    }
    seq.end()
}

/// Newtype used only inside `de_u64_vec`.
#[derive(Debug)]
struct SnowflakeU64(u64);

impl<'de> Deserialize<'de> for SnowflakeU64 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(de_u64(deserializer)?))
    }
}

/// Transparent u64 that serializes as a string and deserializes from string|int.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Snowflake(pub u64);

impl Serialize for Snowflake {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        ser_u64_str(&self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for Snowflake {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self(de_u64(deserializer)?))
    }
}

impl From<u64> for Snowflake {
    fn from(v: u64) -> Self {
        Self(v)
    }
}

impl From<Snowflake> for u64 {
    fn from(v: Snowflake) -> Self {
        v.0
    }
}

/// JavaScript `Number.MAX_SAFE_INTEGER` (2^53−1). Integers above this lose
/// precision when `JSON.parse`d in the browser.
pub const JS_MAX_SAFE_INTEGER: u64 = (1u64 << 53) - 1;

/// Recursively rewrite JSON integers that exceed JS safe integer range as
/// decimal strings. Used for audit-event `data` blobs so the WebUI can
/// `JSON.parse` without corrupting Discord snowflakes (`channel_id`,
/// `message_id`, …). Values already strings / small numbers are unchanged.
pub fn stringify_js_unsafe_integers(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::{Map, Value};
    match value {
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), stringify_js_unsafe_integers(v));
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(stringify_js_unsafe_integers).collect()),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                if u > JS_MAX_SAFE_INTEGER {
                    return Value::String(u.to_string());
                }
            } else if let Some(i) = n.as_i64() {
                let abs = i.unsigned_abs();
                if abs > JS_MAX_SAFE_INTEGER {
                    return Value::String(i.to_string());
                }
            }
            // Preserve exact representation for safe integers / floats.
            Value::Number(n.clone())
        }
        other => other.clone(),
    }
}

/// Parse a stored event-data JSON string and rewrite unsafe integers for the
/// WebUI wire. Non-JSON blobs are returned unchanged.
pub fn wire_safe_event_data_json(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => stringify_js_unsafe_integers(&v).to_string(),
        Err(_) => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use serde_json::json;

    #[derive(Deserialize)]
    struct Body {
        #[serde(deserialize_with = "de_u64")]
        id: u64,
    }

    #[test]
    fn accepts_string_and_integer_snowflakes() {
        let s: Body = serde_json::from_str(r#"{"id":"9007199254740993"}"#).unwrap();
        assert_eq!(s.id, 9007199254740993);
        // Integers that fit in JSON number still parse (legacy / same-origin tooling).
        let n: Body = serde_json::from_str(r#"{"id":12345}"#).unwrap();
        assert_eq!(n.id, 12345);
    }

    #[test]
    fn vec_accepts_string_elements() {
        #[derive(Deserialize)]
        struct W {
            #[serde(deserialize_with = "de_u64_vec")]
            roles: Vec<u64>,
        }
        let w: W = serde_json::from_str(r#"{"roles":["9007199254740993", 99]}"#).unwrap();
        assert_eq!(w.roles, vec![9007199254740993, 99]);
    }

    #[test]
    fn snowflake_roundtrips_as_string() {
        let s = Snowflake(9007199254740993);
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#""9007199254740993""#);
        let back: Snowflake = serde_json::from_str(&json).unwrap();
        assert_eq!(back.0, 9007199254740993);
    }

    #[test]
    fn opt_i64_accepts_null_string_and_integer() {
        #[derive(Deserialize, serde::Serialize)]
        struct W {
            #[serde(
                default,
                serialize_with = "ser_opt_i64_str",
                deserialize_with = "de_opt_i64"
            )]
            id: Option<i64>,
        }
        let none: W = serde_json::from_str(r#"{"id":null}"#).unwrap();
        assert_eq!(none.id, None);
        let missing: W = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(missing.id, None);
        let s: W = serde_json::from_str(r#"{"id":"9007199254740993"}"#).unwrap();
        assert_eq!(s.id, Some(9007199254740993));
        let n: W = serde_json::from_str(r#"{"id":99}"#).unwrap();
        assert_eq!(n.id, Some(99));
        assert_eq!(
            serde_json::to_string(&s).unwrap(),
            r#"{"id":"9007199254740993"}"#
        );
    }

    #[test]
    fn opt_u64_accepts_null_string_integer_and_empty() {
        #[derive(Deserialize)]
        struct W {
            #[serde(default, deserialize_with = "de_opt_u64")]
            user: Option<u64>,
        }
        let none: W = serde_json::from_str(r#"{"user":null}"#).unwrap();
        assert_eq!(none.user, None);
        let missing: W = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(missing.user, None);
        let empty: W = serde_json::from_str(r#"{"user":""}"#).unwrap();
        assert_eq!(empty.user, None);
        let s: W = serde_json::from_str(r#"{"user":"9007199254740993"}"#).unwrap();
        assert_eq!(s.user, Some(9007199254740993));
        let n: W = serde_json::from_str(r#"{"user":99}"#).unwrap();
        assert_eq!(n.user, Some(99));
    }

    #[test]
    fn stringify_js_unsafe_integers_rewrites_snowflakes_only() {
        let v = json!({
            "channel_id": 9007199254740993u64,
            "message_id": 99,
            "nested": { "id": 9007199254740994u64 },
            "list": [1, 9007199254740995u64],
            "name": "ok",
            "flag": true,
        });
        let out = stringify_js_unsafe_integers(&v);
        assert_eq!(out["channel_id"], "9007199254740993");
        assert_eq!(out["message_id"], 99);
        assert_eq!(out["nested"]["id"], "9007199254740994");
        assert_eq!(out["list"][0], 1);
        assert_eq!(out["list"][1], "9007199254740995");
        assert_eq!(out["name"], "ok");
        assert_eq!(out["flag"], true);
    }
}
