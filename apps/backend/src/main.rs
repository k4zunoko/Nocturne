use std::collections::HashSet;
use std::convert::Infallible;
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json as ExtractJson, Path, State};
use axum::http::StatusCode;
use axum::http::HeaderMap;
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
    LocalEventLog, LocalIdGenerator, LocalPlaybackAdapter, LocalSearchAdapter,
    LocalSearchRuntime, LocalYtDlpManager, LocalYtDlpSettingsStore, PlaybackWorkerEvent,
    SharedPlaybackState, YoutubeImportResolution, canonicalize_supported_youtube_url,
};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::sync::broadcast;
use tokio::sync::mpsc;

mod mapping;

use crate::mapping::{
    command_accepted_response, map_core_error, map_cursor_error, map_json_rejection,
    map_search_job_summary, map_sse_event, map_state_snapshot, map_youtube_import_request_error,
    state_headers,
};

#[cfg(test)]
mod test_support {
    pub(crate) fn problem_detail_instance(path: &str) -> &str {
        path
    }
}

type BackendOrchestrator = Orchestrator<
    LocalClock,
    LocalIdGenerator,
    BroadcastEventPublisher,
    LocalPlaybackAdapter,
    LocalSearchAdapter,
>;

#[derive(Clone)]
struct AppState {
    orchestrator: Arc<Mutex<BackendOrchestrator>>,
    events: BroadcastEventPublisher,
    settings_store: Arc<Mutex<LocalAudioSettingsStore>>,
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

fn spawn_yt_dlp_update_worker(
    orchestrator: Arc<Mutex<BackendOrchestrator>>,
    yt_dlp_manager: LocalYtDlpManager,
) {
    tokio::spawn(async move {
        if let Err(error) = yt_dlp_manager.check_for_updates_if_due().await {
            eprintln!("failed to check for yt-dlp updates: {error}");
            return;
        }

        match yt_dlp_manager.apply_pending_update() {
            Ok(true) => {
                let version = yt_dlp_manager.current_version();
                let mut orchestrator = orchestrator.lock().await;
                if let Err(error) = orchestrator.update_yt_dlp_version(version) {
                    eprintln!("failed to publish yt-dlp version update: {error}");
                }
            }
            Ok(false) => {}
            Err(error) => {
                eprintln!("failed to promote yt-dlp update: {error}");
            }
        }
    });
}

fn spawn_search_worker(
    orchestrator: Arc<Mutex<BackendOrchestrator>>,
    search_runtime: LocalSearchRuntime,
) {
    tokio::spawn(async move {
        loop {
            let processed = process_pending_search_jobs(&orchestrator, &search_runtime).await
                + process_pending_youtube_import_jobs(&orchestrator, &search_runtime).await;
            let delay = if processed == 0 {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(10)
            };
            tokio::time::sleep(delay).await;
        }
    });
}

fn spawn_playback_event_bridge(
    orchestrator: Arc<Mutex<BackendOrchestrator>>,
    mut playback_events: mpsc::UnboundedReceiver<PlaybackWorkerEvent>,
) {
    tokio::spawn(async move {
        loop {
            match playback_events.recv().await {
                Some(PlaybackWorkerEvent::Started {
                    playback_session_id,
                    queue_item_id,
                    position_ms,
                }) => {
                    let mut orchestrator = orchestrator.lock().await;
                    match orchestrator.confirm_playback_started(
                        &playback_session_id,
                        &queue_item_id,
                        position_ms,
                    ) {
                        Ok(true) => {}
                        Ok(false) => {}
                        Err(error) => {
                            eprintln!(
                                "failed to confirm playback start for {}: {error}",
                                queue_item_id
                            );
                        }
                    }
                }
                Some(PlaybackWorkerEvent::StartFailed {
                    playback_session_id,
                    queue_item_id,
                    code,
                    message,
                }) => {
                    let mut orchestrator = orchestrator.lock().await;
                    match orchestrator
                        .report_playback_start_failed(&playback_session_id, &queue_item_id)
                    {
                        Ok(true) => {
                            if let Err(error) = orchestrator.emit_system_error(
                                code.clone(),
                                user_message_for_playback_failure(&code),
                                CoreSystemErrorSeverity::Error,
                            ) {
                                eprintln!(
                                    "failed to emit async playback start error for {}: {error}",
                                    queue_item_id
                                );
                            } else {
                                eprintln!(
                                    "playback start failed for {}: {}",
                                    queue_item_id, message
                                );
                            }
                        }
                        Ok(false) => {}
                        Err(error) => {
                            eprintln!(
                                "failed to mark playback start failure for {}: {error}",
                                queue_item_id
                            );
                        }
                    }
                }
                Some(PlaybackWorkerEvent::Interrupted {
                    playback_session_id,
                    queue_item_id,
                    code,
                    message,
                }) => {
                    let mut orchestrator = orchestrator.lock().await;
                    if orchestrator.state().playback().playback_session_id.as_deref()
                        != Some(playback_session_id.as_str())
                    {
                        continue;
                    }

                    if let Err(error) = orchestrator.stop() {
                        eprintln!(
                            "failed to stop interrupted playback for {}: {error}",
                            queue_item_id
                        );
                    }

                    if let Err(error) = orchestrator.emit_system_error(
                        code.clone(),
                        user_message_for_playback_interruption(&code),
                        CoreSystemErrorSeverity::Error,
                    ) {
                        eprintln!(
                            "failed to emit interrupted playback error for {}: {error}",
                            queue_item_id
                        );
                    } else {
                        eprintln!(
                            "playback interrupted for {}: {}",
                            queue_item_id, message
                        );
                    }
                }
                Some(PlaybackWorkerEvent::Progress {
                    playback_session_id,
                    position_ms,
                    paused,
                }) => {
                    if paused {
                        continue;
                    }
                    let mut orchestrator = orchestrator.lock().await;
                    if let Err(error) =
                        orchestrator.report_playback_progress(&playback_session_id, position_ms)
                    {
                        eprintln!(
                            "failed to publish playback progress {} for session {}: {error}",
                            position_ms, playback_session_id
                        );
                    }
                }
                Some(PlaybackWorkerEvent::Ended {
                    playback_session_id,
                    queue_item_id,
                    position_ms,
                }) => {
                    let mut orchestrator = orchestrator.lock().await;
                    if let Err(error) =
                        orchestrator.finish_current_track(&playback_session_id, position_ms)
                    {
                        eprintln!(
                            "failed to advance playback after natural track end {}: {error}",
                            queue_item_id
                        );
                    }
                }
                None => break,
            }
        }
    });
}

async fn process_pending_search_jobs(
    orchestrator: &Arc<Mutex<BackendOrchestrator>>,
    search_runtime: &LocalSearchRuntime,
) -> usize {
    let pending_jobs = search_runtime.drain_pending();

    for job in &pending_jobs {
        let runtime = search_runtime.clone();
        let query = job.query.clone();
        let result = tokio::task::spawn_blocking(move || runtime.resolve(&query)).await;

        let mut orchestrator = orchestrator.lock().await;
        match result {
            Ok(Ok(results)) => {
                if let Err(error) = orchestrator.complete_search(&job.job_id, results) {
                    eprintln!("failed to complete search job {}: {error}", job.job_id);
                }
            }
            Ok(Err(failure)) => {
                let user_message = user_message_for_search_failure(&failure.code);
                let should_emit_system_error = matches!(
                    failure.code.as_str(),
                    "yt_dlp_missing" | "yt_dlp_spawn_failed" | "yt_dlp_timeout"
                );

                if let Err(error) =
                    orchestrator.fail_search(&job.job_id, &failure.code, user_message)
                {
                    eprintln!("failed to fail search job {}: {error}", job.job_id);
                }

                if should_emit_system_error
                    && let Err(error) = orchestrator.emit_system_error(
                        &failure.code,
                        user_message,
                        CoreSystemErrorSeverity::Error,
                    )
                {
                    eprintln!("failed to emit system error for {}: {error}", job.job_id);
                }
            }
            Err(error) => {
                if let Err(report_error) = orchestrator.fail_search(
                    &job.job_id,
                    "search_worker_join_failed",
                    error.to_string(),
                ) {
                    eprintln!(
                        "failed to report search worker join error for {}: {report_error}",
                        job.job_id
                    );
                }
            }
        }
    }

    pending_jobs.len()
}

async fn process_pending_youtube_import_jobs(
    orchestrator: &Arc<Mutex<BackendOrchestrator>>,
    search_runtime: &LocalSearchRuntime,
) -> usize {
    let pending_jobs = search_runtime.drain_pending_youtube_imports();

    for job in &pending_jobs {
        let runtime = search_runtime.clone();
        let url = job.url.clone();
        let result = tokio::task::spawn_blocking(move || runtime.resolve_youtube_url(&url)).await;

        let mut orchestrator = orchestrator.lock().await;
        match result {
            Ok(Ok(YoutubeImportResolution {
                songs,
                total_count,
                failed_count,
                ..
            })) => {
                if let Err(error) =
                    orchestrator.complete_youtube_import(&job.job_id, songs, total_count, failed_count)
                {
                    let code = core_error_code(&error).to_owned();
                    let user_message = user_message_for_youtube_import_completion_error(&error);
                    let should_emit_system_error = matches!(
                        code.as_str(),
                        "yt_dlp_missing"
                            | "yt_dlp_spawn_failed"
                            | "yt_dlp_timeout"
                            | "audio_output_unavailable"
                            | "audio_source_resolve_failed"
                            | "playback_stream_open_failed"
                            | "playback_decode_failed"
                            | "playback_worker_unavailable"
                    );

                    if let Err(report_error) =
                        orchestrator.fail_youtube_import(&job.job_id, &code, user_message)
                    {
                        eprintln!(
                            "failed to convert youtube import completion error for {}: {report_error}",
                            job.job_id
                        );
                    }

                    if should_emit_system_error
                        && let Err(report_error) = orchestrator.emit_system_error(
                            &code,
                            user_message,
                            CoreSystemErrorSeverity::Error,
                        )
                    {
                        eprintln!(
                            "failed to emit system error for youtube import {}: {report_error}",
                            job.job_id
                        );
                    }
                }
            }
            Ok(Err(failure)) => {
                let user_message = user_message_for_youtube_import_failure(&failure.code);
                let should_emit_system_error = matches!(
                    failure.code.as_str(),
                    "yt_dlp_missing" | "yt_dlp_spawn_failed" | "yt_dlp_timeout"
                );

                if let Err(error) =
                    orchestrator.fail_youtube_import(&job.job_id, &failure.code, user_message)
                {
                    eprintln!("failed to fail youtube import job {}: {error}", job.job_id);
                }

                if should_emit_system_error
                    && let Err(error) = orchestrator.emit_system_error(
                        &failure.code,
                        user_message,
                        CoreSystemErrorSeverity::Error,
                    )
                {
                    eprintln!(
                        "failed to emit system error for youtube import {}: {error}",
                        job.job_id
                    );
                }
            }
            Err(error) => {
                if let Err(report_error) = orchestrator.fail_youtube_import(
                    &job.job_id,
                    "youtube_import_worker_join_failed",
                    error.to_string(),
                ) {
                    eprintln!(
                        "failed to report youtube import join error for {}: {report_error}",
                        job.job_id
                    );
                }
            }
        }
    }

    pending_jobs.len()
}

fn user_message_for_search_failure(code: &str) -> &'static str {
    match code {
        "yt_dlp_missing" => "yt-dlp is not installed for this backend.",
        "yt_dlp_spawn_failed" => "The backend could not start the search provider.",
        "yt_dlp_timeout" => "The search provider timed out.",
        "yt_dlp_invalid_json" | "yt_dlp_invalid_response" => {
            "The search provider returned unreadable results."
        }
        "yt_dlp_failed" => "Search provider failed to return results.",
        _ => "Search failed on the backend.",
    }
}

