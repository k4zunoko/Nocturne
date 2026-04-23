use std::sync::{Arc, Mutex};

use nocturne_core::{PlaybackPort, PortError};
use nocturne_domain::QueueItem;

use crate::recover_lock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackCommand {
    Start {
        queue_item_id: String,
        position_ms: u64,
    },
    Pause,
    Resume,
    Stop,
    Seek {
        position_ms: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LocalPlaybackSnapshot {
    pub current_item: Option<QueueItem>,
    pub position_ms: u64,
    pub paused: bool,
    pub history: Vec<PlaybackCommand>,
}

#[derive(Debug, Clone, Default)]
pub struct SharedPlaybackState {
    inner: Arc<Mutex<LocalPlaybackSnapshot>>,
}

impl SharedPlaybackState {
    #[must_use]
    pub fn snapshot(&self) -> LocalPlaybackSnapshot {
        recover_lock(&self.inner).clone()
    }
}

#[derive(Debug, Clone)]
pub struct LocalPlaybackAdapter {
    state: SharedPlaybackState,
}

impl LocalPlaybackAdapter {
    #[must_use]
    pub fn new(state: SharedPlaybackState) -> Self {
        Self { state }
    }
}

impl PlaybackPort for LocalPlaybackAdapter {
    fn start(&mut self, item: &QueueItem, position_ms: u64) -> Result<(), PortError> {
        let mut state = recover_lock(&self.state.inner);
        state.current_item = Some(item.clone());
        state.position_ms = position_ms;
        state.paused = false;
        state.history.push(PlaybackCommand::Start {
            queue_item_id: item.id.clone(),
            position_ms,
        });
        Ok(())
    }

    fn pause(&mut self) -> Result<(), PortError> {
        let mut state = recover_lock(&self.state.inner);
        state.paused = true;
        state.history.push(PlaybackCommand::Pause);
        Ok(())
    }

    fn resume(&mut self) -> Result<(), PortError> {
        let mut state = recover_lock(&self.state.inner);
        state.paused = false;
        state.history.push(PlaybackCommand::Resume);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PortError> {
        let mut state = recover_lock(&self.state.inner);
        state.current_item = None;
        state.position_ms = 0;
        state.paused = false;
        state.history.push(PlaybackCommand::Stop);
        Ok(())
    }

    fn seek(&mut self, position_ms: u64) -> Result<(), PortError> {
        let mut state = recover_lock(&self.state.inner);
        state.position_ms = position_ms;
        state.history.push(PlaybackCommand::Seek { position_ms });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use nocturne_domain::{QueueItemStatus, Song};

    fn queue_item(id: &str) -> QueueItem {
        QueueItem {
            id: id.to_owned(),
            song: Song {
                id: format!("song_{id}"),
                title: format!("Song {id}"),
                channel_name: "Channel".to_owned(),
                duration_ms: 180_000,
                source_url: format!("https://example.com/{id}"),
            },
            added_at: "2026-04-23T12:34:56Z".to_owned(),
            status: QueueItemStatus::Queued,
        }
    }

    #[test]
    fn playback_commands_are_recorded() {
        let shared = SharedPlaybackState::default();
        let mut adapter = LocalPlaybackAdapter::new(shared.clone());

        adapter.start(&queue_item("queue_item_1"), 0).unwrap();
        adapter.seek(42_000).unwrap();
        adapter.pause().unwrap();
        adapter.resume().unwrap();

        let snapshot = shared.snapshot();
        assert_eq!(snapshot.position_ms, 42_000);
        assert!(!snapshot.paused);
        assert_eq!(snapshot.history.len(), 4);
    }
}
