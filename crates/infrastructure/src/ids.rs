use nocturne_core::{IdGeneratorPort, IdKind};

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LocalIdGenerator {
    next: u64,
}

impl LocalIdGenerator {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

impl IdGeneratorPort for LocalIdGenerator {
    fn next_id(&mut self, kind: IdKind) -> String {
        self.next += 1;
        let prefix = match kind {
            IdKind::Command => "cmd",
            IdKind::Event => "evt",
            IdKind::Snapshot => "snap",
            IdKind::PlaybackSession => "playback_session",
            IdKind::QueueItem => "queue_item",
            IdKind::SearchJob => "job",
        };

        format!("{prefix}_{:04}", self.next)
    }
}
