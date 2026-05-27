use super::*;

#[tokio::test]
async fn search_command_endpoint_accepts_job() {
    let app = app_router(test_state());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/commands/search")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"utada traveling"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: CommandAccepted = serde_json::from_slice(&body).unwrap();
    assert!(payload.ok);
    assert!(payload.job_id.is_some());
}

#[tokio::test]
async fn search_command_endpoint_rejects_unknown_fields_with_problem_details() {
    let app = app_router(test_state());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/commands/search")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"utada traveling","limit":5}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: ProblemDetails = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload.title, "request validation failed");
    assert!(payload.detail.contains("unknown field"));
}

#[tokio::test]
async fn search_command_endpoint_rejects_malformed_json_with_problem_details() {
    let app = app_router(test_state());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/commands/search")
                .header("content-type", "application/json")
                .body(Body::from("{bad json"))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: ProblemDetails = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload.title, "malformed json request body");
    assert!(payload.detail.contains("Failed to parse"));
}

#[tokio::test]
async fn search_command_endpoint_rejects_empty_query_with_problem_details() {
    let app = app_router(test_state());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/commands/search")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"   "}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: ProblemDetails = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload.title, "request validation failed");
    assert_eq!(
        payload.r#type,
        "https://nocturne.local/problems/query_empty"
    );
    assert!(payload.detail.contains("search query must not be empty"));
}

#[tokio::test]
async fn search_results_endpoint_returns_completed_results() {
    let event_log = LocalEventLog::default();
    let event_publisher = BroadcastEventPublisher::new(event_log, 8);
    let search_runtime = LocalSearchRuntime::default();
    search_runtime.set_fixture(
        "endpoint fixture",
        vec![Song {
            id: String::from("youtube:endpoint-song"),
            title: String::from("Endpoint Fixture Song"),
            channel_name: String::from("Fixture Channel"),
            duration_ms: 210_000,
            source_url: String::from("https://www.youtube.com/watch?v=endpoint-song"),
        }],
    );
    let orchestrator = Arc::new(Mutex::new(Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        event_publisher.clone(),
        test_playback_adapter(),
        LocalSearchAdapter::new(search_runtime.clone()),
    )));
    let state = AppState {
        orchestrator: orchestrator.clone(),
        events: event_publisher,
        settings_store: test_settings_store(),
    };

    let job_id = {
        let mut locked = orchestrator.lock().await;
        locked
            .submit_search("endpoint fixture")
            .unwrap()
            .job_id
            .unwrap()
    };
    process_pending_search_jobs(&orchestrator, &search_runtime).await;

    let app = app_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/search/results/{job_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: SearchResultsResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload.job.job_id, job_id);
    assert_eq!(payload.results.len(), 1);
    assert_eq!(payload.results[0].title, "Endpoint Fixture Song");
}
