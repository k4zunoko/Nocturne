use std::env;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use nocturne_api::ApiVersion;
use nocturne_core::{NocturneCore, Orchestrator};
use nocturne_domain::AudioSettings;
use nocturne_infrastructure::{
    BroadcastEventPublisher, InfrastructureProfile, LocalAudioSettingsStore, LocalClock,
    LocalEventLog, LocalIdGenerator, LocalPlaybackAdapter, LocalSearchAdapter, LocalSearchRuntime,
    LocalYtDlpManager, LocalYtDlpSettingsStore, PlaybackWorkerEvent, SharedPlaybackState,
};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::sync::mpsc;

mod http;
mod mapping;
mod playback_errors;
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
    let initial_audio_settings = load_initial_audio_settings(&settings_store).await;
    let orchestrator = build_orchestrator(
        event_publisher.clone(),
        playback_state,
        playback_event_tx,
        yt_dlp_manager.clone(),
        search_runtime.clone(),
        initial_audio_settings,
    )?;

    spawn_yt_dlp_update_worker(orchestrator.clone(), yt_dlp_manager.clone());
    spawn_search_worker(orchestrator.clone(), search_runtime);
    spawn_playback_event_bridge(orchestrator.clone(), playback_event_rx);
    let app = app_router(AppState {
        orchestrator: orchestrator.clone(),
        events: event_publisher.clone(),
        settings_store,
    });

    serve_backend(core, app, event_log, orchestrator).await
}

async fn load_initial_audio_settings(
    settings_store: &Arc<Mutex<LocalAudioSettingsStore>>,
) -> AudioSettings {
    let store = settings_store.lock().await;
    match store.load() {
        Ok(settings) => settings,
        Err(error) => {
            eprintln!("failed to load audio settings, falling back to defaults: {error}");
            AudioSettings::default()
        }
    }
}

fn build_orchestrator(
    event_publisher: BroadcastEventPublisher,
    playback_state: SharedPlaybackState,
    playback_event_tx: mpsc::UnboundedSender<PlaybackWorkerEvent>,
    yt_dlp_manager: LocalYtDlpManager,
    search_runtime: LocalSearchRuntime,
    initial_audio_settings: AudioSettings,
) -> Result<Arc<Mutex<BackendOrchestrator>>, Box<dyn std::error::Error>> {
    let mut orchestrator = Orchestrator::new(
        LocalClock::new(),
        LocalIdGenerator::new(),
        event_publisher,
        LocalPlaybackAdapter::with_yt_dlp(
            playback_state,
            playback_event_tx,
            yt_dlp_manager.clone(),
        ),
        LocalSearchAdapter::new(search_runtime),
    );
    orchestrator.set_backend_version(Some(env!("CARGO_PKG_VERSION")));
    orchestrator.set_yt_dlp_version(yt_dlp_manager.current_version());
    orchestrator.hydrate_audio_settings(initial_audio_settings)?;
    Ok(Arc::new(Mutex::new(orchestrator)))
}

async fn serve_backend(
    core: NocturneCore,
    app: Router,
    event_log: LocalEventLog,
    orchestrator: Arc<Mutex<BackendOrchestrator>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let bind_addr = validate_bind_addr(backend_bind_addr()?)?;
    let listener = TcpListener::bind(bind_addr).await?;
    let local_addr = listener.local_addr()?;
    let (queue_len, search_jobs_len) = {
        let mut orchestrator = orchestrator.lock().await;
        let snapshot = orchestrator.snapshot();
        (snapshot.queue.len(), snapshot.search_jobs.len())
    };

    println!(
        "Nocturne backend ready (api {}, profile {}, infra {}, queue {}, search_jobs {}, events {}, listening http://{})",
        ApiVersion::V1,
        core.workspace_profile(),
        InfrastructureProfile::Local,
        queue_len,
        search_jobs_len,
        event_log.len(),
        local_addr,
    );

    axum::serve(listener, app).await?;
    Ok(())
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

    use nocturne_domain::AudioSettings;
    use nocturne_infrastructure::LocalAudioSettingsStore;
    use tokio::sync::Mutex;

    #[test]
    fn validate_bind_addr_rejects_non_loopback_addresses() {
        let addr: SocketAddr = "0.0.0.0:8080".parse().unwrap();
        assert!(validate_bind_addr(addr).is_err());

        let loopback_addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(validate_bind_addr(loopback_addr).unwrap(), loopback_addr);
    }

    #[tokio::test]
    async fn load_initial_audio_settings_falls_back_to_defaults_when_store_load_fails() {
        let settings_store = Arc::new(Mutex::new(LocalAudioSettingsStore::from_path(
            std::env::current_dir().unwrap(),
        )));

        let loaded = load_initial_audio_settings(&settings_store).await;

        assert_eq!(loaded, AudioSettings::default());
    }
}
