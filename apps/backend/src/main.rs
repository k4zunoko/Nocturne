use std::collections::HashSet;
use std::convert::Infallible;
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;

use async_stream::stream;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json as ExtractJson, Path, State};
use axum::http::HeaderMap;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use nocturne_api::{
    ApiVersion, CommandAccepted, EmptyPayload, HealthResponse, PlaybackRepeatRequest,
    PlaybackSeekRequest, PlaybackVolumeRequest, ProblemDetails, QueueAddRequest, QueueMoveRequest,
    QueueRemoveRequest, QueueResponse, SearchCommandRequest, SearchResultsResponse,
    YoutubeImportRequest,
};
use nocturne_core::{
    CoreError, NocturneCore, Orchestrator, SystemErrorSeverity as CoreSystemErrorSeverity,
};
use nocturne_domain::AudioSettings;
use nocturne_infrastructure::{
    BroadcastEventPublisher, InfrastructureProfile, LocalAudioSettingsStore, LocalClock,
    LocalEventLog, LocalIdGenerator, LocalPlaybackAdapter, LocalSearchAdapter, LocalSearchRuntime,
    LocalYtDlpManager, LocalYtDlpSettingsStore, SharedPlaybackState,
    canonicalize_supported_youtube_url,
};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

mod mapping;
#[cfg(test)]
mod test_support;
mod workers;

use crate::mapping::{
    command_accepted_response, map_core_error, map_cursor_error, map_json_rejection,
    map_search_job_summary, map_sse_event, map_state_snapshot, map_youtube_import_request_error,
    state_headers,
};

use crate::workers::{
    spawn_playback_event_bridge, spawn_search_worker, spawn_yt_dlp_update_worker,
};

type BackendOrchestrator = Orchestrator<
    LocalClock,
    LocalIdGenerator,
    BroadcastEventPublisher,
    LocalPlaybackAdapter,
    LocalSearchAdapter,
>;

#[derive(Clone)]
pub(crate) struct AppState {
    orchestrator: Arc<Mutex<BackendOrchestrator>>,
    events: BroadcastEventPublisher,
    settings_store: Arc<Mutex<LocalAudioSettingsStore>>,
}

pub(crate) mod http {
    pub(crate) use super::AppState;
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let core = NocturneCore::new();
    let event_log = LocalEventLog::default();
    let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 128);
    let playback_state = SharedPlaybackState::default();
    let (playback_event_tx, playback_event_rx) = mpsc::unbounded_channel();
    let yt_dlp_manager =
        LocalYtDlpManager::new(reqwest::Client::new(), LocalYtDlpSettingsStore::new()?);
    yt_dlp_manager.prepare()?;
    let search_runtime = LocalSearchRuntime::with_yt_dlp(yt_dlp_manager.clone());
    let settings_store = Arc::new(Mutex::new(LocalAudioSettingsStore::new()?));
    let initial_audio_settings = {
        let store = settings_store.lock().await;
        match store.load() {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("failed to load audio settings, falling back to defaults: {error}");
                AudioSettings::default()
            }
        }
    };

    let mut orchestrator = Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        event_publisher.clone(),
        LocalPlaybackAdapter::with_yt_dlp(
            playback_state.clone(),
            playback_event_tx.clone(),
            yt_dlp_manager.clone(),
        ),
        LocalSearchAdapter::new(search_runtime.clone()),
    );
    orchestrator.set_backend_version(Some(env!("CARGO_PKG_VERSION")));
    orchestrator.set_yt_dlp_version(yt_dlp_manager.current_version());
    orchestrator.hydrate_audio_settings(initial_audio_settings)?;

    let snapshot = orchestrator.snapshot();
    let orchestrator = Arc::new(Mutex::new(orchestrator));
    spawn_yt_dlp_update_worker(orchestrator.clone(), yt_dlp_manager.clone());
    spawn_search_worker(orchestrator.clone(), search_runtime.clone());
    spawn_playback_event_bridge(orchestrator.clone(), playback_event_rx);
    let state = AppState {
        orchestrator,
        events: event_publisher.clone(),
        settings_store,
    };
    let app = app_router(state.clone());

    let bind_addr = validate_bind_addr(backend_bind_addr()?)?;
    let listener = TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;

    println!(
        "Nocturne backend ready (api {}, profile {}, infra {}, queue {}, search_jobs {}, events {}, listening http://{})",
        ApiVersion::V1,
        core.workspace_profile(),
        InfrastructureProfile::Local,
        snapshot.queue.len(),
        snapshot.search_jobs.len(),
        event_log.len(),
        local_addr,
    );

    axum::serve(listener, app).await?;
    Ok(())
}

