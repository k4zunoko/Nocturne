mod audio;
mod clock;
mod events;
mod ids;
mod search;

use std::fmt::{self, Display, Formatter};
use std::sync::{Mutex, MutexGuard};

pub use audio::{
    LocalPlaybackAdapter, LocalPlaybackSnapshot, PlaybackCommand, PlaybackStartStatus,
    SharedPlaybackState,
};
pub use clock::LocalClock;
pub use events::{BroadcastEventPublisher, EventCursorError, LocalEventLog, LocalEventPublisher};
pub use ids::LocalIdGenerator;
pub use search::{
    LocalSearchAdapter, LocalSearchFailure, LocalSearchRuntime, PendingSearchJob,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfrastructureProfile {
    Local,
}

impl Display for InfrastructureProfile {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Local => f.write_str("local-subprocess-adapters"),
        }
    }
}

fn recover_lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}
