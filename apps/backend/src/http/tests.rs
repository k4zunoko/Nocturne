use super::{AppState, app_router};
use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::http::{Request, StatusCode};
use nocturne_api::{
    CommandAccepted, HealthResponse, PlaybackSeekRequest, PlaybackVolumeRequest, ProblemDetails,
    QueueAddRequest, QueueMoveRequest, QueueRemoveRequest, QueueResponse, SearchResultsResponse,
    StateSnapshot, YoutubeImportRequest,
};
use nocturne_core::{
    CoreEvent, CoreEventEnvelope, CoreEventKind, EventPublisherPort, Orchestrator,
    SystemErrorEvent, SystemErrorSeverity as CoreSeverity,
};
use nocturne_domain::Song;
use nocturne_infrastructure::{
    BroadcastEventPublisher, LocalClock, LocalEventLog, LocalIdGenerator, LocalSearchAdapter,
    LocalSearchRuntime,
};
use tokio::sync::Mutex;
use tower::ServiceExt;

use crate::mapping::CURRENT_EVENT_ID_HEADER;
use crate::test_support::{
    current_session_id, test_playback_adapter, test_settings_store, test_state,
};
use crate::workers::{process_pending_search_jobs, process_pending_youtube_import_jobs};

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