fn user_message_for_youtube_import_failure(code: &str) -> &'static str {
    match code {
        "youtube_url_invalid" => "Input is not a valid YouTube URL.",
        "youtube_url_unsupported" => "This YouTube URL format is not supported yet.",
        "yt_dlp_missing" => "yt-dlp is not installed for this backend.",
        "yt_dlp_spawn_failed" => "The backend could not start YouTube URL resolution.",
        "yt_dlp_timeout" => "YouTube URL resolution timed out.",
        "yt_dlp_invalid_json" | "yt_dlp_invalid_response" => {
            "The backend could not read metadata for this YouTube URL."
        }
        "yt_dlp_failed" => "The backend failed to resolve this YouTube URL.",
        "youtube_import_empty" => {
            "The backend did not find any playable videos for this YouTube URL."
        }
        _ => "YouTube URL import failed on the backend.",
    }
}

fn user_message_for_youtube_import_completion_error(error: &CoreError) -> &'static str {
    match error {
        CoreError::Port { code, .. } => user_message_for_playback_failure(code),
        CoreError::Validation { code, .. } | CoreError::Conflict { code, .. } => {
            user_message_for_youtube_import_failure(code)
        }
        CoreError::NotFound { .. } => "YouTube URL import failed on the backend.",
    }
}

