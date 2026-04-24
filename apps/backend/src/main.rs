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
use axum::http::{HeaderMap, HeaderName, HeaderValue};
use axum::response::IntoResponse;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::routing::{get, post};
use axum::{Json, Router};
use nocturne_api::{
    ApiVersion, BackendStatus, CommandAccepted, EventEnvelope, EventName, HealthResponse,
    PlaybackPositionUpdated, PlaybackStateChanged, PlaybackTrackChanged, ProblemDetails,
    QueueUpdateReason as ApiQueueUpdateReason, QueueUpdated, SearchCommandRequest,
    SearchJobCompleted, SearchJobFailed, SearchJobStatus as ApiSearchJobStatus, SearchJobSummary,
    SearchResultsResponse, StateSnapshot, SystemError, SystemErrorSeverity,
};
use nocturne_core::{
    CoreError, CoreEvent, CoreEventEnvelope, CoreSnapshot, NocturneCore, Orchestrator,
    QueueUpdateReason as CoreQueueUpdateReason, SearchJobRecord,
    SearchJobStatus as CoreSearchJobStatus, SystemErrorSeverity as CoreSystemErrorSeverity,
};
use nocturne_infrastructure::{
    BroadcastEventPublisher, EventCursorError, InfrastructureProfile, LocalClock, LocalEventLog,
    LocalIdGenerator, LocalPlaybackAdapter, LocalSearchAdapter, LocalSearchRuntime,
    SharedPlaybackState,
};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::sync::broadcast;

type BackendOrchestrator = Orchestrator<
    LocalClock,
    LocalIdGenerator,
    BroadcastEventPublisher,
    LocalPlaybackAdapter,
    LocalSearchAdapter,
>;

const CURRENT_EVENT_ID_HEADER: HeaderName =
    HeaderName::from_static(nocturne_api::CURRENT_EVENT_ID_HEADER);

