use super::*;

#[tokio::test]
async fn volume_command_updates_snapshot_audio_settings() {
    let app = app_router(test_state());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/commands/playback/volume")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&PlaybackVolumeRequest { volume_percent: 65 }).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
}

#[tokio::test]
async fn playback_play_and_get_endpoint_return_current_state() {
    let event_log = LocalEventLog::default();
    let event_publisher = BroadcastEventPublisher::new(event_log, 8);
    let mut orchestrator = Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        event_publisher.clone(),
        test_playback_adapter(),
        LocalSearchAdapter::new(LocalSearchRuntime::default()),
    );
    orchestrator
        .enqueue_song(Song {
            id: String::from("song_a"),
            title: String::from("Song A"),
            channel_name: String::from("Channel A"),
            duration_ms: 1000,
            source_url: String::from("https://example.com/a"),
        })
        .unwrap();

    let app = app_router(AppState {
        orchestrator: Arc::new(Mutex::new(orchestrator)),
        events: event_publisher,
        settings_store: test_settings_store(),
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/commands/playback/play")
                .header("content-type", "application/json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/playback")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: nocturne_domain::PlaybackState = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload.state, nocturne_domain::PlaybackStatus::Loading);
    assert!(payload.current_queue_item_id.is_some());
}

#[tokio::test]
async fn playback_seek_endpoint_updates_position() {
    let event_log = LocalEventLog::default();
    let event_publisher = BroadcastEventPublisher::new(event_log, 8);
    let mut orchestrator = Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        event_publisher.clone(),
        test_playback_adapter(),
        LocalSearchAdapter::new(LocalSearchRuntime::default()),
    );
    orchestrator
        .enqueue_song(Song {
            id: String::from("song_a"),
            title: String::from("Song A"),
            channel_name: String::from("Channel A"),
            duration_ms: 1000,
            source_url: String::from("https://example.com/a"),
        })
        .unwrap();
    let playback_session_id = current_session_id(&orchestrator);
    orchestrator
        .confirm_playback_started(&playback_session_id, "queue_item_0001", 0)
        .unwrap();

    let app = app_router(AppState {
        orchestrator: Arc::new(Mutex::new(orchestrator)),
        events: event_publisher,
        settings_store: test_settings_store(),
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/commands/playback/seek")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&PlaybackSeekRequest { position_ms: 321 }).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/playback")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: nocturne_domain::PlaybackState = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload.position_ms, 321);
}

#[tokio::test]
async fn playback_pause_endpoint_rejects_unknown_fields() {
    let app = app_router(test_state());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/commands/playback/pause")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"unexpected":true}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: ProblemDetails = serde_json::from_slice(&body).unwrap();
    assert!(payload.detail.contains("unknown field"));
}
