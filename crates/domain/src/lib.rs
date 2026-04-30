use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AudioSettings {
    pub volume_percent: u8,
}

impl AudioSettings {
    pub const DEFAULT_VOLUME_PERCENT: u8 = 50;

    #[must_use]
    pub const fn new(volume_percent: u8) -> Self {
        Self { volume_percent }
    }

    #[must_use]
    pub const fn clamped(self) -> Self {
        Self {
            volume_percent: if self.volume_percent > 100 {
                100
            } else {
                self.volume_percent
            },
        }
    }

    #[must_use]
    pub fn gain(self) -> f32 {
        f32::from(self.clamped().volume_percent) / 100.0
    }
}

impl Default for AudioSettings {
    fn default() -> Self {
        Self::new(Self::DEFAULT_VOLUME_PERCENT)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Song {
    pub id: String,
    pub title: String,
    pub channel_name: String,
    pub duration_ms: u64,
    pub source_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueItemStatus {
    Queued,
    Loading,
    Playing,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    pub id: String,
    pub song: Song,
    pub added_at: String,
    pub status: QueueItemStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlaybackStatus {
    Stopped,
    Loading,
    Playing,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybackState {
    pub state: PlaybackStatus,
    pub position_ms: u64,
    pub current_queue_item_id: Option<String>,
}