fn core_error_code(error: &CoreError) -> &str {
    match error {
        CoreError::Validation { code, .. } | CoreError::Conflict { code, .. } => code,
        CoreError::Port { code, .. } => code,
        CoreError::NotFound { kind, .. } => kind,
    }
}

fn user_message_for_playback_failure(code: &str) -> &'static str {
    match code {
        "yt_dlp_missing" => "yt-dlp is not installed for this backend.",
        "yt_dlp_spawn_failed" => "The backend could not start yt-dlp for playback.",
        "yt_dlp_timeout" => "The backend timed out while preparing audio playback.",
        "audio_output_unavailable" => "Audio output is unavailable on the backend.",
        "audio_source_resolve_failed"
        | "playback_stream_open_failed"
        | "playback_decode_failed" => "The backend could not load audio for playback.",
        "playback_seek_failed" => "The backend could not seek the current playback stream.",
        "playback_url_invalid" | "playback_runtime_failed" => {
            "The backend could not prepare playback."
        }
        "playback_worker_unavailable" => "The backend playback worker is unavailable.",
        _ => "Playback failed on the backend.",
    }
}

fn user_message_for_playback_interruption(code: &str) -> &'static str {
    match code {
        "audio_output_changed" => {
            "Audio output changed. Playback stopped, and the next play will use the current default device."
        }
        "audio_output_stream_lost" => {
            "Playback stopped because the backend lost its audio output stream. Try playing again."
        }
        _ => "Playback stopped because audio output became unavailable on the backend.",
    }
}

