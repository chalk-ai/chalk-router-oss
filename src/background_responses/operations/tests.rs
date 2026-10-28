use super::*;
use crate::service::BackgroundResponseService;

fn entry(phase: &str, settled_at: Option<DateTime<Utc>>) -> MemoryJob {
    let mut job = super::super::tests::job();
    job.phase = phase.to_string();
    job.submitted_at = Utc::now() - chrono::Duration::days(100);
    MemoryJob {
        job,
        due: Utc::now() + chrono::Duration::hours(1),
        claim_until: None,
        settled_at,
        last_error_code: None,
    }
}

async fn insert(runtime: &BackgroundResponses, entry: MemoryJob) -> Uuid {
    let event_id = entry.job.event_id;
    let mut memory = runtime.memory.lock().await;
    if let Some(settled_at) = entry.settled_at {
        memory.settled.insert((settled_at, event_id));
    }
    memory.jobs.insert(event_id, entry);
    event_id
}

fn runtime(days: u32) -> Arc<BackgroundResponses> {
    Arc::new(BackgroundResponses::new(
        None,
        "test".to_string(),
        PollConfig::default().with_retention_days(days),
    ))
}

#[tokio::test]
async fn memory_listing_is_stable_filtered_bounded_and_validated() {
    let runtime = runtime(30);
    let timestamp = Utc::now();
    let mut identifiers = Vec::new();
    for _ in 0..105 {
        let mut entry = entry("active", None);
        entry.job.submitted_at = timestamp;
        entry.job.upstream_status = Some("private unknown status".to_string());
        entry.last_error_code = Some("private upstream error".to_string());
        identifiers.push(insert(&runtime, entry).await);
    }
    insert(&runtime, entry("settled", Some(timestamp))).await;
    let service = BackgroundResponseService::new(runtime.clone());
    let first = service.list(None, None, Some(5000)).await.unwrap();
    assert_eq!(first.jobs.len(), 100);
    assert_eq!(first.jobs[0].upstream_status.as_deref(), Some("unknown"));
    assert_eq!(
        first.jobs[0].last_error_code.as_deref(),
        Some("upstream_or_settlement_unavailable")
    );
    let second = service
        .list(None, first.next_cursor.as_deref(), Some(100))
        .await
        .unwrap();
    assert_eq!(second.jobs.len(), 5);
    assert!(second.next_cursor.is_none());
    let mut uppercase_cursor =
        StablePageCursor::decode(first.next_cursor.as_ref().unwrap()).unwrap();
    uppercase_cursor.id = uppercase_cursor.id.to_uppercase();
    let uppercase_page = service
        .list(None, Some(&uppercase_cursor.encode()), Some(100))
        .await
        .unwrap();
    assert_eq!(
        uppercase_page
            .jobs
            .iter()
            .map(|job| job.event_id)
            .collect::<Vec<_>>(),
        second
            .jobs
            .iter()
            .map(|job| job.event_id)
            .collect::<Vec<_>>()
    );
    let actual: BTreeSet<_> = first
        .jobs
        .iter()
        .chain(&second.jobs)
        .map(|job| job.event_id)
        .collect();
    assert_eq!(actual, identifiers.into_iter().collect());
    assert_eq!(
        service
            .list(Some("settled"), None, None)
            .await
            .unwrap()
            .jobs
            .len(),
        1
    );
    assert!(service.list(Some("wrong"), None, None).await.is_err());
    assert!(service.list(None, Some("invalid"), None).await.is_err());
    assert!(service.list(None, None, Some(0)).await.is_err());
    let invalid_cursor = StablePageCursor {
        created_at: timestamp,
        id: "not-a-uuid".to_string(),
    }
    .encode();
    assert!(service
        .list(None, Some(&invalid_cursor), None)
        .await
        .is_err());
}

#[tokio::test]
async fn memory_retention_uses_settlement_age_and_bounded_exclusive_batches() {
    let runtime = runtime(30);
    let old = Utc::now() - chrono::Duration::days(31);
    for _ in 0..101 {
        insert(&runtime, entry("settled", Some(old))).await;
    }
    let recent = insert(&runtime, entry("settled", Some(Utc::now()))).await;
    for phase in ["submitting", "active", "terminal", "unresolved"] {
        insert(&runtime, entry(phase, None)).await;
    }
    let mut claimed = entry("settled", Some(old));
    claimed.claim_until = Some(Utc::now() + chrono::Duration::minutes(1));
    let claimed = insert(&runtime, claimed).await;
    let (first, second) = tokio::join!(runtime.cleanup(), runtime.cleanup());
    let (first, second) = (first.unwrap(), second.unwrap());
    assert!(first <= 100 && second <= 100);
    assert_eq!(first + second, 101);
    assert_eq!(runtime.memory.lock().await.jobs.len(), 6);
    assert!(runtime.memory.lock().await.jobs.contains_key(&recent));
    assert!(runtime.memory.lock().await.jobs.contains_key(&claimed));
    let disabled = BackgroundResponses::new(
        None,
        "test".to_string(),
        PollConfig::default().with_retention_days(0),
    );
    insert(&disabled, entry("settled", Some(old))).await;
    let mut active = entry("active", None);
    active.due = Utc::now();
    insert(&disabled, active).await;
    assert_eq!(disabled.cleanup().await.unwrap(), 0);
    assert_eq!(disabled.claim().await.unwrap().len(), 1);
    assert!(self::runtime(u32::MAX).cleanup().await.is_err());
}
