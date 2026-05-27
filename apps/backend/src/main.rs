use std::env;
use std::net::SocketAddr;
use std::sync::Arc;

use nocturne_api::ApiVersion;
use nocturne_core::{
    CoreError, NocturneCore, Orchestrator, SystemErrorSeverity as CoreSystemErrorSeverity,
};
use nocturne_domain::AudioSettings;
use nocturne_infrastructure::{
    BroadcastEventPublisher, InfrastructureProfile, LocalAudioSettingsStore, LocalClock,
    LocalEventLog, LocalIdGenerator, LocalPlaybackAdapter, LocalSearchAdapter, LocalSearchRuntime,
    LocalYtDlpManager, LocalYtDlpSettingsStore, SharedPlaybackState,
};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

mod http;
mod mapping;
#[cfg(test)]
mod test_support;
mod workers;

use crate::http::{AppState, app_router};
use crate::workers::{
    spawn_playback_event_bridge, spawn_search_worker, spawn_yt_dlp_update_worker,
};

type BackendOrchestrator = Orchestrator<
    LocalClock,
    LocalIdGenerator,
    BroadcastEventPublisher,
    LocalPlaybackAdapter,
    LocalSearchAdapter,
>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let core = NocturneCore::new();
    let event_log = LocalEventLog::default();
    let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 128);
    let playback_state = SharedPlaybackState::default();
    let (playback_event_tx, playback_event_rx) = mpsc::unbounded_channel();
    let yt_dlp_manager =
        LocalYtDlpManager::new(reqwest::Client::new(), LocalYtDlpSettingsStore::new()?);
    yt_dlp_manager.prepare()?;
    let search_runtime = LocalSearchRuntime::with_yt_dlp(yt_dlp_manager.clone());
    let settings_store = Arc::new(Mutex::new(LocalAudioSettingsStore::new()?));
    let initial_audio_settings = {
        let store = settings_store.lock().await;
        match store.load() {
            Ok(settings) => settings,
            Err(error) => {
                eprintln!("failed to load audio settings, falling back to defaults: {error}");
                AudioSettings::default()
            }
        }
    };

    let mut orchestrator = Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        event_publisher.clone(),
        LocalPlaybackAdapter::with_yt_dlp(
            playback_state.clone(),
            playback_event_tx.clone(),
            yt_dlp_manager.clone(),
        ),
        LocalSearchAdapter::new(search_runtime.clone()),
    );
    orchestrator.set_backend_version(Some(env!("CARGO_PKG_VERSION")));
    orchestrator.set_yt_dlp_version(yt_dlp_manager.current_version());
    orchestrator.hydrate_audio_settings(initial_audio_settings)?;

    let snapshot = orchestrator.snapshot();
    let orchestrator = Arc::new(Mutex::new(orchestrator));
    spawn_yt_dlp_update_worker(orchestrator.clone(), yt_dlp_manager.clone());
    spawn_search_worker(orchestrator.clone(), search_runtime.clone());
    spawn_playback_event_bridge(orchestrator.clone(), playback_event_rx);
    let state = AppState {
        orchestrator,
        events: event_publisher.clone(),
        settings_store,
    };
    let app = app_router(state.clone());

    let bind_addr = validate_bind_addr(backend_bind_addr()?)?;
    let listener = TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;

    println!(
        "Nocturne backend ready (api {}, profile {}, infra {}, queue {}, search_jobs {}, events {}, listening http://{})",
        ApiVersion::V1,
        core.workspace_profile(),
        InfrastructureProfile::Local,
        snapshot.queue.len(),
        snapshot.search_jobs.len(),
        event_log.len(),
        local_addr,
    );

    axum::serve(listener, app).await?;
    Ok(())
}

