use axum::Json;
use axum::extract::rejection::JsonRejection;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::sse::Event;
use nocturne_api::{
    BackendStatus, CommandAccepted, EventEnvelope, EventName, PlaybackProgress, ProblemDetails,
    SearchJobCompleted, SearchJobFailed, SearchJobStatus as ApiSearchJobStatus, SearchJobSummary,
    StateSnapshot, SystemError, SystemErrorSeverity, YoutubeImportCompleted, YoutubeImportFailed,
    YoutubeImportJobStatus as ApiYoutubeImportJobStatus, YoutubeImportJobSummary,
};
use nocturne_core::{
    CommandReceipt, CoreError, CoreEvent, CoreEventEnvelope, CoreSnapshot, SearchJobRecord,
    SearchJobStatus as CoreSearchJobStatus, SystemErrorSeverity as CoreSystemErrorSeverity,
    YoutubeImportJobRecord, YoutubeImportJobStatus as CoreYoutubeImportJobStatus,
};
use nocturne_infrastructure::{EventCursorError, LocalSearchFailure};
use serde_json::Value;

pub(crate) const CURRENT_EVENT_ID_HEADER: axum::http::HeaderName =
    axum::http::HeaderName::from_static(nocturne_api::CURRENT_EVENT_ID_HEADER);

pub(crate) fn map_sse_event(core: CoreEventEnvelope<CoreEvent>) -> Event {
    let envelope = map_event_envelope(core);
    let payload = serde_json::to_string(&envelope).expect("server event should serialize");

    Event::default()
        .id(envelope.event_id)
        .event(envelope.event)
        .data(payload)
}

pub(crate) fn command_accepted_response(
    receipt: CommandReceipt,
) -> (StatusCode, Json<CommandAccepted>) {
    (
        StatusCode::ACCEPTED,
        Json(CommandAccepted {
            ok: true,
            command_id: receipt.command_id,
            accepted_at: Some(receipt.accepted_at),
            job_id: receipt.job_id,
            queue_item_id: receipt.queue_item_id,
        }),
    )
}

pub(crate) fn state_headers(last_event_id: Option<String>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(last_event_id) = last_event_id {
        let value =
            HeaderValue::from_str(&last_event_id).expect("event id should be a valid header value");
        headers.insert(CURRENT_EVENT_ID_HEADER, value);
    }
    headers
}

pub(crate) fn map_state_snapshot(snapshot: CoreSnapshot) -> StateSnapshot {
    StateSnapshot {
        backend: BackendStatus {
            ready: snapshot.backend.ready,
            version: snapshot.backend.version,
            yt_dlp_version: snapshot.backend.yt_dlp_version,
        },
        playback: snapshot.playback,
        audio: snapshot.audio,
        current_song: snapshot.current_song,
        queue: snapshot.queue,
        search_jobs: snapshot
            .search_jobs
            .into_iter()
            .map(map_search_job_summary)
            .collect(),
        youtube_import_jobs: snapshot
            .youtube_import_jobs
            .into_iter()
            .map(map_youtube_import_job_summary)
            .collect(),
        revision: snapshot.revision,
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
        CoreEvent::StateUpdated(event) => (
            EventName::StateUpdated,
            serde_json::to_value(map_state_snapshot(event.snapshot))
                .expect("state updated event should serialize"),
        ),
        CoreEvent::PlaybackProgress(event) => (
            EventName::PlaybackProgress,
            serde_json::to_value(PlaybackProgress {
                playback_session_id: event.playback_session_id,
                position_ms: event.position_ms,
            })
            .expect("playback progress event should serialize"),
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
        CoreEvent::YoutubeImportStarted(job) => (
            EventName::YoutubeImportStarted,
            serde_json::to_value(map_youtube_import_job_summary(job))
                .expect("youtube import started event should serialize"),
        ),
        CoreEvent::YoutubeImportCompleted(event) => (
            EventName::YoutubeImportCompleted,
            serde_json::to_value(YoutubeImportCompleted {
                job: map_youtube_import_job_summary(event.job),
            })
            .expect("youtube import completed event should serialize"),
        ),
        CoreEvent::YoutubeImportFailed(event) => (
            EventName::YoutubeImportFailed,
            serde_json::to_value(YoutubeImportFailed {
                job_id: event.job_id,
                code: event.code,
                message: event.message,
            })
            .expect("youtube import failed event should serialize"),
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

pub(crate) fn map_search_job_summary(job: SearchJobRecord) -> SearchJobSummary {
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

fn map_youtube_import_job_summary(job: YoutubeImportJobRecord) -> YoutubeImportJobSummary {
    YoutubeImportJobSummary {
        job_id: job.job_id,
        status: map_youtube_import_job_status(job.status),
        url: job.url,
        total_count: job.total_count,
        queued_count: job.queued_count,
        failed_count: job.failed_count,
        created_at: job.created_at,
        completed_at: job.completed_at,
        error_code: job.error_code,
        error_message: job.error_message,
    }
}

pub(crate) fn map_youtube_import_request_error(
    error: LocalSearchFailure,
    instance: &str,
) -> (StatusCode, Json<ProblemDetails>) {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(ProblemDetails {
            r#type: format!("https://nocturne.local/problems/{}", error.code),
            title: String::from("request validation failed"),
            status: StatusCode::UNPROCESSABLE_ENTITY.as_u16(),
            detail: error.message,
            instance: Some(String::from(instance)),
            errors: Vec::new(),
        }),
    )
}

fn map_youtube_import_job_status(status: CoreYoutubeImportJobStatus) -> ApiYoutubeImportJobStatus {
    match status {
        CoreYoutubeImportJobStatus::Running => ApiYoutubeImportJobStatus::Running,
        CoreYoutubeImportJobStatus::Completed => ApiYoutubeImportJobStatus::Completed,
        CoreYoutubeImportJobStatus::Failed => ApiYoutubeImportJobStatus::Failed,
    }
}

fn map_system_error_severity(severity: CoreSystemErrorSeverity) -> SystemErrorSeverity {
    match severity {
        CoreSystemErrorSeverity::Warning => SystemErrorSeverity::Warning,
        CoreSystemErrorSeverity::Error => SystemErrorSeverity::Error,
    }
}

pub(crate) fn map_cursor_error(error: EventCursorError) -> (StatusCode, Json<ProblemDetails>) {
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

pub(crate) fn map_core_error(
    error: CoreError,
    instance: &str,
) -> (StatusCode, Json<ProblemDetails>) {
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

pub(crate) fn map_json_rejection(
    error: JsonRejection,
    instance: &str,
) -> (StatusCode, Json<ProblemDetails>) {
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
    use nocturne_api::EventName;
    use nocturne_core::{
        CoreEvent, CoreEventEnvelope, SystemErrorEvent, SystemErrorSeverity as CoreSeverity,
    };

    use super::map_event_envelope;
    use crate::mapping::map_core_error;
    use crate::test_support::problem_detail_instance;

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
        assert_eq!(envelope.event, EventName::SystemError.to_string());
        assert_eq!(envelope.data["code"], "test_error");
        assert_eq!(envelope.data["severity"], "error");
        assert!(envelope.data.get("event").is_none());
    }

    #[test]
    fn validation_errors_map_to_problem_details() {
        let (status, body) = map_core_error(
            nocturne_core::CoreError::validation("empty_query", "query must not be empty"),
            problem_detail_instance("/api/v1/commands/search"),
        );

        assert_eq!(status, axum::http::StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body.0.detail, "query must not be empty");
    }
}
