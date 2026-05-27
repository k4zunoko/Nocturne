use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;

use async_stream::stream;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json as ExtractJson, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use nocturne_api::{
    CommandAccepted, EmptyPayload, HealthResponse, PlaybackRepeatRequest, PlaybackSeekRequest,
    PlaybackVolumeRequest, ProblemDetails, QueueAddRequest, QueueMoveRequest, QueueRemoveRequest,
    QueueResponse, SearchCommandRequest, SearchResultsResponse, YoutubeImportRequest,
};
use nocturne_core::SystemErrorSeverity as CoreSystemErrorSeverity;
use tokio::sync::Mutex;
use tokio::sync::broadcast;

use crate::mapping::{
    command_accepted_response, map_core_error, map_cursor_error, map_json_rejection,
    map_search_job_summary, map_sse_event, map_state_snapshot, map_youtube_import_request_error,
    state_headers,
};
use crate::{BackendOrchestrator, report_playback_command_error};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) orchestrator: Arc<Mutex<BackendOrchestrator>>,
    pub(crate) events: nocturne_infrastructure::BroadcastEventPublisher,
    pub(crate) settings_store: Arc<Mutex<nocturne_infrastructure::LocalAudioSettingsStore>>,
}

pub(crate) fn app_router(state: AppState) -> Router {
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

async fn state_handler(State(state): State<AppState>) -> impl IntoResponse {
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
    let canonical_url = nocturne_infrastructure::canonicalize_supported_youtube_url(&request.url)
        .map_err(|error| {
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

#[cfg(test)]
mod tests;