fn report_playback_command_error(orchestrator: &mut BackendOrchestrator, error: &CoreError) {
    let CoreError::Port { code, .. } = error else {
        return;
    };

    if let Err(report_error) = orchestrator.emit_system_error(
        code.clone(),
        crate::workers::user_message_for_playback_failure(code),
        CoreSystemErrorSeverity::Error,
    ) {
        eprintln!("failed to emit playback system error for {code}: {report_error}");
    }
}

fn backend_bind_addr() -> Result<SocketAddr, Box<dyn std::error::Error>> {
    let addr: SocketAddr = env::var("NOCTURNE_BACKEND_ADDR")
        .unwrap_or_else(|_| String::from("127.0.0.1:0"))
        .parse()?;

    if !addr.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("NOCTURNE_BACKEND_ADDR must use a loopback address, got {addr}"),
        )
        .into());
    }

    Ok(addr)
}

fn validate_bind_addr(addr: SocketAddr) -> Result<SocketAddr, Box<dyn std::error::Error>> {
    if addr.ip().is_loopback() {
        Ok(addr)
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!("NOCTURNE_BACKEND_ADDR must stay on loopback, got {addr}"),
        )
        .into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    use nocturne_core::{CoreEvent, SystemErrorSeverity as CoreSeverity};
    use nocturne_domain::{AudioSettings, Song};
    use nocturne_infrastructure::{
        BroadcastEventPublisher, LocalClock, LocalEventLog, LocalIdGenerator, LocalSearchAdapter,
        LocalSearchRuntime, PlaybackWorkerEvent,
    };
    use tokio::sync::Mutex;
    use tokio::sync::mpsc;

    use crate::test_support::{current_session_id, test_playback_adapter};
    use crate::workers::{process_pending_search_jobs, spawn_playback_event_bridge};

    #[test]
    fn validate_bind_addr_rejects_non_loopback_addresses() {
        let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        assert!(validate_bind_addr(addr).is_err());

        let loopback_addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(validate_bind_addr(loopback_addr).unwrap(), loopback_addr);
    }

    #[tokio::test]
    async fn worker_emits_system_error_for_missing_yt_dlp() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
        let search_runtime = LocalSearchRuntime::default();
        search_runtime.set_failure(
            "missing provider",
            "yt_dlp_missing",
            "hidden internal detail",
        );
        let orchestrator = Arc::new(Mutex::new(Orchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher.clone(),
            test_playback_adapter(),
            LocalSearchAdapter::new(search_runtime.clone()),
        )));

        {
            let mut locked = orchestrator.lock().await;
            locked.submit_search("missing provider").unwrap();
        }
        process_pending_search_jobs(&orchestrator, &search_runtime).await;

        let snapshot = event_log.snapshot();
        let has_system_error = snapshot
            .iter()
            .any(|event| matches!(event.data, CoreEvent::SystemError(_)));
        assert!(has_system_error);
    }

    #[test]
    fn playback_port_errors_emit_user_visible_system_errors() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
        let mut orchestrator = Orchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher,
            test_playback_adapter(),
            LocalSearchAdapter::new(LocalSearchRuntime::default()),
        );

        report_playback_command_error(
            &mut orchestrator,
            &CoreError::Port {
                code: String::from("audio_output_unavailable"),
                message: String::from("hidden detail"),
            },
        );

        let snapshot = event_log.snapshot();
        let system_error = snapshot
            .iter()
            .find_map(|event| match &event.data {
                CoreEvent::SystemError(system_error) => Some(system_error),
                _ => None,
            })
            .expect("expected a system.error event");

        assert_eq!(system_error.code, "audio_output_unavailable");
        assert_eq!(
            system_error.message,
            "Audio output is unavailable on the backend."
        );
        assert_eq!(system_error.severity, CoreSeverity::Error);
    }

    #[tokio::test]
    async fn interrupted_playback_event_stops_playback_and_emits_system_error() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
        let mut orchestrator = Orchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher,
            test_playback_adapter(),
            LocalSearchAdapter::new(LocalSearchRuntime::default()),
        );
        orchestrator
            .hydrate_audio_settings(AudioSettings::default())
            .unwrap();

        let receipt = orchestrator
            .enqueue_song(Song {
                id: String::from("youtube:interrupted-song"),
                title: String::from("Interrupted Song"),
                channel_name: String::from("Fixture Channel"),
                duration_ms: 123_000,
                source_url: String::from("https://example.com/interrupted-song"),
            })
            .unwrap();
        let queue_item_id = receipt.queue_item_id.expect("expected queued item id");
        let playback_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&playback_session_id, &queue_item_id, 321)
            .unwrap();

        let orchestrator = Arc::new(Mutex::new(orchestrator));
        let (playback_event_tx, playback_event_rx) = mpsc::unbounded_channel();
        spawn_playback_event_bridge(orchestrator.clone(), playback_event_rx);

        playback_event_tx
            .send(PlaybackWorkerEvent::Interrupted {
                playback_session_id,
                queue_item_id,
                code: String::from("audio_output_changed"),
                message: String::from("recovery failed after device loss"),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let locked = orchestrator.lock().await;
        assert_eq!(
            locked.state().playback().state,
            nocturne_domain::PlaybackStatus::Stopped
        );
        assert!(locked.state().playback().current_queue_item_id.is_none());
        drop(locked);

        let snapshot = event_log.snapshot();
        let system_error = snapshot
            .iter()
            .rev()
            .find_map(|event| match &event.data {
                CoreEvent::SystemError(system_error) => Some(system_error),
                _ => None,
            })
            .expect("expected a system.error event");

        assert_eq!(system_error.code, "audio_output_changed");
        assert_eq!(
            system_error.message,
            "Audio output changed. Playback stopped, and the next play will use the current default device."
        );
        assert_eq!(system_error.severity, CoreSeverity::Error);
    }

    #[tokio::test]
    async fn interrupted_stream_error_emits_generic_audio_output_message() {
        let event_log = LocalEventLog::default();
        let event_publisher = BroadcastEventPublisher::new(event_log.clone(), 8);
        let mut orchestrator = Orchestrator::new(
            LocalClock::new(),
            LocalIdGenerator::new(),
            event_publisher,
            test_playback_adapter(),
            LocalSearchAdapter::new(LocalSearchRuntime::default()),
        );
        orchestrator
            .hydrate_audio_settings(AudioSettings::default())
            .unwrap();

        let receipt = orchestrator
            .enqueue_song(Song {
                id: String::from("youtube:interrupted-stream-song"),
                title: String::from("Interrupted Stream Song"),
                channel_name: String::from("Fixture Channel"),
                duration_ms: 123_000,
                source_url: String::from("https://example.com/interrupted-stream-song"),
            })
            .unwrap();
        let queue_item_id = receipt.queue_item_id.expect("expected queued item id");
        let playback_session_id = current_session_id(&orchestrator);
        orchestrator
            .confirm_playback_started(&playback_session_id, &queue_item_id, 321)
            .unwrap();

        let orchestrator = Arc::new(Mutex::new(orchestrator));
        let (playback_event_tx, playback_event_rx) = mpsc::unbounded_channel();
        spawn_playback_event_bridge(orchestrator.clone(), playback_event_rx);

        playback_event_tx
            .send(PlaybackWorkerEvent::Interrupted {
                playback_session_id,
                queue_item_id,
                code: String::from("audio_output_stream_lost"),
                message: String::from("stream error: device disappeared"),
            })
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;

        let snapshot = event_log.snapshot();
        let system_error = snapshot
            .iter()
            .rev()
            .find_map(|event| match &event.data {
                CoreEvent::SystemError(system_error) => Some(system_error),
                _ => None,
            })
            .expect("expected a system.error event");

        assert_eq!(system_error.code, "audio_output_stream_lost");
        assert_eq!(
            system_error.message,
            "Playback stopped because the backend lost its audio output stream. Try playing again."
        );
        assert_eq!(system_error.severity, CoreSeverity::Error);
    }
}
