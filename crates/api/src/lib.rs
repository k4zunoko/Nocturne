use std::fmt::{self, Display, Formatter};

use nocturne_domain::{PlaybackState, QueueItem, Song};
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendSnapshot {
    pub ready: bool,
    pub version: Option<String>,
    pub playback: PlaybackState,
    pub current_song: Option<Song>,
    pub queue: Vec<QueueItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandAccepted {
    pub ok: bool,
    pub command_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventEnvelope<T> {
    pub event_id: String,
    pub event: String,
    pub timestamp: String,
    pub data: T,
}
