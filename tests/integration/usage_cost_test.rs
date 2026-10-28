//! Optional PostgreSQL coverage for the cost the usage ledger reports.
//!
//! Cost is priced per call at write time, which is what lets an aggregate over
//! a dimension that is not the model — an API key serving several models — sum
//! costs that were each computed from the model that actually produced them.
//! These tests assert that property end to end, and that an unpriced model
//! stays absent from the estimate rather than contributing a zero to it.

use std::collections::HashMap;
use std::sync::Arc;

use openai_server::api_keys::{ApiKeyStore, InMemoryApiKeyStore};
use openai_server::hashing::KeyHasher;
use openai_server::service::usage::{QueryUsageInput, UsageGrouping, UsageService};
use openai_server::usage_tracker::{
    UsageDimension, UsageFilters, UsageRecord, UsageSource, UsageTracker,
};

/// In the checked-in rate card at $3.00/$15.00 per Mtok, with cache rates.
const PRICED_MODEL: &str = "claude-sonnet-4-5";
/// Deliberately absent from every card, so it can never acquire a price and
/// silently turn this test green for the wrong reason.
const UNPRICED_MODEL: &str = "not-a-real-model-xyzzy";

struct Fixture {
    pool: sqlx::PgPool,
    environment_id: String,
    tracker: UsageTracker,
    service: UsageService,
    key_hash: String,
}

async fn fixture(name: &str) -> Option<Fixture> {
    let database_url = std::env::var("TEST_DATABASE_URL")
        .ok()
        .filter(|value| !value.is_empty())?;
    openai_server::migrate::run(&database_url)
        .await
        .expect("apply router migrations");
    let connect_options = openai_server::database::connect_options(&database_url)
        .expect("parse disposable PostgreSQL test database URL");
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_with(connect_options)
        .await
        .expect("connect to disposable PostgreSQL test database");

    // A fresh environment id per test keeps concurrent runs from seeing each
    // other's ledger rows through the shared database.
    let environment_id = format!("usage-cost-{name}-{}", uuid::Uuid::new_v4());
    let tracker = UsageTracker::new(pool.clone(), environment_id.clone())
        .await
        .expect("create usage tracker");
    let store: Arc<dyn ApiKeyStore> = Arc::new(InMemoryApiKeyStore::new(KeyHasher::new(
        b"usage-cost-test-pepper".to_vec(),
    )));
    Some(Fixture {
        pool,
        environment_id: environment_id.clone(),
        service: UsageService::new(Some(tracker.clone()), store),
        tracker,
        key_hash: format!("key-{environment_id}"),
    })
}

impl Fixture {
    async fn record(&self, model: &str, prompt: u64, completion: u64) {
        self.tracker
            .record(UsageRecord {
                event_id: uuid::Uuid::new_v4(),
                time: chrono::Utc::now(),
                key_hash: &self.key_hash,
                usage_pool_id: None,
                connection_id: "conn-anthropic",
                provider_kind: "anthropic",
                model,
                prompt_tokens: prompt,
                completion_tokens: completion,
                cached_tokens: 0,
                cache_write_tokens: 0,
                reasoning_tokens: 0,
                usage_source: UsageSource::Reported,
                input_token_attribution: Default::default(),
            })
            .await
            .expect("persist call");
    }

    async fn query(
        &self,
        grouping: UsageGrouping,
    ) -> Vec<openai_server::usage_tracker::UsageAggregateRow> {
        self.service
            .query(QueryUsageInput {
                start: chrono::Utc::now() - chrono::Duration::hours(1),
                end: chrono::Utc::now() + chrono::Duration::hours(1),
                bucket_size: "1h".to_string(),
                grouping,
                filters: UsageFilters {
                    api_key_hashes: vec![self.key_hash.clone()],
                    ..Default::default()
                },
                api_key_labels: HashMap::new(),
            })
            .await
            .expect("query usage aggregates")
    }
}

/// 1M prompt tokens at $3.00/Mtok plus 1M completion at $15.00/Mtok.
const ONE_MTOK: u64 = 1_000_000;
const ONE_MTOK_COST: f64 = 18.0;

fn approx(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "{actual} is not approximately {expected}"
    );
}

