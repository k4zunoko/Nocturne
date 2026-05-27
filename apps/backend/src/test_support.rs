#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use nocturne_core::Orchestrator;
use nocturne_domain::{AudioSettings, Song};
use nocturne_infrastructure::{
    BroadcastEventPublisher, LocalAudioSettingsStore, LocalClock, LocalEventLog, LocalIdGenerator,
    LocalPlaybackAdapter, LocalSearchAdapter, LocalSearchRuntime, SharedPlaybackState,
};
use tokio::sync::Mutex;
use tokio::sync::mpsc;

use crate::BackendOrchestrator;
use crate::http::AppState;

static TEST_SETTINGS_STORE_ID: AtomicU64 = AtomicU64::new(0);

pub(crate) fn test_playback_adapter() -> LocalPlaybackAdapter {
    let (event_tx, _) = mpsc::unbounded_channel();
    LocalPlaybackAdapter::new(SharedPlaybackState::default(), event_tx)
}

pub(crate) fn problem_detail_instance(path: &str) -> &str {
    path
}

pub(crate) fn test_settings_store() -> Arc<Mutex<LocalAudioSettingsStore>> {
    let store_id = TEST_SETTINGS_STORE_ID.fetch_add(1, Ordering::Relaxed);
    Arc::new(Mutex::new(LocalAudioSettingsStore::from_path(
        std::env::temp_dir().join(format!(
            "nocturne-test-audio-settings-{}-{store_id}.json",
            std::process::id()
        )),
    )))
}

pub(crate) fn test_state() -> AppState {
    let event_log = LocalEventLog::default();
    let event_publisher = BroadcastEventPublisher::new(event_log, 8);
    let mut orchestrator = Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        event_publisher.clone(),
        test_playback_adapter(),
        LocalSearchAdapter::new(LocalSearchRuntime::default()),
    );
    orchestrator
        .hydrate_audio_settings(AudioSettings::default())
        .unwrap();

    AppState {
        orchestrator: Arc::new(Mutex::new(orchestrator)),
        events: event_publisher,
        settings_store: test_settings_store(),
    }
}

pub(crate) fn current_session_id(orchestrator: &BackendOrchestrator) -> String {
    orchestrator
        .state()
        .playback()
        .playback_session_id
        .clone()
        .expect("expected playback session id")
}

pub(crate) fn test_search_runtime_with_fixture(
    query: &str,
) -> (
    BroadcastEventPublisher,
    LocalSearchRuntime,
    Arc<Mutex<BackendOrchestrator>>,
) {
    let event_log = LocalEventLog::default();
    let event_publisher = BroadcastEventPublisher::new(event_log, 8);
    let search_runtime = LocalSearchRuntime::default();
    search_runtime.set_fixture(
        query,
        vec![Song {
            id: String::from("youtube:worker-fixture"),
            title: String::from("Worker Fixture Song"),
            channel_name: String::from("Fixture Channel"),
            duration_ms: 123_000,
            source_url: String::from("https://example.com/worker-fixture"),
        }],
    );

    let mut orchestrator = Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        event_publisher.clone(),
        test_playback_adapter(),
        LocalSearchAdapter::new(search_runtime.clone()),
    );
    orchestrator
        .hydrate_audio_settings(AudioSettings::default())
        .unwrap();

    (
        event_publisher,
        search_runtime,
        Arc::new(Mutex::new(orchestrator)),
    )
}

#[cfg(test)]
mod tests {
    use super::test_state;
    use crate::http::AppState;

    #[test]
    fn test_state_returns_http_app_state() {
        let _: AppState = test_state();
    }
}
