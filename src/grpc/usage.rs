use tonic::{Request, Response, Status};

use crate::service::usage::{QueryUsageInput, UsageGrouping};
use crate::service::ServiceError;
use crate::usage_tracker::{UsageDimension, UsageFilters};
use crate::AppState;

use super::conv;
use super::proto;
use super::proto::usage_service_server::UsageService;

pub struct UsageServer {
    pub state: AppState,
}

#[tonic::async_trait]
impl UsageService for UsageServer {
    async fn query_usage(
        &self,
        request: Request<proto::QueryUsageRequest>,
    ) -> Result<Response<proto::QueryUsageResponse>, Status> {
        let state = self.state.for_request(&request);
        let request = request.into_inner();
        let start = required_timestamp(request.start.as_ref(), "start")?;
        let end = required_timestamp(request.end.as_ref(), "end")?;
        let grouping = usage_grouping(request.grouping)?;
        let filters = request.filters.unwrap_or_default();
        let rows = state
            .usage_service()
            .query(QueryUsageInput {
                start,
                end,
                bucket_size: if request.bucket_size.is_empty() {
                    "1h".to_string()
                } else {
                    request.bucket_size
                },
                grouping,
                filters: UsageFilters {
                    api_key_hashes: filters.api_key_hashes,
                    usage_pool_ids: filters.usage_pool_ids,
                    connection_ids: filters.connection_ids,
                    models: filters.models,
                },
                api_key_labels: filters.api_key_labels.into_iter().collect(),
            })
            .await?;
        Ok(Response::new(proto::QueryUsageResponse {
            rows: rows
                .into_iter()
                .map(|row| proto::UsageAggregateRow {
                    bucket: Some(conv::ts_to_proto(row.bucket)),
                    dimension_value: row.dimension_value,
                    prompt_tokens: row.prompt_tokens,
                    completion_tokens: row.completion_tokens,
                    cached_tokens: row.cached_tokens,
                    cache_write_tokens: row.cache_write_tokens,
                    reasoning_tokens: row.reasoning_tokens,
                    total_tokens: row.total_tokens,
                    request_count: row.request_count,
                    estimated_cost_usd: row.estimated_cost_usd,
                    // Reported only beside a cost, so a client cannot read
                    // "nothing was priced" as "everything was priced".
                    unpriced_request_count: row
                        .estimated_cost_usd
                        .map(|_| row.unpriced_request_count),
                })
                .collect(),
        }))
    }
}

fn usage_grouping(
    grouping: Option<proto::query_usage_request::Grouping>,
) -> Result<UsageGrouping, ServiceError> {
    use proto::query_usage_request::Grouping;

    match grouping {
        Some(Grouping::Global(true)) => Ok(UsageGrouping::Global),
        Some(Grouping::Global(false)) => Err(ServiceError::invalid_field(
            "grouping.global",
            "global must be true when selected",
        )),
        Some(Grouping::Dimension(raw)) => {
            let dimension = proto::UsageDimension::try_from(raw).map_err(|_| {
                ServiceError::invalid_field("grouping.dimension", "unknown usage dimension")
            })?;
            let group_by = match dimension {
                proto::UsageDimension::Unspecified => {
                    return Err(ServiceError::invalid_field(
                        "grouping.dimension",
                        "usage dimension must be specified",
                    ));
                }
                proto::UsageDimension::Connection => UsageDimension::Connection,
                proto::UsageDimension::Model => UsageDimension::Model,
                proto::UsageDimension::ApiKey => UsageDimension::ApiKey,
                proto::UsageDimension::UsagePool => UsageDimension::UsagePool,
            };
            Ok(UsageGrouping::Dimension(group_by))
        }
        Some(Grouping::ApiKeyLabel(label_key)) => Ok(UsageGrouping::ApiKeyLabel(label_key)),
        None => Err(ServiceError::invalid_field(
            "grouping",
            "one grouping is required",
        )),
    }
}

fn required_timestamp(
    timestamp: Option<&prost_types::Timestamp>,
    field: &'static str,
) -> Result<chrono::DateTime<chrono::Utc>, ServiceError> {
    let timestamp = timestamp
        .ok_or_else(|| ServiceError::invalid_field(field, format!("{field} is required")))?;
    conv::ts_from_proto(timestamp).ok_or_else(|| {
        ServiceError::invalid_field(field, format!("{field} is not a valid timestamp"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proto::query_usage_request::Grouping;

    #[test]
    fn missing_and_unspecified_grouping_are_rejected() {
        let missing: Status = usage_grouping(None).unwrap_err().into();
        assert_eq!(missing.code(), tonic::Code::InvalidArgument);
        let unspecified: Status = usage_grouping(Some(Grouping::Dimension(
            proto::UsageDimension::Unspecified as i32,
        )))
        .unwrap_err()
        .into();
        assert_eq!(unspecified.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn false_global_grouping_is_rejected() {
        let error: Status = usage_grouping(Some(Grouping::Global(false)))
            .unwrap_err()
            .into();
        assert_eq!(error.code(), tonic::Code::InvalidArgument);
    }

    #[test]
    fn explicit_groupings_map_without_side_channels() {
        assert_eq!(
            usage_grouping(Some(Grouping::Global(true))).unwrap(),
            UsageGrouping::Global
        );
        assert_eq!(
            usage_grouping(Some(Grouping::Dimension(
                proto::UsageDimension::Connection as i32
            )))
            .unwrap(),
            UsageGrouping::Dimension(UsageDimension::Connection)
        );
        assert_eq!(
            usage_grouping(Some(Grouping::ApiKeyLabel("team".into()))).unwrap(),
            UsageGrouping::ApiKeyLabel("team".into())
        );
    }
}
