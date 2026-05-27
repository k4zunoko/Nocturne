use super::*;

#[tokio::test]
async fn youtube_import_endpoint_canonicalizes_short_urls_before_job_submission() {
    let search_runtime = LocalSearchRuntime::default();
    search_runtime.set_fixture(
        "https://www.youtube.com/watch?v=tuyZ9f6mHZk",
        vec![Song {
            id: String::from("youtube:tuyZ9f6mHZk"),
            title: String::from("traveling"),
            channel_name: String::from("Hikaru Utada"),
            duration_ms: 295_000,
            source_url: String::from("https://www.youtube.com/watch?v=tuyZ9f6mHZk"),
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
                .uri("/api/v1/commands/import/youtube")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&YoutubeImportRequest {
                        url: String::from("https://youtu.be/tuyZ9f6mHZk?list=PL1234567890"),
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);

    let locked = orchestrator.lock().await;
    assert_eq!(locked.youtube_import_jobs().len(), 1);
    assert_eq!(
        locked.youtube_import_jobs()[0].url,
        "https://www.youtube.com/watch?v=tuyZ9f6mHZk"
    );
}

#[tokio::test]
async fn youtube_import_endpoint_accepts_url_and_enqueues_song() {
    let search_runtime = LocalSearchRuntime::default();
    search_runtime.set_fixture(
        "https://www.youtube.com/watch?v=tuyZ9f6mHZk",
        vec![Song {
            id: String::from("youtube:tuyZ9f6mHZk"),
            title: String::from("traveling"),
            channel_name: String::from("Hikaru Utada"),
            duration_ms: 295_000,
            source_url: String::from("https://www.youtube.com/watch?v=tuyZ9f6mHZk"),
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
                .uri("/api/v1/commands/import/youtube")
                .header("content-type", "application/json")
                .body(Body::from(
                    serde_json::to_vec(&YoutubeImportRequest {
                        url: String::from("https://www.youtube.com/watch?v=tuyZ9f6mHZk"),
                    })
                    .unwrap(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::ACCEPTED);
    process_pending_youtube_import_jobs(&orchestrator, &search_runtime).await;

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
    assert_eq!(
        payload.items[0].song.source_url,
        "https://www.youtube.com/watch?v=tuyZ9f6mHZk"
    );
}

#[tokio::test]
async fn youtube_import_endpoint_rejects_unknown_fields() {
    let app = app_router(test_state());

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/commands/import/youtube")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"url":"https://youtu.be/tuyZ9f6mHZk","extra":true}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
}