#[tokio::test]
async fn gemini_embedding_http_records_modality_cost_and_unpriced_gaps_once() {
    use super::gemini_embeddings_test::{send, state};
    use crate::support::mock::serve;
    use axum::{routing::post, Json, Router};
    use serde_json::json;

    let Some(fixture) = fixture("gemini-http").await else {
        return;
    };
    for details in [
        Some(json!([{"modality":"TEXT", "tokenCount":7}, {"modality":"IMAGE", "tokenCount":258}])),
        None,
        Some(json!([{"modality":"IMAGE", "tokenCount":258}])),
    ] {
        let upstream = serve(Router::new().route(
            "/custom/v1beta/models/gemini-embedding-2:embedContent",
            post(move || {
                let details = details.clone();
                async move {
                    let mut metadata = json!({"promptTokenCount":265});
                    if let Some(details) = details {
                        metadata["promptTokenDetails"] = details;
                    }
                    Json(json!({"embedding":{"values":[0.5]}, "usageMetadata":metadata}))
                }
            }),
        ))
        .await;
        let mut state = state(&upstream).await;
        state.usage_tracker = Some(fixture.tracker.clone());
        let (status, _, body) = send(
            state,
            json!({
                "model":"google/gemini-embedding-2", "input":[
                    {"type":"text", "text":"a dog"},
                    {"type":"image_url", "image_url":{"url":"data:image/png;base64,aW1hZ2U="}}
                ]
            }),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(
            body["usage"],
            json!({"prompt_tokens":265, "total_tokens":265})
        );
    }
    let rows = fixture
        .service
        .query(QueryUsageInput {
            start: chrono::Utc::now() - chrono::Duration::hours(1),
            end: chrono::Utc::now() + chrono::Duration::hours(1),
            bucket_size: "1h".into(),
            grouping: UsageGrouping::Dimension(UsageDimension::Model),
            filters: UsageFilters::default(),
            api_key_labels: HashMap::new(),
        })
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].request_count, 3);
    assert_eq!(rows[0].unpriced_request_count, 2);
    approx(rows[0].estimated_cost_usd.unwrap(), 0.0001175);
    let result: (i64, i64, i64, i64, Option<f64>, i64) = sqlx::query_as(
        "SELECT COUNT(*)::BIGINT, SUM(prompt_tokens)::BIGINT, SUM(completion_tokens)::BIGINT, \
         SUM(total_tokens)::BIGINT, SUM(estimated_cost_usd), \
         COUNT(*) FILTER (WHERE estimated_cost_usd IS NULL)::BIGINT \
         FROM chalk_router.usage_events WHERE environment_id=$1",
    )
    .bind(&fixture.environment_id)
    .fetch_one(&fixture.pool)
    .await
    .unwrap();
    assert_eq!(
        (result.0, result.1, result.2, result.3, result.5),
        (3, 795, 0, 795, 2)
    );
    approx(result.4.expect("one priced mixed request"), 0.0001175);
}

#[tokio::test]
async fn a_priced_model_reports_its_list_price_cost() {
    let Some(fixture) = fixture("priced").await else {
        eprintln!("skipping usage cost test; set TEST_DATABASE_URL");
        return;
    };
    fixture.record(PRICED_MODEL, ONE_MTOK, ONE_MTOK).await;

    let rows = fixture
        .query(UsageGrouping::Dimension(UsageDimension::Model))
        .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].dimension_value, PRICED_MODEL);
    approx(rows[0].estimated_cost_usd.expect("priced"), ONE_MTOK_COST);
    assert_eq!(rows[0].unpriced_request_count, 0);
}

#[tokio::test]
async fn an_unpriced_model_is_absent_from_the_estimate_rather_than_free() {
    let Some(fixture) = fixture("unpriced").await else {
        eprintln!("skipping usage cost test; set TEST_DATABASE_URL");
        return;
    };
    fixture.record(UNPRICED_MODEL, ONE_MTOK, ONE_MTOK).await;

    let rows = fixture
        .query(UsageGrouping::Dimension(UsageDimension::Model))
        .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].estimated_cost_usd, None,
        "an unpriced call must not report a cost of zero"
    );
    assert_eq!(rows[0].unpriced_request_count, 1);
}

#[tokio::test]
async fn a_grouping_that_spans_models_sums_each_models_own_price() {
    let Some(fixture) = fixture("mixed").await else {
        eprintln!("skipping usage cost test; set TEST_DATABASE_URL");
        return;
    };
    fixture.record(PRICED_MODEL, ONE_MTOK, ONE_MTOK).await;
    fixture.record(PRICED_MODEL, ONE_MTOK, ONE_MTOK).await;
    fixture.record(UNPRICED_MODEL, ONE_MTOK, ONE_MTOK).await;

    // Grouped by API key, one row covers all three calls across two models.
    // The pricing therefore cannot have happened at query time.
    let rows = fixture
        .query(UsageGrouping::Dimension(UsageDimension::ApiKey))
        .await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].request_count, 3);
    approx(
        rows[0].estimated_cost_usd.expect("partially priced"),
        2.0 * ONE_MTOK_COST,
    );
    assert_eq!(
        rows[0].unpriced_request_count, 1,
        "the unpriced call must be visible as a gap in the estimate"
    );
}
