use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::api_keys::ApiKeyStore;
use crate::service::ServiceError;
use crate::usage_tracker::{
    UsageAggregateRow, UsageDimension, UsageFilters, UsageQuery, UsageTracker,
};

// Include leap years so both rolling-year and calendar-year queries fit.
const MAX_QUERY_RANGE_DAYS: i64 = 366;
const MAX_BUCKETS: i64 = 10_000;
const MAX_RESULT_ROWS: i64 = 10_000;
const MAX_INTERMEDIATE_ROWS: i64 = 10_000;
const MAX_FILTER_VALUES: usize = 100;
const MAX_LABEL_CANDIDATE_KEYS: i64 = 1_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageGrouping {
    Global,
    Dimension(UsageDimension),
    ApiKeyLabel(String),
}

#[derive(Debug, Clone)]
pub struct QueryUsageInput {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub bucket_size: String,
    pub grouping: UsageGrouping,
    pub filters: UsageFilters,
    pub api_key_labels: HashMap<String, String>,
}

pub struct UsageService {
    tracker: Option<UsageTracker>,
    api_key_store: Arc<dyn ApiKeyStore>,
}

impl UsageService {
    pub fn new(tracker: Option<UsageTracker>, api_key_store: Arc<dyn ApiKeyStore>) -> Self {
        Self {
            tracker,
            api_key_store,
        }
    }

    pub async fn query(
        &self,
        input: QueryUsageInput,
    ) -> Result<Vec<UsageAggregateRow>, ServiceError> {
        let QueryUsageInput {
            start,
            end,
            bucket_size,
            grouping,
            mut filters,
            api_key_labels,
        } = input;
        let tracker = self.tracker.as_ref().ok_or_else(|| {
            ServiceError::failed_precondition(
                "Usage tracking is disabled (set ROUTER_USAGE_BACKEND)",
            )
        })?;
        let bucket_seconds = bucket_seconds(&bucket_size)?;
        validate_bounds(start, end, bucket_seconds)?;
        validate_filters(&filters, &api_key_labels)?;
        validate_grouping(&grouping)?;

        let required_label_key = match &grouping {
            UsageGrouping::ApiKeyLabel(label_key) => Some(label_key.as_str()),
            UsageGrouping::Global | UsageGrouping::Dimension(_) => None,
        };
        let label_grouping = required_label_key.is_some();
        let label_query = label_grouping || !api_key_labels.is_empty();
        let mut label_values = HashMap::new();
        if label_query {
            let candidates = self
                .api_key_store
                .select_by_labels(
                    &api_key_labels,
                    required_label_key,
                    &filters.api_key_hashes,
                    MAX_LABEL_CANDIDATE_KEYS + 1,
                )
                .await?;
            if candidates.len() > MAX_LABEL_CANDIDATE_KEYS as usize {
                return Err(ServiceError::invalid_argument(format!(
                    "API-key label selection exceeds {MAX_LABEL_CANDIDATE_KEYS} keys; add a label or API-key filter"
                )));
            }

            if candidates.is_empty() {
                return Ok(Vec::new());
            }

            if let Some(label_key) = required_label_key {
                for key in &candidates {
                    if let Some(value) = key.labels.get(label_key) {
                        label_values.insert(key.key_hash.clone(), value.clone());
                    }
                }
            }
            filters.api_key_hashes = candidates
                .into_iter()
                .map(|key| key.key_hash.clone())
                .collect();
        }

        let tracker_grouping = match grouping {
            UsageGrouping::Global => UsageDimension::None,
            UsageGrouping::Dimension(dimension) => dimension,
            UsageGrouping::ApiKeyLabel(_) => UsageDimension::ApiKey,
        };
        let rows = tracker
            .query(&UsageQuery {
                start,
                end,
                bucket_seconds,
                group_by: tracker_grouping,
                filters,
                row_limit: if label_grouping {
                    MAX_INTERMEDIATE_ROWS
                } else {
                    MAX_RESULT_ROWS
                },
            })
            .await
            .map_err(|error| {
                tracing::error!(%error, "Failed to query usage aggregates");
                ServiceError::internal("Failed to query usage data")
            })?;
        let row_limit = (if label_grouping {
            MAX_INTERMEDIATE_ROWS
        } else {
            MAX_RESULT_ROWS
        }) as usize;
        if rows.len() > row_limit {
            return Err(ServiceError::invalid_argument(
                "usage query exceeds the intermediate row limit; narrow the range or filters",
            ));
        }

        let rows = if label_grouping {
            fold_label_rows(rows, &label_values)?
        } else {
            rows
        };
        if rows.len() > MAX_RESULT_ROWS as usize {
            return Err(ServiceError::invalid_argument(format!(
                "usage query exceeds the {MAX_RESULT_ROWS}-row limit; narrow the range or filters"
            )));
        }
        Ok(rows)
    }
}