fn report_playback_command_error(orchestrator: &mut BackendOrchestrator, error: &CoreError) {
    let CoreError::Port { code, .. } = error else {
        return;
    };

    if let Err(report_error) = orchestrator.emit_system_error(
        code.clone(),
        crate::workers::user_message_for_playback_failure(code),
        CoreSystemErrorSeverity::Error,
    ) {
        eprintln!("failed to emit playback system error for {code}: {report_error}");
    }
}

fn app_router(state: AppState) -> Router {
    Router::new()
        .route(nocturne_api::HEALTH_PATH, get(health_handler))
        .route(nocturne_api::STATE_PATH, get(state_handler))
        .route(nocturne_api::EVENTS_PATH, get(events_handler))
        .route("/api/v1/queue", get(queue_handler))
        .route("/api/v1/playback", get(playback_handler))
        .route("/api/v1/commands/search", post(search_command_handler))
        .route(
            "/api/v1/commands/import/youtube",
            post(youtube_import_command_handler),
        )
        .route("/api/v1/commands/queue/add", post(queue_add_handler))
        .route("/api/v1/commands/queue/remove", post(queue_remove_handler))
        .route("/api/v1/commands/queue/move", post(queue_move_handler))
        .route("/api/v1/commands/queue/clear", post(queue_clear_handler))
        .route(
            "/api/v1/commands/playback/play",
            post(playback_play_handler),
        )
        .route(
            "/api/v1/commands/playback/pause",
            post(playback_pause_handler),
        )
        .route(
            "/api/v1/commands/playback/play-pause",
            post(playback_play_pause_handler),
        )
        .route(
            "/api/v1/commands/playback/stop",
            post(playback_stop_handler),
        )
        .route(
            "/api/v1/commands/playback/next",
            post(playback_next_handler),
        )
        .route(
            "/api/v1/commands/playback/restart_current",
            post(playback_restart_current_handler),
        )
        .route(
            "/api/v1/commands/playback/repeat",
            post(playback_repeat_handler),
        )
        .route(
            "/api/v1/commands/playback/seek",
            post(playback_seek_handler),
        )
        .route(
            "/api/v1/commands/playback/volume",
            post(playback_volume_handler),
        )
        .route(
            "/api/v1/search/results/{job_id}",
            get(search_results_handler),
        )
        .with_state(state)
}

fn backend_bind_addr() -> Result<SocketAddr, Box<dyn std::error::Error>> {
    let addr: SocketAddr = env::var("NOCTURNE_BACKEND_ADDR")
        .unwrap_or_else(|_| String::from("127.0.0.1:0"))
        .parse()?;

    if !addr.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("NOCTURNE_BACKEND_ADDR must use a loopback address, got {addr}"),
        )
        .into());
    }

    Ok(addr)
}

async fn health_handler(State(state): State<AppState>) -> Json<HealthResponse> {
    let orchestrator = state.orchestrator.lock().await;
    let backend = orchestrator.backend_state();

    Json(HealthResponse {
        ok: true,
        ready: backend.ready,
        version: backend.version.clone(),
        yt_dlp_version: backend.yt_dlp_version.clone(),
    })
}

async fn state_handler(State(state): State<AppState>) -> impl axum::response::IntoResponse {
    let (snapshot, last_event_id) = {
        let mut orchestrator = state.orchestrator.lock().await;
        let snapshot = map_state_snapshot(orchestrator.snapshot());
        let last_event_id = state.events.log().latest_event_id();
        (snapshot, last_event_id)
    };

    (state_headers(last_event_id), Json(snapshot))
}