fn report_playback_command_error(orchestrator: &mut BackendOrchestrator, error: &CoreError) {
    let CoreError::Port { code, .. } = error else {
        return;
    };

    if let Err(report_error) = orchestrator.emit_system_error(
        code.clone(),
        user_message_for_playback_failure(code),
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
    use std::sync::atomic::{AtomicU64, Ordering};

    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use nocturne_api::{
        PlaybackSeekRequest, PlaybackVolumeRequest, QueueAddRequest, QueueMoveRequest,
        QueueRemoveRequest, QueueResponse, StateSnapshot, YoutubeImportRequest,
    };
    use nocturne_core::{
        CoreEvent, CoreEventEnvelope, CoreEventKind, EventPublisherPort,
        SearchJobStatus as CoreSearchJobStatus, SystemErrorEvent,
        SystemErrorSeverity as CoreSeverity,
    };
    use nocturne_domain::Song;
    use tower::ServiceExt;

    use crate::mapping::CURRENT_EVENT_ID_HEADER;

    static TEST_SETTINGS_STORE_ID: AtomicU64 = AtomicU64::new(0);

    fn test_playback_adapter() -> LocalPlaybackAdapter {
        let (event_tx, _) = mpsc::unbounded_channel();
        LocalPlaybackAdapter::new(SharedPlaybackState::default(), event_tx)
    }

    fn current_session_id(orchestrator: &BackendOrchestrator) -> String {
        orchestrator
            .state()
            .playback()
            .playback_session_id
            .clone()
            .expect("expected playback session id")
    }

    fn test_settings_store() -> Arc<Mutex<LocalAudioSettingsStore>> {
        let store_id = TEST_SETTINGS_STORE_ID.fetch_add(1, Ordering::Relaxed);
        Arc::new(Mutex::new(LocalAudioSettingsStore::from_path(
            std::env::temp_dir().join(format!(
                "nocturne-test-audio-settings-{}-{store_id}.json",
                std::process::id()
            )),
        )))
    }

    fn test_state() -> AppState {
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
            .hydrate_audio_settings(AudioSettings::default())
            .unwrap();

        AppState {
            orchestrator: Arc::new(Mutex::new(orchestrator)),
            events: event_publisher,
            settings_store: test_settings_store(),
        }
    }

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
    async fn pending_search_jobs_are_completed_by_worker_helper() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log, 8);
        let search_runtime = LocalSearchRuntime::default();
        search_runtime.set_fixture(
            "worker fixture",
            vec![Song {
                id: String::from("youtube:test-song"),
                title: String::from("Worker Fixture Song"),
                channel_name: String::from("Fixture Channel"),
                duration_ms: 123_000,
                source_url: String::from("https://www.youtube.com/watch?v=test-song"),
            }],
        );

        let orchestrator = Arc::new(Mutex::new(Orchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher,
            test_playback_adapter(),
            LocalSearchAdapter::new(search_runtime.clone()),
        )));

        let job_id = {
            let mut locked = orchestrator.lock().await;
            locked
                .submit_search("worker fixture")
                .unwrap()
                .job_id
                .unwrap()
        };

        let processed = process_pending_search_jobs(&orchestrator, &search_runtime).await;
        assert_eq!(processed, 1);

        let locked = orchestrator.lock().await;
        let results = locked.search_results(&job_id).unwrap();
        assert_eq!(results.job.status, CoreSearchJobStatus::Completed);
        assert_eq!(results.results.len(), 1);
        assert_eq!(results.results[0].title, "Worker Fixture Song");
    }

    #[tokio::test]
    async fn pending_youtube_import_jobs_are_completed_by_worker_helper() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
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

        let orchestrator = Arc::new(Mutex::new(Orchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher,
            test_playback_adapter(),
            LocalSearchAdapter::new(search_runtime.clone()),
        )));

        {
            let mut locked = orchestrator.lock().await;
            locked
                .submit_youtube_import("https://www.youtube.com/watch?v=tuyZ9f6mHZk")
                .unwrap();
        }

        let processed = process_pending_youtube_import_jobs(&orchestrator, &search_runtime).await;
        assert_eq!(processed, 1);

        let locked = orchestrator.lock().await;
        assert_eq!(locked.queue().len(), 1);
        assert_eq!(locked.queue()[0].song.id, "youtube:tuyZ9f6mHZk");
        drop(locked);

        let snapshot = event_log.snapshot();
        assert!(
            snapshot
                .iter()
                .any(|event| event.event == "youtube.import.completed")
        );
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
        assert_eq!(locked.state().playback().state, nocturne_domain::PlaybackStatus::Stopped);
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