fn fold_label_rows(
    rows: Vec<UsageAggregateRow>,
    label_values: &HashMap<String, String>,
) -> Result<Vec<UsageAggregateRow>, ServiceError> {
    let mut folded = BTreeMap::<(DateTime<Utc>, String), UsageAggregateRow>::new();
    for row in rows {
        let Some(label_value) = label_values.get(&row.dimension_value) else {
            continue;
        };
        let key = (row.bucket, label_value.clone());
        let aggregate = folded.entry(key.clone()).or_insert(UsageAggregateRow {
            bucket: key.0,
            dimension_value: key.1,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: 0,
            request_count: 0,
            estimated_cost_usd: None,
            unpriced_request_count: 0,
            connection_id: None,
        });
        for (target, value) in [
            (&mut aggregate.prompt_tokens, row.prompt_tokens),
            (&mut aggregate.completion_tokens, row.completion_tokens),
            (&mut aggregate.cached_tokens, row.cached_tokens),
            (&mut aggregate.cache_write_tokens, row.cache_write_tokens),
            (&mut aggregate.reasoning_tokens, row.reasoning_tokens),
            (&mut aggregate.total_tokens, row.total_tokens),
            (&mut aggregate.request_count, row.request_count),
            (
                &mut aggregate.unpriced_request_count,
                row.unpriced_request_count,
            ),
        ] {
            *target = target
                .checked_add(value)
                .ok_or_else(|| ServiceError::internal("usage aggregate exceeds int64"))?;
        }
        // Folding an unpriced key into a priced one must not turn the missing
        // cost into a zero, and folding two unpriced keys must stay unpriced.
        if let Some(cost) = row.estimated_cost_usd {
            *aggregate.estimated_cost_usd.get_or_insert(0.0) += cost;
        }
    }
    Ok(folded.into_values().collect())
}

fn bucket_seconds(bucket_size: &str) -> Result<i64, ServiceError> {
    match bucket_size {
        "1m" => Ok(60),
        "5m" => Ok(300),
        "15m" => Ok(900),
        "1h" => Ok(3_600),
        "6h" => Ok(21_600),
        "1d" => Ok(86_400),
        "7d" | "1w" => Ok(604_800),
        _ => Err(ServiceError::invalid_field(
            "bucket_size",
            "bucket_size must be one of 1m, 5m, 15m, 1h, 6h, 1d, or 7d",
        )),
    }
}

/// Bound ledger scans and bucket counts independently, including annual windows.
fn validate_bounds(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    bucket_seconds: i64,
) -> Result<(), ServiceError> {
    let duration = end.signed_duration_since(start);
    let seconds = duration.num_seconds();
    if seconds <= 0 {
        return Err(ServiceError::invalid_field(
            "end",
            "end must be later than start",
        ));
    }
    if duration > chrono::Duration::days(MAX_QUERY_RANGE_DAYS) {
        return Err(ServiceError::invalid_argument(format!(
            "usage query range may not exceed {MAX_QUERY_RANGE_DAYS} days"
        )));
    }
    let bucket_count = seconds.saturating_add(bucket_seconds - 1) / bucket_seconds;
    if bucket_count > MAX_BUCKETS {
        return Err(ServiceError::invalid_field(
            "bucket_size",
            format!("usage query may not exceed {MAX_BUCKETS} buckets"),
        ));
    }
    Ok(())
}

fn validate_filters(
    filters: &UsageFilters,
    api_key_labels: &HashMap<String, String>,
) -> Result<(), ServiceError> {
    for (field, values) in [
        ("api_key_hashes", &filters.api_key_hashes),
        ("usage_pool_ids", &filters.usage_pool_ids),
        ("connection_ids", &filters.connection_ids),
        ("models", &filters.models),
    ] {
        if values.len() > MAX_FILTER_VALUES {
            return Err(ServiceError::invalid_field(
                field,
                format!("{field} may contain at most {MAX_FILTER_VALUES} values"),
            ));
        }
        if values.iter().any(|value| value.is_empty()) {
            return Err(ServiceError::invalid_field(
                field,
                format!("{field} may not contain empty values"),
            ));
        }
    }
    crate::labels::validate(api_key_labels)
        .map_err(|error| ServiceError::invalid_field("filters.api_key_labels", error.to_string()))
}

