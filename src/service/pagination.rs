//! Shared cursor-pagination helpers. Existing rate-limit and usage-pool RPCs
//! use timestamp cursors; newer collections use opaque `(created_at, id)`
//! cursors so equal timestamps cannot skip records.

use base64::Engine;
use chrono::{DateTime, SecondsFormat, Utc};

use crate::service::ServiceError;

/// Default page size when `limit` is unset.
pub const DEFAULT_LIMIT: i64 = 40;
/// Upper bound a caller-supplied `limit` is clamped to.
pub const MAX_LIMIT: i64 = 1000;

/// Opaque cursor for collections ordered by `(created_at DESC, id DESC)`.
/// Including the immutable ID prevents records with equal timestamps from
/// being skipped at page boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StablePageCursor {
    pub created_at: DateTime<Utc>,
    pub id: String,
}

impl StablePageCursor {
    pub fn encode(&self) -> String {
        // Preserve nanoseconds for in-memory development stores; PostgreSQL
        // values naturally round-trip with zeroes below microsecond precision.
        let payload = format!(
            "{}|{}",
            self.created_at.to_rfc3339_opts(SecondsFormat::Nanos, true),
            self.id
        );
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload)
    }

    pub fn decode(raw: &str) -> Result<Self, ServiceError> {
        let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw)
            .map_err(|_| ServiceError::invalid_field("cursor", "not a valid cursor"))?;
        let payload = String::from_utf8(decoded)
            .map_err(|_| ServiceError::invalid_field("cursor", "not a valid cursor"))?;
        let (timestamp, id) = payload
            .split_once('|')
            .ok_or_else(|| ServiceError::invalid_field("cursor", "not a valid cursor"))?;
        if id.is_empty() {
            return Err(ServiceError::invalid_field("cursor", "not a valid cursor"));
        }
        let created_at = DateTime::parse_from_rfc3339(timestamp)
            .map_err(|_| ServiceError::invalid_field("cursor", "not a valid cursor"))?
            .with_timezone(&Utc);
        Ok(Self {
            created_at,
            id: id.to_string(),
        })
    }
}

pub fn parse_stable_cursor(cursor: Option<&str>) -> Result<Option<StablePageCursor>, ServiceError> {
    match cursor {
        None | Some("") => Ok(None),
        Some(raw) => StablePageCursor::decode(raw).map(Some),
    }
}

/// Parse a pagination cursor. `None` or an empty string default to "now"
/// (i.e. the first page starts from the most recent row). Anything else must
/// parse as RFC 3339, or this returns an `invalid_field` [`ServiceError`].
pub fn parse_cursor(cursor: Option<&str>) -> Result<DateTime<Utc>, ServiceError> {
    match cursor {
        None => Ok(Utc::now()),
        Some("") => Ok(Utc::now()),
        Some(s) => DateTime::parse_from_rfc3339(s)
            .map(|dt| dt.with_timezone(&Utc))
            .map_err(|e| {
                ServiceError::invalid_field(
                    "cursor",
                    format!("not a valid RFC 3339 timestamp: {e}"),
                )
            }),
    }
}

/// Render a timestamp as the cursor string format: RFC 3339 with microsecond
/// precision and a `Z` (rather than `+00:00`) UTC suffix. Round-trips exactly
/// through [`parse_cursor`] for any Postgres `timestamptz` value.
pub fn format_cursor(ts: DateTime<Utc>) -> String {
    ts.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Resolve a caller-supplied `limit` into the actual page size to fetch.
/// `None` defaults to [`DEFAULT_LIMIT`]; `<= 0` is rejected as an
/// `invalid_field` [`ServiceError`]; anything above [`MAX_LIMIT`] is
/// silently clamped down to it.
pub fn resolve_limit(limit: Option<i32>) -> Result<i64, ServiceError> {
    match limit {
        None => Ok(DEFAULT_LIMIT),
        Some(n) if n <= 0 => Err(ServiceError::invalid_field(
            "limit",
            "must be greater than 0",
        )),
        Some(n) => Ok((n as i64).min(MAX_LIMIT)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cursor_none_defaults_to_now() {
        let before = Utc::now();
        let parsed = parse_cursor(None).expect("none is valid");
        let after = Utc::now();
        assert!(parsed >= before && parsed <= after);
    }

    #[test]
    fn parse_cursor_empty_string_defaults_to_now() {
        let before = Utc::now();
        let parsed = parse_cursor(Some("")).expect("empty is valid");
        let after = Utc::now();
        assert!(parsed >= before && parsed <= after);
    }

    #[test]
    fn parse_cursor_rejects_garbage() {
        let err = parse_cursor(Some("not-a-timestamp")).unwrap_err();
        assert!(
            matches!(err, ServiceError::InvalidArgument { field: Some(ref f), .. } if f == "cursor")
        );
    }

    #[test]
    fn format_then_parse_round_trips() {
        // Truncate to microsecond precision up front since that's the
        // precision the format preserves.
        let ts = Utc::now();
        let micros = ts.timestamp_subsec_micros();
        let ts = DateTime::from_timestamp(ts.timestamp(), micros * 1000).unwrap();

        let formatted = format_cursor(ts);
        let parsed = parse_cursor(Some(&formatted)).expect("round-trip parse");
        assert_eq!(parsed, ts);
    }

    #[test]
    fn format_cursor_uses_z_suffix() {
        let ts = DateTime::from_timestamp(0, 0).unwrap();
        assert_eq!(format_cursor(ts), "1970-01-01T00:00:00.000000Z");
    }

    #[test]
    fn resolve_limit_none_defaults() {
        assert_eq!(resolve_limit(None).unwrap(), 40);
    }

    #[test]
    fn resolve_limit_rejects_zero_and_negative() {
        assert!(resolve_limit(Some(0)).is_err());
        assert!(resolve_limit(Some(-1)).is_err());
    }

    #[test]
    fn resolve_limit_clamps_above_max() {
        assert_eq!(resolve_limit(Some(5000)).unwrap(), MAX_LIMIT);
    }

    #[test]
    fn resolve_limit_passes_through_in_range() {
        assert_eq!(resolve_limit(Some(10)).unwrap(), 10);
    }

    #[test]
    fn stable_cursor_is_opaque_and_round_trips() {
        let cursor = StablePageCursor {
            created_at: DateTime::from_timestamp(1_700_000_000, 123_456_789).unwrap(),
            id: "route-123".to_string(),
        };
        let encoded = cursor.encode();
        assert!(!encoded.contains("route-123"));
        assert_eq!(StablePageCursor::decode(&encoded).unwrap(), cursor);
        assert!(StablePageCursor::decode("not-base64").is_err());
    }

    #[test]
    fn stable_cursor_rejects_empty_timestamp() {
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode("|row-123");
        assert!(StablePageCursor::decode(&encoded).is_err());
    }
}
