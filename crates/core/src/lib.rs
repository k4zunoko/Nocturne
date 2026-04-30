//! Headless application boundary for Nocturne.
//!
//! This crate is the future home of orchestration logic that should remain
//! independent from transport (HTTP/SSE) and presentation (TUI/Web).

pub mod models;
pub mod orchestrator;
pub mod ports;

use nocturne_domain::{PlaybackState, PlaybackStatus};

pub use models::{
    BackendState, CommandReceipt, CoreEvent, CoreEventEnvelope, CoreEventKind, CoreId,
    CoreSnapshot, CoreTimestamp, PlaybackPositionUpdatedEvent, PlaybackStateChangedEvent,
    PlaybackTrackChangedEvent, QueueUpdateReason, QueueUpdatedEvent, SearchJobCompletedEvent,
    SearchJobFailedEvent, SearchJobRecord, SearchJobStatus, SearchResultsRecord, SystemErrorEvent,
    SystemErrorSeverity,
};
pub use orchestrator::{CoreError, Orchestrator};
pub use ports::{
    ClockPort, EventPublisherPort, IdGeneratorPort, IdKind, PlaybackPort, PortError, SearchPort,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NocturneCore;

impl NocturneCore {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    #[must_use]
    pub const fn workspace_profile(self) -> &'static str {
        "client-server-v1"
    }

    #[must_use]
    pub const fn initial_playback_state(self) -> PlaybackState {
        PlaybackState {
            state: PlaybackStatus::Stopped,
            position_ms: 0,
            current_queue_item_id: None,
        }
    }
}
