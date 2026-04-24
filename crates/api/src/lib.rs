use std::fmt::{self, Display, Formatter};

use nocturne_domain::{PlaybackState, QueueItem, Song};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Id = String;
pub type Timestamp = String;
pub type DurationMs = u64;
pub type Index = u64;

pub const API_V1_BASE_PATH: &str = "/api/v1";
pub const HEALTH_PATH: &str = "/api/v1/health";
pub const STATE_PATH: &str = "/api/v1/state";
pub const EVENTS_PATH: &str = "/api/v1/events";
pub const LAST_EVENT_ID_HEADER: &str = "last-event-id";
pub const CURRENT_EVENT_ID_HEADER: &str = "x-nocturne-last-event-id";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiVersion {
    V1,
}

impl Display for ApiVersion {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::V1 => f.write_str("v1"),
        }
    }
}

impl ApiVersion {
    #[must_use]
    pub const fn base_path(self) -> &'static str {
        match self {
            Self::V1 => API_V1_BASE_PATH,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchJobStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchJobSummary {
    pub job_id: Id,
    pub status: SearchJobStatus,
    pub query: String,
    pub created_at: Timestamp,
    pub completed_at: Option<Timestamp>,
    pub result_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendStatus {
    pub ready: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthResponse {
    pub ok: bool,
    pub ready: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub backend: BackendStatus,
    pub playback: PlaybackState,
    pub current_song: Option<Song>,
    pub queue: Vec<QueueItem>,
    pub search_jobs: Vec<SearchJobSummary>,
    pub snapshot_id: Id,
    pub timestamp: Timestamp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAccepted {
    pub ok: bool,
    pub command_id: String,
    pub accepted_at: Option<Timestamp>,
    pub job_id: Option<Id>,
    pub queue_item_id: Option<Id>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProblemFieldError {
    pub pointer: String,
    pub message: String,
    pub code: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProblemDetails {
    pub r#type: String,
    pub title: String,
    pub status: u16,
    pub detail: String,
    pub instance: Option<String>,
    pub errors: Vec<ProblemFieldError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchCommandRequest {
    pub query: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchResultsResponse {
    pub job: SearchJobSummary,
    pub results: Vec<Song>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueAddRequest {
    pub song_id: Id,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueRemoveRequest {
    pub queue_item_id: Id,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueueMoveRequest {
    pub queue_item_id: Id,
    pub to_index: Index,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmptyPayload {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueResponse {
    pub items: Vec<QueueItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlaybackSeekRequest {
    pub position_ms: DurationMs,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueUpdateReason {
    Add,
    Remove,
    Move,
    Clear,
    CurrentChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemErrorSeverity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackStateChanged {
    pub state: nocturne_domain::PlaybackStatus,
    pub current_queue_item_id: Option<Id>,
    pub position_ms: DurationMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackTrackChanged {
    pub queue_item_id: Id,
    pub song: Song,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackPositionUpdated {
    pub position_ms: DurationMs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueUpdated {
    pub reason: QueueUpdateReason,
    pub items: Vec<QueueItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchJobCompleted {
    pub job: SearchJobSummary,
    pub results: Vec<Song>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SearchJobFailed {
    pub job_id: Id,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemError {
    pub code: String,
    pub message: String,
    pub severity: SystemErrorSeverity,
    pub context: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventName {
    PlaybackStateChanged,
    PlaybackTrackChanged,
    PlaybackPositionUpdated,
    QueueUpdated,
    SearchJobStarted,
    SearchJobCompleted,
    SearchJobFailed,
    SystemError,
}

impl Display for EventName {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::PlaybackStateChanged => "playback.state.changed",
            Self::PlaybackTrackChanged => "playback.track.changed",
            Self::PlaybackPositionUpdated => "playback.position.updated",
            Self::QueueUpdated => "queue.updated",
            Self::SearchJobStarted => "search.job.started",
            Self::SearchJobCompleted => "search.job.completed",
            Self::SearchJobFailed => "search.job.failed",
            Self::SystemError => "system.error",
        };

        f.write_str(name)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope<T> {
    pub event_id: Id,
    pub event: String,
    pub timestamp: Timestamp,
    pub data: T,
}

impl<T> EventEnvelope<T> {
    #[must_use]
    pub fn new(event_id: Id, event: EventName, timestamp: Timestamp, data: T) -> Self {
        Self {
            event_id,
            event: event.to_string(),
            timestamp,
            data,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", content = "data")]
pub enum ServerEvent {
    #[serde(rename = "playback.state.changed")]
    PlaybackStateChanged(PlaybackStateChanged),
    #[serde(rename = "playback.track.changed")]
    PlaybackTrackChanged(PlaybackTrackChanged),
    #[serde(rename = "playback.position.updated")]
    PlaybackPositionUpdated(PlaybackPositionUpdated),
    #[serde(rename = "queue.updated")]
    QueueUpdated(QueueUpdated),
    #[serde(rename = "search.job.started")]
    SearchJobStarted(SearchJobSummary),
    #[serde(rename = "search.job.completed")]
    SearchJobCompleted(SearchJobCompleted),
    #[serde(rename = "search.job.failed")]
    SearchJobFailed(SearchJobFailed),
    #[serde(rename = "system.error")]
    SystemError(SystemError),
}