fn validate_grouping(grouping: &UsageGrouping) -> Result<(), ServiceError> {
    match grouping {
        UsageGrouping::ApiKeyLabel(label_key) => {
            let label = HashMap::from([(label_key.to_string(), "x".to_string())]);
            crate::labels::validate(&label).map_err(|error| {
                ServiceError::invalid_field("grouping.api_key_label", error.to_string())
            })
        }
        UsageGrouping::Dimension(UsageDimension::None) => Err(ServiceError::invalid_field(
            "grouping.dimension",
            "global grouping must use the explicit global variant",
        )),
        UsageGrouping::Global | UsageGrouping::Dimension(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Annual support must retain ordering and bucket-density safeguards.
    #[test]
    fn bounds_require_order_range_and_bucket_limits() {
        let start = DateTime::from_timestamp(0, 0).unwrap();
        assert!(validate_bounds(start, start, 60).is_err());
        assert!(validate_bounds(start, start - chrono::Duration::seconds(1), 60).is_err());
        assert!(validate_bounds(start, start + chrono::Duration::days(8), 60).is_err());
        assert!(validate_bounds(start, start + chrono::Duration::days(6), 60).is_ok());
    }

    /// Rolling years and leap years fit, but oversized or overly dense windows do not.
    #[test]
    fn annual_bounds_allow_years_and_preserve_limits() {
        let start = DateTime::parse_from_rfc3339("2024-01-01T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        for days in [90, 91, 365, 366] {
            for bucket in ["1h", "1d", "7d"] {
                assert!(validate_bounds(
                    start,
                    start + chrono::Duration::days(days),
                    bucket_seconds(bucket).unwrap(),
                )
                .is_ok());
            }
        }
        let leap_year_end = start + chrono::Duration::days(366);
        assert_eq!(leap_year_end.to_rfc3339(), "2025-01-01T00:00:00+00:00");
        for extra in [chrono::Duration::nanoseconds(1), chrono::Duration::days(1)] {
            assert!(validate_bounds(start, leap_year_end + extra, 604_800).is_err());
        }
        for bucket in ["1m", "5m", "15m"] {
            assert!(
                validate_bounds(start, leap_year_end, bucket_seconds(bucket).unwrap()).is_err()
            );
        }
    }

    #[test]
    fn grouping_variants_are_validated_independently() {
        assert!(validate_grouping(&UsageGrouping::Global).is_ok());
        assert!(validate_grouping(&UsageGrouping::Dimension(UsageDimension::Connection)).is_ok());
        assert!(validate_grouping(&UsageGrouping::Dimension(UsageDimension::None)).is_err());
        assert!(validate_grouping(&UsageGrouping::ApiKeyLabel("feature".into())).is_ok());
        assert!(validate_grouping(&UsageGrouping::ApiKeyLabel("".into())).is_err());
        assert!(validate_grouping(&UsageGrouping::ApiKeyLabel("chalk.secret".into())).is_err());
    }

    fn labelled_row(key: &str, tokens: i64, cost: Option<f64>) -> UsageAggregateRow {
        UsageAggregateRow {
            bucket: DateTime::from_timestamp(0, 0).unwrap(),
            dimension_value: key.to_string(),
            prompt_tokens: tokens,
            completion_tokens: 1,
            cached_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: tokens + 1,
            request_count: 1,
            estimated_cost_usd: cost,
            unpriced_request_count: i64::from(cost.is_none()),
            connection_id: None,
        }
    }

    fn chat_labels() -> HashMap<String, String> {
        HashMap::from([
            ("key-a".to_string(), "chat".to_string()),
            ("key-b".to_string(), "chat".to_string()),
        ])
    }

    #[test]
    fn label_post_aggregation_folds_api_keys_without_exposing_hashes() {
        let rows = fold_label_rows(
            vec![
                labelled_row("key-a", 2, Some(0.25)),
                labelled_row("key-b", 3, Some(0.5)),
            ],
            &chat_labels(),
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].dimension_value, "chat");
        assert_eq!(rows[0].prompt_tokens, 5);
        assert_eq!(rows[0].total_tokens, 7);
        assert_eq!(rows[0].request_count, 2);
        assert_eq!(rows[0].estimated_cost_usd, Some(0.75));
        assert_eq!(rows[0].unpriced_request_count, 0);
    }

    #[test]
    fn folding_an_unpriced_key_keeps_the_priced_cost_and_counts_the_gap() {
        let rows = fold_label_rows(
            vec![
                labelled_row("key-a", 2, Some(0.25)),
                labelled_row("key-b", 3, None),
            ],
            &chat_labels(),
        )
        .unwrap();
        assert_eq!(rows[0].estimated_cost_usd, Some(0.25));
        assert_eq!(rows[0].unpriced_request_count, 1);
    }

    #[test]
    fn folding_only_unpriced_keys_stays_unpriced_rather_than_free() {
        let rows = fold_label_rows(
            vec![
                labelled_row("key-a", 2, None),
                labelled_row("key-b", 3, None),
            ],
            &chat_labels(),
        )
        .unwrap();
        assert_eq!(rows[0].estimated_cost_usd, None);
        assert_eq!(rows[0].unpriced_request_count, 2);
    }
}