async fn events_handler(State(state): State<AppState>, headers: HeaderMap) -> impl IntoResponse {
    let last_event_id = headers
        .get(nocturne_api::LAST_EVENT_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);

    let (mut receiver, replay) = state
        .events
        .subscribe_after(last_event_id.as_deref())
        .map_err(map_cursor_error)?;
    let mut replayed_ids = replay
        .iter()
        .map(|event| event.event_id.clone())
        .collect::<HashSet<_>>();

    let stream = stream! {
        for event in replay {
            yield Ok::<Event, Infallible>(map_sse_event(event));
        }

        loop {
            let event = match receiver.recv().await {
                Ok(event) if replayed_ids.remove(&event.event_id) => continue,
                Ok(event) => map_sse_event(event),
                Err(broadcast::error::RecvError::Lagged(_)) => break,
                Err(broadcast::error::RecvError::Closed) => break,
            };

            yield Ok::<Event, Infallible>(event);
        }
    };

    Ok::<_, (StatusCode, Json<ProblemDetails>)>(Sse::new(stream).keep_alive(KeepAlive::default()))
}

async fn search_command_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<SearchCommandRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(request) =
        request.map_err(|error| map_json_rejection(error, "/api/v1/commands/search"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator
        .submit_search(request.query)
        .map_err(|error| map_core_error(error, "/api/v1/commands/search"))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(CommandAccepted {
            ok: true,
            command_id: receipt.command_id,
            accepted_at: Some(receipt.accepted_at),
            job_id: receipt.job_id,
            queue_item_id: receipt.queue_item_id,
        }),
    ))
}

async fn youtube_import_command_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<YoutubeImportRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(request) =
        request.map_err(|error| map_json_rejection(error, "/api/v1/commands/import/youtube"))?;
    let canonical_url = canonicalize_supported_youtube_url(&request.url).map_err(|error| {
        map_youtube_import_request_error(error, "/api/v1/commands/import/youtube")
    })?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator
        .submit_youtube_import(canonical_url)
        .map_err(|error| map_core_error(error, "/api/v1/commands/import/youtube"))?;

    Ok(command_accepted_response(receipt))
}

async fn queue_handler(State(state): State<AppState>) -> Json<QueueResponse> {
    let orchestrator = state.orchestrator.lock().await;
    Json(QueueResponse {
        items: orchestrator.queue().to_vec(),
    })
}

async fn playback_handler(State(state): State<AppState>) -> Json<nocturne_domain::PlaybackState> {
    let orchestrator = state.orchestrator.lock().await;
    Json(orchestrator.state().playback().clone())
}