#[derive(Clone)]
struct AppState {
    orchestrator: Arc<Mutex<BackendOrchestrator>>,
    events: BroadcastEventPublisher,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let core = NocturneCore::new();
    let event_log = LocalEventLog::default();
    let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 128);
    let playback_state = SharedPlaybackState::default();
    let search_runtime = LocalSearchRuntime::default();

    let mut orchestrator = Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        event_publisher.clone(),
        LocalPlaybackAdapter::new(playback_state.clone()),
        LocalSearchAdapter::new(search_runtime.clone()),
    );
    orchestrator.set_backend_version(Some(env!("CARGO_PKG_VERSION")));

    let snapshot = orchestrator.snapshot();
    let orchestrator = Arc::new(Mutex::new(orchestrator));
    spawn_search_worker(orchestrator.clone(), search_runtime.clone());
    let state = AppState {
        orchestrator,
        events: event_publisher.clone(),
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

fn spawn_search_worker(
    orchestrator: Arc<Mutex<BackendOrchestrator>>,
    search_runtime: LocalSearchRuntime,
) {
    tokio::spawn(async move {
        loop {
            let processed = process_pending_search_jobs(&orchestrator, &search_runtime).await;
            let delay = if processed == 0 {
                Duration::from_millis(100)
            } else {
                Duration::from_millis(10)
            };
            tokio::time::sleep(delay).await;
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

fn app_router(state: AppState) -> Router {
    Router::new()
        .route(nocturne_api::HEALTH_PATH, get(health_handler))
        .route(nocturne_api::STATE_PATH, get(state_handler))
        .route(nocturne_api::EVENTS_PATH, get(events_handler))
        .route("/api/v1/commands/search", post(search_command_handler))
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

fn map_sse_event(core: CoreEventEnvelope<CoreEvent>) -> Event {
    let envelope = map_event_envelope(core);
    let payload = serde_json::to_string(&envelope).expect("server event should serialize");

    Event::default()
        .id(envelope.event_id)
        .event(envelope.event)
        .data(payload)
}

fn state_headers(last_event_id: Option<String>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(last_event_id) = last_event_id {
        let value =
            HeaderValue::from_str(&last_event_id).expect("event id should be a valid header value");
        headers.insert(CURRENT_EVENT_ID_HEADER, value);
    }
    headers
}

fn map_state_snapshot(snapshot: CoreSnapshot) -> StateSnapshot {
    StateSnapshot {
        backend: BackendStatus {
            ready: snapshot.backend.ready,
            version: snapshot.backend.version,
        },
        playback: snapshot.playback,
        current_song: snapshot.current_song,
        queue: snapshot.queue,
        search_jobs: snapshot
            .search_jobs
            .into_iter()
            .map(map_search_job_summary)
            .collect(),
        snapshot_id: snapshot.snapshot_id,
        timestamp: snapshot.timestamp,
    }
}

fn map_event_envelope(core: CoreEventEnvelope<CoreEvent>) -> EventEnvelope<Value> {
    let CoreEventEnvelope {
        event_id,
        event: _,
        timestamp,
        data,
    } = core;

    let (event_name, server_event) = map_server_event(data);
    EventEnvelope::new(event_id, event_name, timestamp, server_event)
}

fn map_server_event(core: CoreEvent) -> (EventName, Value) {
    match core {
        CoreEvent::PlaybackStateChanged(event) => (
            EventName::PlaybackStateChanged,
            serde_json::to_value(PlaybackStateChanged {
                state: event.state,
                current_queue_item_id: event.current_queue_item_id,
                position_ms: event.position_ms,
            })
            .expect("playback state event should serialize"),
        ),
        CoreEvent::PlaybackTrackChanged(event) => (
            EventName::PlaybackTrackChanged,
            serde_json::to_value(PlaybackTrackChanged {
                queue_item_id: event.queue_item_id,
                song: event.song,
            })
            .expect("track changed event should serialize"),
        ),
        CoreEvent::PlaybackPositionUpdated(event) => (
            EventName::PlaybackPositionUpdated,
            serde_json::to_value(PlaybackPositionUpdated {
                position_ms: event.position_ms,
            })
            .expect("position event should serialize"),
        ),
        CoreEvent::QueueUpdated(event) => (
            EventName::QueueUpdated,
            serde_json::to_value(QueueUpdated {
                reason: map_queue_update_reason(event.reason),
                items: event.items,
            })
            .expect("queue updated event should serialize"),
        ),
        CoreEvent::SearchJobStarted(job) => (
            EventName::SearchJobStarted,
            serde_json::to_value(map_search_job_summary(job))
                .expect("search job started event should serialize"),
        ),
        CoreEvent::SearchJobCompleted(event) => {
            let job = map_search_job_summary(event.job);
            (
                EventName::SearchJobCompleted,
                serde_json::to_value(SearchJobCompleted {
                    job,
                    results: event.results,
                })
                .expect("search job completed event should serialize"),
            )
        }
        CoreEvent::SearchJobFailed(event) => (
            EventName::SearchJobFailed,
            serde_json::to_value(SearchJobFailed {
                job_id: event.job_id,
                code: event.code,
                message: event.message,
            })
            .expect("search job failed event should serialize"),
        ),
        CoreEvent::SystemError(event) => (
            EventName::SystemError,
            serde_json::to_value(SystemError {
                code: event.code,
                message: event.message,
                severity: map_system_error_severity(event.severity),
                context: None,
            })
            .expect("system error event should serialize"),
        ),
    }
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

fn map_search_job_summary(job: SearchJobRecord) -> SearchJobSummary {
    SearchJobSummary {
        job_id: job.job_id,
        status: map_search_job_status(job.status),
        query: job.query,
        created_at: job.created_at,
        completed_at: job.completed_at,
        result_count: job.result_count,
    }
}

fn map_search_job_status(status: CoreSearchJobStatus) -> ApiSearchJobStatus {
    match status {
        CoreSearchJobStatus::Queued => ApiSearchJobStatus::Queued,
        CoreSearchJobStatus::Running => ApiSearchJobStatus::Running,
        CoreSearchJobStatus::Completed => ApiSearchJobStatus::Completed,
        CoreSearchJobStatus::Failed => ApiSearchJobStatus::Failed,
    }
}

fn map_queue_update_reason(reason: CoreQueueUpdateReason) -> ApiQueueUpdateReason {
    match reason {
        CoreQueueUpdateReason::Add => ApiQueueUpdateReason::Add,
        CoreQueueUpdateReason::Remove => ApiQueueUpdateReason::Remove,
        CoreQueueUpdateReason::Move => ApiQueueUpdateReason::Move,
        CoreQueueUpdateReason::Clear => ApiQueueUpdateReason::Clear,
        CoreQueueUpdateReason::CurrentChanged => ApiQueueUpdateReason::CurrentChanged,
    }
}

fn map_system_error_severity(severity: CoreSystemErrorSeverity) -> SystemErrorSeverity {
    match severity {
        CoreSystemErrorSeverity::Warning => SystemErrorSeverity::Warning,
        CoreSystemErrorSeverity::Error => SystemErrorSeverity::Error,
    }
}

fn map_cursor_error(error: EventCursorError) -> (StatusCode, Json<ProblemDetails>) {
    match error {
        EventCursorError::CursorNotFound => (
            StatusCode::CONFLICT,
            Json(ProblemDetails {
                r#type: String::from("https://nocturne.local/problems/event-cursor-not-found"),
                title: String::from("event cursor no longer available"),
                status: StatusCode::CONFLICT.as_u16(),
                detail: String::from(
                    "The requested Last-Event-ID is no longer present in the replay buffer. Refresh state and reconnect to the event stream.",
                ),
                instance: Some(String::from(nocturne_api::EVENTS_PATH)),
                errors: Vec::new(),
            }),
        ),
    }
}

fn map_core_error(error: CoreError, instance: &str) -> (StatusCode, Json<ProblemDetails>) {
    match error {
        CoreError::Validation { code, message } => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ProblemDetails {
                r#type: format!("https://nocturne.local/problems/{code}"),
                title: String::from("request validation failed"),
                status: StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
                detail: message,
                instance: Some(String::from(instance)),
                errors: Vec::new(),
            }),
        ),
        CoreError::NotFound { kind, id } => (
            StatusCode::NOT_FOUND,
            Json(ProblemDetails {
                r#type: format!("https://nocturne.local/problems/{kind}-not-found"),
                title: format!("{kind} not found"),
                status: StatusCode::NOT_FOUND.as_u16(),
                detail: format!("{kind} '{id}' was not found."),
                instance: Some(String::from(instance)),
                errors: Vec::new(),
            }),
        ),
        CoreError::Conflict { code, message } => (
            StatusCode::CONFLICT,
            Json(ProblemDetails {
                r#type: format!("https://nocturne.local/problems/{code}"),
                title: String::from("request conflict"),
                status: StatusCode::CONFLICT.as_u16(),
                detail: message,
                instance: Some(String::from(instance)),
                errors: Vec::new(),
            }),
        ),
        CoreError::Port { code, message } => (
            StatusCode::BAD_GATEWAY,
            Json(ProblemDetails {
                r#type: format!("https://nocturne.local/problems/{code}"),
                title: String::from("backend dependency failure"),
                status: StatusCode::BAD_GATEWAY.as_u16(),
                detail: message,
                instance: Some(String::from(instance)),
                errors: Vec::new(),
            }),
        ),
    }
}

fn map_json_rejection(error: JsonRejection, instance: &str) -> (StatusCode, Json<ProblemDetails>) {
    match error {
        JsonRejection::JsonSyntaxError(inner) => (
            StatusCode::BAD_REQUEST,
            Json(ProblemDetails {
                r#type: String::from("https://nocturne.local/problems/malformed-json"),
                title: String::from("malformed json request body"),
                status: StatusCode::BAD_REQUEST.as_u16(),
                detail: inner.body_text(),
                instance: Some(String::from(instance)),
                errors: Vec::new(),
            }),
        ),
        JsonRejection::JsonDataError(inner) => (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ProblemDetails {
                r#type: String::from("https://nocturne.local/problems/request-validation"),
                title: String::from("request validation failed"),
                status: StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
                detail: inner.body_text(),
                instance: Some(String::from(instance)),
                errors: Vec::new(),
            }),
        ),
        JsonRejection::MissingJsonContentType(inner) => (
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Json(ProblemDetails {
                r#type: String::from("https://nocturne.local/problems/unsupported-media-type"),
                title: String::from("unsupported media type"),
                status: StatusCode::UNSUPPORTED_MEDIA_TYPE.as_u16(),
                detail: inner.body_text(),
                instance: Some(String::from(instance)),
                errors: Vec::new(),
            }),
        ),
        JsonRejection::BytesRejection(inner) => (
            StatusCode::BAD_REQUEST,
            Json(ProblemDetails {
                r#type: String::from("https://nocturne.local/problems/request-body-read-failed"),
                title: String::from("request body could not be read"),
                status: StatusCode::BAD_REQUEST.as_u16(),
                detail: inner.body_text(),
                instance: Some(String::from(instance)),
                errors: Vec::new(),
            }),
        ),
        _ => (
            StatusCode::BAD_REQUEST,
            Json(ProblemDetails {
                r#type: String::from("https://nocturne.local/problems/invalid-json-request"),
                title: String::from("invalid json request body"),
                status: StatusCode::BAD_REQUEST.as_u16(),
                detail: error.body_text(),
                instance: Some(String::from(instance)),
                errors: Vec::new(),
            }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode};
    use nocturne_core::{
        CoreEventKind, EventPublisherPort, SystemErrorEvent, SystemErrorSeverity as CoreSeverity,
    };
    use nocturne_domain::Song;
    use tower::ServiceExt;

    fn test_state() -> AppState {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log, 8);
        let orchestrator = Orchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher.clone(),
            LocalPlaybackAdapter::new(SharedPlaybackState::default()),
            LocalSearchAdapter::new(LocalSearchRuntime::default()),
        );

        AppState {
            orchestrator: Arc::new(Mutex::new(orchestrator)),
            events: event_publisher,
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
    }

    #[test]
    fn core_events_map_to_api_envelopes() {
        let envelope = map_event_envelope(CoreEventEnvelope::new(
            String::from("evt_0001"),
            nocturne_core::CoreEventKind::SystemError,
            String::from("2026-04-23T12:34:56Z"),
            CoreEvent::SystemError(SystemErrorEvent {
                code: String::from("test_error"),
                message: String::from("test message"),
                severity: CoreSeverity::Error,
            }),
        ));

        assert_eq!(envelope.event_id, "evt_0001");
        assert_eq!(envelope.event, "system.error");
        assert_eq!(envelope.data["code"], "test_error");
        assert_eq!(envelope.data["severity"], "error");
        assert!(envelope.data.get("event").is_none());
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
            LocalPlaybackAdapter::new(SharedPlaybackState::default()),
            LocalSearchAdapter::new(LocalSearchRuntime::default()),
        );
        let app = app_router(AppState {
            orchestrator: Arc::new(Mutex::new(orchestrator)),
            events: event_publisher,
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
            LocalPlaybackAdapter::new(SharedPlaybackState::default()),
            LocalSearchAdapter::new(LocalSearchRuntime::default()),
        );
        let app = app_router(AppState {
            orchestrator: Arc::new(Mutex::new(orchestrator)),
            events: event_publisher,
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
            LocalPlaybackAdapter::new(SharedPlaybackState::default()),
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
            LocalPlaybackAdapter::new(SharedPlaybackState::default()),
            LocalSearchAdapter::new(search_runtime.clone()),
        )));
        let state = AppState {
            orchestrator: orchestrator.clone(),
            events: event_publisher,
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
            LocalPlaybackAdapter::new(SharedPlaybackState::default()),
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
}
