use super::*;

#[tokio::test]
async fn health_endpoint_returns_ready_status() {
    let app = app_router(test_state());
    let response = app
        .oneshot(
            Request::builder()
                .uri(nocturne_api::HEALTH_PATH)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: HealthResponse = serde_json::from_slice(&body).unwrap();
    assert!(payload.ok);
    assert!(payload.ready);
}

#[tokio::test]
async fn state_endpoint_returns_snapshot_shape() {
    let app = app_router(test_state());

    let response = app
        .oneshot(
            Request::builder()
                .uri(nocturne_api::STATE_PATH)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: StateSnapshot = serde_json::from_slice(&body).unwrap();

    assert!(payload.backend.ready);
    assert!(payload.snapshot_id.starts_with("snap_"));
    assert_eq!(payload.queue.len(), 0);
    assert_eq!(payload.audio.volume_percent, 50);
}

#[tokio::test]
async fn state_endpoint_returns_latest_event_id_header_when_available() {
    let event_log = LocalEventLog::default();
    let mut event_publisher = BroadcastEventPublisher::new(event_log, 8);
    event_publisher
        .publish(CoreEventEnvelope::new(
            String::from("evt_0001"),
            CoreEventKind::SystemError,
            String::from("2026-04-23T12:34:56Z"),
            CoreEvent::SystemError(SystemErrorEvent {
                code: String::from("test_error"),
                message: String::from("test message"),
                severity: CoreSeverity::Warning,
            }),
        ))
        .unwrap();

    let orchestrator = Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        event_publisher.clone(),
        test_playback_adapter(),
        LocalSearchAdapter::new(LocalSearchRuntime::default()),
    );
    let app = app_router(AppState {
        orchestrator: Arc::new(Mutex::new(orchestrator)),
        events: event_publisher,
        settings_store: test_settings_store(),
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri(nocturne_api::STATE_PATH)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(CURRENT_EVENT_ID_HEADER).unwrap(),
        "evt_0001"
    );
}

#[tokio::test]
async fn events_endpoint_rejects_missing_cursor() {
    let event_log = LocalEventLog::default();
    let mut event_publisher = BroadcastEventPublisher::new(event_log, 8);
    event_publisher
        .publish(CoreEventEnvelope::new(
            String::from("evt_0001"),
            CoreEventKind::SystemError,
            String::from("2026-04-23T12:34:56Z"),
            CoreEvent::SystemError(SystemErrorEvent {
                code: String::from("test_error"),
                message: String::from("test message"),
                severity: CoreSeverity::Warning,
            }),
        ))
        .unwrap();

    let orchestrator = Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        event_publisher.clone(),
        test_playback_adapter(),
        LocalSearchAdapter::new(LocalSearchRuntime::default()),
    );
    let app = app_router(AppState {
        orchestrator: Arc::new(Mutex::new(orchestrator)),
        events: event_publisher,
        settings_store: test_settings_store(),
    });

    let response = app
        .oneshot(
            Request::builder()
                .uri(nocturne_api::EVENTS_PATH)
                .header(nocturne_api::LAST_EVENT_ID_HEADER, "evt_missing")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: ProblemDetails = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload.status, StatusCode::CONFLICT.as_u16());
    assert!(payload.detail.contains("Refresh state"));
}