async fn queue_add_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<QueueAddRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(request) =
        request.map_err(|error| map_json_rejection(error, "/api/v1/commands/queue/add"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator
        .enqueue_song_by_id(&request.song_id)
        .map_err(|error| map_core_error(error, "/api/v1/commands/queue/add"))?;

    Ok(command_accepted_response(receipt))
}

async fn queue_remove_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<QueueRemoveRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(request) =
        request.map_err(|error| map_json_rejection(error, "/api/v1/commands/queue/remove"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator
        .remove_queue_item(&request.queue_item_id)
        .map_err(|error| map_core_error(error, "/api/v1/commands/queue/remove"))?;

    Ok(command_accepted_response(receipt))
}

async fn queue_move_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<QueueMoveRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(request) =
        request.map_err(|error| map_json_rejection(error, "/api/v1/commands/queue/move"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator
        .move_queue_item(&request.queue_item_id, request.to_index)
        .map_err(|error| map_core_error(error, "/api/v1/commands/queue/move"))?;

    Ok(command_accepted_response(receipt))
}

async fn queue_clear_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<EmptyPayload>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(_) =
        request.map_err(|error| map_json_rejection(error, "/api/v1/commands/queue/clear"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator
        .clear_queue()
        .map_err(|error| map_core_error(error, "/api/v1/commands/queue/clear"))?;

    Ok(command_accepted_response(receipt))
}

async fn playback_play_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<EmptyPayload>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(_) =
        request.map_err(|error| map_json_rejection(error, "/api/v1/commands/playback/play"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator.play().map_err(|error| {
        report_playback_command_error(&mut orchestrator, &error);
        map_core_error(error, "/api/v1/commands/playback/play")
    })?;

    Ok(command_accepted_response(receipt))
}

async fn playback_pause_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<EmptyPayload>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(_) =
        request.map_err(|error| map_json_rejection(error, "/api/v1/commands/playback/pause"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator.pause().map_err(|error| {
        report_playback_command_error(&mut orchestrator, &error);
        map_core_error(error, "/api/v1/commands/playback/pause")
    })?;

    Ok(command_accepted_response(receipt))
}

async fn playback_play_pause_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<EmptyPayload>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(_) = request
        .map_err(|error| map_json_rejection(error, "/api/v1/commands/playback/play-pause"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator.play_pause().map_err(|error| {
        report_playback_command_error(&mut orchestrator, &error);
        map_core_error(error, "/api/v1/commands/playback/play-pause")
    })?;

    Ok(command_accepted_response(receipt))
}

async fn playback_stop_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<EmptyPayload>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(_) =
        request.map_err(|error| map_json_rejection(error, "/api/v1/commands/playback/stop"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator.stop().map_err(|error| {
        report_playback_command_error(&mut orchestrator, &error);
        map_core_error(error, "/api/v1/commands/playback/stop")
    })?;

    Ok(command_accepted_response(receipt))
}

async fn playback_next_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<EmptyPayload>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(_) =
        request.map_err(|error| map_json_rejection(error, "/api/v1/commands/playback/next"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator.next().map_err(|error| {
        report_playback_command_error(&mut orchestrator, &error);
        map_core_error(error, "/api/v1/commands/playback/next")
    })?;

    Ok(command_accepted_response(receipt))
}

async fn playback_restart_current_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<EmptyPayload>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(_) = request
        .map_err(|error| map_json_rejection(error, "/api/v1/commands/playback/restart_current"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator.restart_current().map_err(|error| {
        report_playback_command_error(&mut orchestrator, &error);
        map_core_error(error, "/api/v1/commands/playback/restart_current")
    })?;

    Ok(command_accepted_response(receipt))
}

async fn playback_repeat_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<PlaybackRepeatRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(request) =
        request.map_err(|error| map_json_rejection(error, "/api/v1/commands/playback/repeat"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator
        .set_repeat_mode(request.repeat_mode)
        .map_err(|error| map_core_error(error, "/api/v1/commands/playback/repeat"))?;

    Ok(command_accepted_response(receipt))
}

async fn playback_seek_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<PlaybackSeekRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(request) =
        request.map_err(|error| map_json_rejection(error, "/api/v1/commands/playback/seek"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator.seek(request.position_ms).map_err(|error| {
        report_playback_command_error(&mut orchestrator, &error);
        map_core_error(error, "/api/v1/commands/playback/seek")
    })?;

    Ok(command_accepted_response(receipt))
}

async fn playback_volume_handler(
    State(state): State<AppState>,
    request: Result<ExtractJson<PlaybackVolumeRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<CommandAccepted>), (StatusCode, Json<ProblemDetails>)> {
    let ExtractJson(request) =
        request.map_err(|error| map_json_rejection(error, "/api/v1/commands/playback/volume"))?;
    let mut orchestrator = state.orchestrator.lock().await;
    let receipt = orchestrator
        .set_volume(request.volume_percent)
        .map_err(|error| {
            report_playback_command_error(&mut orchestrator, &error);
            map_core_error(error, "/api/v1/commands/playback/volume")
        })?;
    let settings = *orchestrator.state().audio();
    drop(orchestrator);

    let save_result = {
        let store = state.settings_store.lock().await;
        store.save(&settings)
    };

    if let Err(error) = save_result {
        let mut orchestrator = state.orchestrator.lock().await;
        if let Err(report_error) = orchestrator.emit_system_error(
            "audio_settings_persist_failed",
            format!("Volume changed for this session, but saving it failed: {error}"),
            CoreSystemErrorSeverity::Warning,
        ) {
            eprintln!("failed to emit audio settings persistence warning: {report_error}");
        }
    }

    Ok(command_accepted_response(receipt))
}

async fn search_results_handler(
    State(state): State<AppState>,
    Path(job_id): Path<String>,
) -> Result<Json<SearchResultsResponse>, (StatusCode, Json<ProblemDetails>)> {
    let orchestrator = state.orchestrator.lock().await;
    let record = orchestrator.search_results(&job_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(ProblemDetails {
                r#type: String::from("https://nocturne.local/problems/search-results-not-found"),
                title: String::from("search results not found"),
                status: StatusCode::NOT_FOUND.as_u16(),
                detail: format!("No completed search results were found for job '{job_id}'."),
                instance: Some(format!("/api/v1/search/results/{job_id}")),
                errors: Vec::new(),
            }),
        )
    })?;

    Ok(Json(SearchResultsResponse {
        job: map_search_job_summary(record.job),
        results: record.results,
    }))
}

fn validate_bind_addr(addr: SocketAddr) -> Result<SocketAddr, Box<dyn std::error::Error>> {
    if addr.ip().is_loopback() {
        Ok(addr)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("NOCTURNE_BACKEND_ADDR must stay on loopback, got {addr}"),
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use nocturne_api::{
        PlaybackSeekRequest, PlaybackVolumeRequest, QueueAddRequest, QueueMoveRequest,
        QueueRemoveRequest, QueueResponse, StateSnapshot, YoutubeImportRequest,
    };
    use nocturne_core::{
        CoreEvent, CoreEventEnvelope, CoreEventKind, EventPublisherPort, SystemErrorEvent,
        SystemErrorSeverity as CoreSeverity,
    };
    use nocturne_domain::Song;
    use nocturne_infrastructure::PlaybackWorkerEvent;
    use tower::ServiceExt;

    use crate::mapping::CURRENT_EVENT_ID_HEADER;
    use crate::test_support::{
        current_session_id, test_playback_adapter, test_settings_store, test_state,
    };
    use crate::workers::{
        process_pending_search_jobs, process_pending_youtube_import_jobs,
        spawn_playback_event_bridge,
    };

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

    #[test]
    fn validate_bind_addr_rejects_non_loopback_addresses() {
        let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        assert!(validate_bind_addr(addr).is_err());

        let loopback_addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(validate_bind_addr(loopback_addr).unwrap(), loopback_addr);
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
                    .body(Body::from(r#"{"query":"utada traveling""#))
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
    async fn worker_emits_system_error_for_missing_yt_dlp() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
        let search_runtime = LocalSearchRuntime::default();
        search_runtime.set_failure(
            "missing provider",
            "yt_dlp_missing",
            "hidden internal detail",
        );
        let orchestrator = Arc::new(Mutex::new(Orchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher.clone(),
            test_playback_adapter(),
            LocalSearchAdapter::new(search_runtime.clone()),
        )));

        {
            let mut locked = orchestrator.lock().await;
            locked.submit_search("missing provider").unwrap();
        }
        process_pending_search_jobs(&orchestrator, &search_runtime).await;

        let snapshot = event_log.snapshot();
        let has_system_error = snapshot
            .iter()
            .any(|event| matches!(event.data, CoreEvent::SystemError(_)));
        assert!(has_system_error);
    }

    #[test]
    fn playback_port_errors_emit_user_visible_system_errors() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
        let mut orchestrator = Orchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher,
            test_playback_adapter(),
            LocalSearchAdapter::new(LocalSearchRuntime::default()),
        );

        report_playback_command_error(
            &mut orchestrator,
            &CoreError::Port {
                code: String::from("audio_output_unavailable"),
                message: String::from("hidden detail"),
            },
        );

        let snapshot = event_log.snapshot();
        let system_error = snapshot
            .iter()
            .find_map(|event| match &event.data {
                CoreEvent::SystemError(system_error) => Some(system_error),
                _ => None,
            })
            .expect("expected a system.error event");

        assert_eq!(system_error.code, "audio_output_unavailable");
        assert_eq!(
            system_error.message,
            "Audio output is unavailable on the backend."
        );
        assert_eq!(system_error.severity, CoreSeverity::Error);
    }

    #[tokio::test]
    async fn interrupted_playback_event_stops_playback_and_emits_system_error() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
        let mut orchestrator = Orchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher,
            test_playback_adapter(),
            LocalSearchAdapter::new(LocalSearchRuntime::default()),
        );
        orchestrator
            .hydrate_audio_settings(AudioSettings::default())
            .unwrap();

        let receipt = orchestrator
            .enqueue_song(Song {
                id: String::from("youtube:interrupted-song"),
                title: String::from("Interrupted Song"),
                channel_name: String::from("Fixture Channel"),
                duration_ms: 123_000,
                source_url: String::from("https://example.com/interrupted-song"),
            })
            .unwrap();
        let queue_item_id = receipt.queue_item_id.expect("expected queued item id");
        let playback_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&playback_session_id, &queue_item_id, 321)
            .unwrap();

        let orchestrator = Arc::new(Mutex::new(orchestrator));
        let (playback_event_tx, playback_event_rx) = mpsc::unbounded_channel();
        spawn_playback_event_bridge(orchestrator.clone(), playback_event_rx);

        playback_event_tx
            .send(PlaybackWorkerEvent::Interrupted {
                playback_session_id,
                queue_item_id,
                code: String::from("audio_output_changed"),
                message: String::from("recovery failed after device loss"),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let locked = orchestrator.lock().await;
        assert_eq!(
            locked.state().playback().state,
            nocturne_domain::PlaybackStatus::Stopped
        );
        assert!(locked.state().playback().current_queue_item_id.is_none());
        drop(locked);

        let snapshot = event_log.snapshot();
        let system_error = snapshot
            .iter()
            .rev()
            .find_map(|event| match &event.data {
                CoreEvent::SystemError(system_error) => Some(system_error),
                _ => None,
            })
            .expect("expected a system.error event");

        assert_eq!(system_error.code, "audio_output_changed");
        assert_eq!(
            system_error.message,
            "Audio output changed. Playback stopped, and the next play will use the current default device."
        );
        assert_eq!(system_error.severity, CoreSeverity::Error);
    }

    #[tokio::test]
    async fn interrupted_stream_error_emits_generic_audio_output_message() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
        let mut orchestrator = Orchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher,
            test_playback_adapter(),
            LocalSearchAdapter::new(LocalSearchRuntime::default()),
        );
        orchestrator
            .hydrate_audio_settings(AudioSettings::default())
            .unwrap();

        let receipt = orchestrator
            .enqueue_song(Song {
                id: String::from("youtube:interrupted-stream-song"),
                title: String::from("Interrupted Stream Song"),
                channel_name: String::from("Fixture Channel"),
                duration_ms: 123_000,
                source_url: String::from("https://example.com/interrupted-stream-song"),
            })
            .unwrap();
        let queue_item_id = receipt.queue_item_id.expect("expected queued item id");
        let playback_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&playback_session_id, &queue_item_id, 321)
            .unwrap();

        let orchestrator = Arc::new(Mutex::new(orchestrator));
        let (playback_event_tx, playback_event_rx) = mpsc::unbounded_channel();
        spawn_playback_event_bridge(orchestrator.clone(), playback_event_rx);

        playback_event_tx
            .send(PlaybackWorkerEvent::Interrupted {
                playback_session_id,
                queue_item_id,
                code: String::from("audio_output_stream_lost"),
                message: String::from("stream error: device disappeared"),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let snapshot = event_log.snapshot();
        let system_error = snapshot
            .iter()
            .rev()
            .find_map(|event| match &event.data {
                CoreEvent::SystemError(system_error) => Some(system_error),
                _ => None,
            })
            .expect("expected a system.error event");

        assert_eq!(system_error.code, "audio_output_stream_lost");
        assert_eq!(
            system_error.message,
            "Playback stopped because the backend lost its audio output stream. Try playing again."
        );
        assert_eq!(system_error.severity, CoreSeverity::Error);
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
}
