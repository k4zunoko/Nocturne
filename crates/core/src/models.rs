use std::fmt::{self, Display, Formatter};

use nocturne_domain::{AudioSettings, PlaybackState, PlaybackStatus, QueueItem, Song};

pub type CoreId = String;
pub type CoreTimestamp = String;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendState {
    pub ready: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchJobStatus {
    Queued,
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchJobRecord {
    pub job_id: CoreId,
    pub status: SearchJobStatus,
    pub query: String,
    pub created_at: CoreTimestamp,
    pub completed_at: Option<CoreTimestamp>,
    pub result_count: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResultsRecord {
    pub job: SearchJobRecord,
    pub results: Vec<Song>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreSnapshot {
    pub backend: BackendState,
    pub playback: PlaybackState,
    pub audio: AudioSettings,
    pub current_song: Option<Song>,
    pub queue: Vec<QueueItem>,
    pub search_jobs: Vec<SearchJobRecord>,
    pub snapshot_id: CoreId,
    pub timestamp: CoreTimestamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandReceipt {
    pub command_id: CoreId,
    pub accepted_at: CoreTimestamp,
    pub job_id: Option<CoreId>,
    pub queue_item_id: Option<CoreId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueUpdateReason {
    Add,
    Remove,
    Move,
    Clear,
    CurrentChanged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemErrorSeverity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackStateChangedEvent {
    pub state: PlaybackStatus,
    pub current_queue_item_id: Option<CoreId>,
    pub position_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackTrackChangedEvent {
    pub queue_item_id: CoreId,
    pub song: Song,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackPositionUpdatedEvent {
    pub position_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueUpdatedEvent {
    pub reason: QueueUpdateReason,
    pub items: Vec<QueueItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchJobCompletedEvent {
    pub job: SearchJobRecord,
    pub results: Vec<Song>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchJobFailedEvent {
    pub job_id: CoreId,
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemErrorEvent {
    pub code: String,
    pub message: String,
    pub severity: SystemErrorSeverity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreEvent {
    PlaybackStateChanged(PlaybackStateChangedEvent),
    AudioSettingsChanged(AudioSettings),
    PlaybackTrackChanged(PlaybackTrackChangedEvent),
    PlaybackPositionUpdated(PlaybackPositionUpdatedEvent),
    QueueUpdated(QueueUpdatedEvent),
    SearchJobStarted(SearchJobRecord),
    SearchJobCompleted(SearchJobCompletedEvent),
    SearchJobFailed(SearchJobFailedEvent),
    SystemError(SystemErrorEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreEventKind {
    PlaybackStateChanged,
    AudioSettingsChanged,
    PlaybackTrackChanged,
    PlaybackPositionUpdated,
    QueueUpdated,
    SearchJobStarted,
    SearchJobCompleted,
    SearchJobFailed,
    SystemError,
}

impl Display for CoreEventKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::PlaybackStateChanged => "playback.state.changed",
            Self::AudioSettingsChanged => "audio.settings.changed",
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreEventEnvelope<T> {
    pub event_id: CoreId,
    pub event: String,
    pub timestamp: CoreTimestamp,
    pub data: T,
}

impl<T> CoreEventEnvelope<T> {
    #[must_use]
    pub fn new(event_id: CoreId, event: CoreEventKind, timestamp: CoreTimestamp, data: T) -> Self {
        Self {
            event_id,
            event: event.to_string(),
            timestamp,
            data,
        }
    }
}
