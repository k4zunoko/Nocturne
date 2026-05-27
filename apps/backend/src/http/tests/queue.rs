use super::*;

#[tokio::test]
async fn queue_add_endpoint_accepts_song_and_queue_endpoint_returns_item() {
    let search_runtime = LocalSearchRuntime::default();
    search_runtime.set_fixture(
        "queue add fixture",
        vec![Song {
            id: String::from("youtube:queue-song"),
            title: String::from("Queue Song"),
            channel_name: String::from("Fixture Channel"),
            duration_ms: 123_000,
            source_url: String::from("https://www.youtube.com/watch?v=queue-song"),
        }],
    );

    let event_log = LocalEventLog::default();
    let event_publisher = BroadcastEventPublisher::new(event_log, 8);
    let orchestrator = Arc::new(Mutex::new(Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        event_publisher.clone(),
        test_playback_adapter(),
        LocalSearchAdapter::new(search_runtime.clone()),
    )));

    let job_id = {
        let mut locked = orchestrator.lock().await;
        locked
            .submit_search("queue add fixture")
            .unwrap()
            .job_id
            .unwrap()
    };
    process_pending_search_jobs(&orchestrator, &search_runtime).await;

    let app = app_router(AppState {
        orchestrator: orchestrator.clone(),
        events: event_publisher,
        settings_store: test_settings_store(),
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/commands/queue/add")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&QueueAddRequest {
                        song_id: String::from("youtube:queue-song"),
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: CommandAccepted = serde_json::from_slice(&body).unwrap();
    assert!(payload.ok);
    assert!(payload.queue_item_id.is_some());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/queue")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: QueueResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload.items.len(), 1);
    assert_eq!(payload.items[0].song.id, "youtube:queue-song");

    let results = orchestrator.lock().await.search_results(&job_id).unwrap();
    assert_eq!(results.results.len(), 1);
}

#[tokio::test]
async fn queue_add_endpoint_rejects_unknown_fields() {
    let app = app_router(test_state());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/commands/queue/add")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"song_id":"song_1","extra":true}"#))
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
async fn queue_remove_endpoint_rejects_current_item() {
    let event_log = LocalEventLog::default();
    let event_publisher = BroadcastEventPublisher::new(event_log, 8);
    let mut orchestrator = Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        event_publisher.clone(),
        test_playback_adapter(),
        LocalSearchAdapter::new(LocalSearchRuntime::default()),
    );
    let song = Song {
        id: String::from("youtube:current-song"),
        title: String::from("Current Song"),
        channel_name: String::from("Fixture Channel"),
        duration_ms: 123_000,
        source_url: String::from("https://www.youtube.com/watch?v=current-song"),
    };
    let queue_item_id = orchestrator
        .enqueue_song(song)
        .unwrap()
        .queue_item_id
        .unwrap();
    let playback_session_id = current_session_id(&orchestrator);
    orchestrator
        .confirm_playback_started(&playback_session_id, &queue_item_id, 0)
        .unwrap();

    let app = app_router(AppState {
        orchestrator: Arc::new(Mutex::new(orchestrator)),
        events: event_publisher,
        settings_store: test_settings_store(),
    });

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/commands/queue/remove")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&QueueRemoveRequest { queue_item_id }).unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: ProblemDetails = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        payload.r#type,
        "https://nocturne.local/problems/current_queue_item_not_removable"
    );
}

#[tokio::test]
async fn queue_move_endpoint_reorders_items() {
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
    let second_id = orchestrator
        .enqueue_song(Song {
            id: String::from("song_b"),
            title: String::from("Song B"),
            channel_name: String::from("Channel B"),
            duration_ms: 2000,
            source_url: String::from("https://example.com/b"),
        })
        .unwrap()
        .queue_item_id
        .unwrap();
    let playback_session_id = current_session_id(&orchestrator);
    orchestrator
        .confirm_playback_started(&playback_session_id, "queue_item_0001", 0)
        .unwrap();
    orchestrator.stop().unwrap();

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
                .uri("/api/v1/commands/queue/move")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&QueueMoveRequest {
                        queue_item_id: second_id,
                        to_index: 0,
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/queue")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: QueueResponse = serde_json::from_slice(&body).unwrap();
    assert_eq!(payload.items[0].song.id, "song_b");
    assert_eq!(payload.items[1].song.id, "song_a");
}

#[tokio::test]
async fn queue_clear_endpoint_clears_queue() {
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
                .uri("/api/v1/commands/queue/clear")
                .header("content-type", "application/json")
                .body(Body::from(r#"{}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/v1/queue")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let payload: QueueResponse = serde_json::from_slice(&body).unwrap();
    assert!(payload.items.is_empty());
}
