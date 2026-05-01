mod audio;
mod clock;
mod events;
mod ids;
mod search;
mod settings;
mod yt_dlp;

use std::fmt::{self, Display, Formatter};
use std::sync::{Mutex, MutexGuard};

pub use audio::{
    LocalPlaybackAdapter, LocalPlaybackSnapshot, PlaybackCommand, PlaybackWorkerEvent,
    SharedPlaybackState,
};
pub use clock::LocalClock;
pub use events::{BroadcastEventPublisher, EventCursorError, LocalEventLog, LocalEventPublisher};
pub use ids::LocalIdGenerator;
pub use search::{LocalSearchAdapter, LocalSearchFailure, LocalSearchRuntime, PendingSearchJob};
pub use settings::LocalAudioSettingsStore;
pub use yt_dlp::{
    DEFAULT_WINDOWS_YT_DLP_PATH, LocalYtDlpManager, LocalYtDlpSettings, LocalYtDlpSettingsStore,
    UpdateCheckResult, YT_DLP_COMMAND_TIMEOUT, YT_DLP_PATH_ENV, YT_DLP_UPDATE_INTERVAL,
    YtDlpReleaseChannel,
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
