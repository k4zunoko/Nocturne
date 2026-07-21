use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use kira::sound::streaming::{StreamingSoundData, StreamingSoundHandle};
use kira::sound::{FromFileError, PlaybackState};
use kira::{AudioManager, AudioManagerSettings, DefaultBackend, Decibels, Tween};
use nocturne_core::{PlaybackPort, PortError};
use nocturne_domain::{AudioSettings, QueueItem};
use symphonia::core::io::MediaSource;
use stream_download::process::{ProcessStreamParams, YtDlpCommand};
use stream_download::storage::temp::TempStorageProvider;
use stream_download::{Settings, StreamDownload};
use tokio::runtime::{Builder as RuntimeBuilder, Runtime};
use tokio::sync::mpsc::UnboundedSender;

use crate::recover_lock;
use crate::yt_dlp::LocalYtDlpManager;

const YT_DLP_AUDIO_FORMAT: &str = "140/139/bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]/bestaudio[ext=mp3]/bestaudio[acodec^=mp3]/best[ext=mp4]/best";
const YT_DLP_PROCESS_AUDIO_FORMAT: &str = "140/139";
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(250);
const PLAYBACK_URL_CACHE_TTL: Duration = Duration::from_secs(15 * 60);
const PLAYBACK_URL_CACHE_LIMIT: usize = 64;
const PLAYBACK_STREAM_PREFETCH_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackCommand {
    Start {
        playback_session_id: String,
        queue_item_id: String,
        position_ms: u64,
    },
    Pause,
    Resume,
    Stop,
    Seek {
        position_ms: u64,
    },
    SetVolume {
        volume_percent: u8,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackWorkerEvent {
    Started {
        playback_session_id: String,
        queue_item_id: String,
        position_ms: u64,
    },
    Progress {
        playback_session_id: String,
        position_ms: u64,
        paused: bool,
    },
    Ended {
        playback_session_id: String,
        queue_item_id: String,
        position_ms: u64,
    },
    StartFailed {
        playback_session_id: String,
        queue_item_id: String,
        code: String,
        message: String,
    },
    Interrupted {
        playback_session_id: String,
        queue_item_id: String,
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPlaybackSnapshot {
    pub current_item: Option<QueueItem>,
    pub playback_session_id: Option<String>,
    pub position_ms: u64,
    pub paused: bool,
    pub volume_percent: u8,
    pub history: Vec<PlaybackCommand>,
}

impl Default for LocalPlaybackSnapshot {
    fn default() -> Self {
        Self {
            current_item: None,
            playback_session_id: None,
            position_ms: 0,
            paused: false,
            volume_percent: AudioSettings::DEFAULT_VOLUME_PERCENT,
            history: Vec::new(),
        }
    }
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

#[derive(Debug)]
pub struct LocalPlaybackAdapter {
    commands: SyncSender<WorkerCommand>,
    worker: Option<JoinHandle<()>>,
}

impl LocalPlaybackAdapter {
    #[must_use]
    pub fn new(state: SharedPlaybackState, event_tx: UnboundedSender<PlaybackWorkerEvent>) -> Self {
        Self::with_yt_dlp(state, event_tx, LocalYtDlpManager::default())
    }

    #[must_use]
    pub fn with_yt_dlp(
        state: SharedPlaybackState,
        event_tx: UnboundedSender<PlaybackWorkerEvent>,
        yt_dlp: LocalYtDlpManager,
    ) -> Self {
        let (commands, receiver) = mpsc::sync_channel(8);
        let worker_state = state.clone();
        let worker =
            thread::spawn(move || playback_worker_loop(receiver, worker_state, event_tx, yt_dlp));

        Self {
            commands,
            worker: Some(worker),
        }
    }

    fn send_command(&mut self, command: WorkerCommand) -> Result<(), PortError> {
        self.commands.send(command).map_err(|_| {
            PortError::new(
                "playback_worker_unavailable",
                "playback worker is no longer available",
            )
        })
    }

    fn request(
        &mut self,
        build: impl FnOnce(SyncSender<Result<(), PortError>>) -> WorkerCommand,
    ) -> Result<(), PortError> {
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        self.send_command(build(reply_tx))?;
        reply_rx.recv().map_err(|_| {
            PortError::new(
                "playback_worker_unavailable",
                "playback worker did not respond",
            )
        })?
    }
}

impl PlaybackPort for LocalPlaybackAdapter {
    fn start(
        &mut self,
        item: &QueueItem,
        playback_session_id: &str,
        position_ms: u64,
    ) -> Result<(), PortError> {
        self.send_command(WorkerCommand::Start {
            item: item.clone(),
            playback_session_id: playback_session_id.to_owned(),
            position_ms,
        })
    }

    fn set_volume(&mut self, gain: f32) -> Result<(), PortError> {
        let volume_percent = gain_to_percent(gain);
        self.request(|reply| WorkerCommand::SetVolume {
            volume_percent,
            reply,
        })
    }

    fn pause(&mut self) -> Result<(), PortError> {
        self.request(|reply| WorkerCommand::Pause { reply })
    }

    fn resume(&mut self) -> Result<(), PortError> {
        self.request(|reply| WorkerCommand::Resume { reply })
    }

    fn stop(&mut self) -> Result<(), PortError> {
        self.request(|reply| WorkerCommand::Stop { reply })
    }

    fn seek(&mut self, position_ms: u64) -> Result<(), PortError> {
        self.request(|reply| WorkerCommand::Seek { position_ms, reply })
    }
}

impl Drop for LocalPlaybackAdapter {
    fn drop(&mut self) {
        let _ = self.send_command(WorkerCommand::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

enum WorkerCommand {
    Start {
        item: QueueItem,
        playback_session_id: String,
        position_ms: u64,
    },
    Pause {
        reply: SyncSender<Result<(), PortError>>,
    },
    Resume {
        reply: SyncSender<Result<(), PortError>>,
    },
    Stop {
        reply: SyncSender<Result<(), PortError>>,
    },
    Seek {
        position_ms: u64,
        reply: SyncSender<Result<(), PortError>>,
    },
    SetVolume {
        volume_percent: u8,
        reply: SyncSender<Result<(), PortError>>,
    },
    Shutdown,
}

enum ActivePlayback {
    Simulated {
        playback_session_id: String,
        anchor_at: Instant,
        base_position_ms: u64,
        duration_ms: u64,
        paused: bool,
    },
    Kira {
        playback_session_id: String,
        queue_item_id: String,
        handle: StreamingSoundHandle<FromFileError>,
    },
}

#[derive(Debug, Clone)]
struct ResolvedPlaybackUrl {
    playback_url: String,
    resolved_at: Instant,
}

fn playback_worker_loop(
    receiver: mpsc::Receiver<WorkerCommand>,
    state: SharedPlaybackState,
    event_tx: UnboundedSender<PlaybackWorkerEvent>,
    yt_dlp: LocalYtDlpManager,
) {
    let mut active = None;
    let mut manager: Option<AudioManager<DefaultBackend>> = None;
    let mut playback_runtime = None;
    let mut resolved_playback_urls = HashMap::new();
    let mut volume_percent = AudioSettings::DEFAULT_VOLUME_PERCENT;

    loop {
        match receiver.recv_timeout(WORKER_POLL_INTERVAL) {
            Ok(WorkerCommand::Start {
                item,
                playback_session_id,
                position_ms,
            }) => {
                handle_start(
                    &state,
                    &mut active,
                    &mut manager,
                    &mut playback_runtime,
                    &mut resolved_playback_urls,
                    &yt_dlp,
                    &event_tx,
                    item,
                    playback_session_id,
                    position_ms,
                    volume_percent,
                );
            }
            Ok(WorkerCommand::SetVolume {
                volume_percent: next_volume_percent,
                reply,
            }) => {
                volume_percent = next_volume_percent;
                let _ = reply.send(handle_set_volume(&state, &mut active, volume_percent));
            }
            Ok(WorkerCommand::Pause { reply }) => {
                let _ = reply.send(handle_pause(&state, &mut active));
            }
            Ok(WorkerCommand::Resume { reply }) => {
                let _ = reply.send(handle_resume(&state, &mut active));
            }
            Ok(WorkerCommand::Stop { reply }) => {
                let _ = reply.send(handle_stop(&state, &mut active));
            }
            Ok(WorkerCommand::Seek { position_ms, reply }) => {
                let _ = reply.send(handle_seek(&state, &mut active, position_ms));
            }
            Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                clear_active_playback(&state, &mut active, false);
                break;
            }
            Err(RecvTimeoutError::Timeout) => poll_active_playback(&state, &mut active, &event_tx),
        }
    }
}

fn handle_start(
    state: &SharedPlaybackState,
    active: &mut Option<ActivePlayback>,
    manager: &mut Option<AudioManager<DefaultBackend>>,
    playback_runtime: &mut Option<Runtime>,
    resolved_playback_urls: &mut HashMap<String, ResolvedPlaybackUrl>,
    yt_dlp: &LocalYtDlpManager,
    event_tx: &UnboundedSender<PlaybackWorkerEvent>,
    item: QueueItem,
    playback_session_id: String,
    position_ms: u64,
    volume_percent: u8,
) {
    clear_active_playback(state, active, false);
    let load_started_at = Instant::now();

    let next_active = if should_simulate_source(&item.song.source_url) {
        ActivePlayback::Simulated {
            playback_session_id: playback_session_id.clone(),
            anchor_at: Instant::now(),
            base_position_ms: position_ms,
            duration_ms: item.song.duration_ms,
            paused: false,
        }
    } else {
        match open_real_playback(
            manager,
            &item,
            &playback_session_id,
            position_ms,
            playback_runtime,
            resolved_playback_urls,
            yt_dlp,
            volume_percent,
        ) {
            Ok(active_playback) => active_playback,
            Err(error) => {
                eprintln!(
                    "[nocturne][playback] start failed queue_item_id={} session_id={} elapsed_ms={} code={} message={}",
                    item.id,
                    playback_session_id,
                    load_started_at.elapsed().as_millis(),
                    error.code(),
                    error.message(),
                );
                let mut snapshot = recover_lock(&state.inner);
                snapshot.current_item = None;
                snapshot.playback_session_id = None;
                snapshot.position_ms = 0;
                snapshot.paused = false;
                let _ = event_tx.send(PlaybackWorkerEvent::StartFailed {
                    playback_session_id,
                    queue_item_id: item.id.clone(),
                    code: error.code().to_owned(),
                    message: error.message().to_owned(),
                });
                return;
            }
        }
    };

    {
        let mut snapshot = recover_lock(&state.inner);
        snapshot.current_item = Some(item.clone());
        snapshot.playback_session_id = Some(playback_session_id.clone());
        snapshot.position_ms = position_ms;
        snapshot.paused = false;
        snapshot.volume_percent = volume_percent;
        snapshot.history.push(PlaybackCommand::Start {
            playback_session_id: playback_session_id.clone(),
            queue_item_id: item.id.clone(),
            position_ms,
        });
    }

    let _ = event_tx.send(PlaybackWorkerEvent::Started {
        playback_session_id,
        queue_item_id: item.id.clone(),
        position_ms,
    });

    eprintln!(
        "[nocturne][playback] start completed queue_item_id={} elapsed_ms={}",
        item.id,
        load_started_at.elapsed().as_millis(),
    );

    *active = Some(next_active);
}

fn handle_pause(
    state: &SharedPlaybackState,
    active: &mut Option<ActivePlayback>,
) -> Result<(), PortError> {
    match active.as_mut() {
        Some(ActivePlayback::Simulated {
            anchor_at,
            base_position_ms,
            duration_ms,
            paused,
            ..
        }) => {
            if !*paused {
                *base_position_ms =
                    simulated_position_ms(*anchor_at, *base_position_ms, *duration_ms);
                *paused = true;
            }
        }
        Some(ActivePlayback::Kira { handle, .. }) => {
            handle.pause(Tween::default());
        }
        None => {}
    }

    let mut snapshot = recover_lock(&state.inner);
    snapshot.paused = true;
    snapshot.history.push(PlaybackCommand::Pause);
    Ok(())
}

fn handle_set_volume(
    state: &SharedPlaybackState,
    active: &mut Option<ActivePlayback>,
    volume_percent: u8,
) -> Result<(), PortError> {
    if let Some(ActivePlayback::Kira { handle, .. }) = active.as_mut() {
        handle.set_volume(percent_to_decibels(volume_percent), Tween::default());
    }

    let mut snapshot = recover_lock(&state.inner);
    snapshot.volume_percent = volume_percent;
    snapshot
        .history
        .push(PlaybackCommand::SetVolume { volume_percent });
    Ok(())
}

fn handle_resume(
    state: &SharedPlaybackState,
    active: &mut Option<ActivePlayback>,
) -> Result<(), PortError> {
    match active.as_mut() {
        Some(ActivePlayback::Simulated {
            anchor_at, paused, ..
        }) => {
            if *paused {
                *anchor_at = Instant::now();
                *paused = false;
            }
        }
        Some(ActivePlayback::Kira { handle, .. }) => {
            handle.resume(Tween::default());
        }
        None => {}
    }

    let mut snapshot = recover_lock(&state.inner);
    snapshot.paused = false;
    snapshot.history.push(PlaybackCommand::Resume);
    Ok(())
}

fn handle_stop(
    state: &SharedPlaybackState,
    active: &mut Option<ActivePlayback>,
) -> Result<(), PortError> {
    clear_active_playback(state, active, true);
    Ok(())
}

fn handle_seek(
    state: &SharedPlaybackState,
    active: &mut Option<ActivePlayback>,
    position_ms: u64,
) -> Result<(), PortError> {
    match active.as_mut() {
        Some(ActivePlayback::Simulated {
            anchor_at,
            base_position_ms,
            duration_ms,
            ..
        }) => {
            *base_position_ms = position_ms.min(*duration_ms);
            *anchor_at = Instant::now();
        }
        Some(ActivePlayback::Kira { handle, .. }) => {
            handle.seek_to(position_ms as f64 / 1000.0);
        }
        None => {}
    }

    let mut snapshot = recover_lock(&state.inner);
    snapshot.position_ms = position_ms;
    snapshot.history.push(PlaybackCommand::Seek { position_ms });
    Ok(())
}

/// Derives the `(paused, finished)` flags for the shared snapshot / worker
/// events from a Kira playback state. Only a fully `Stopped` sound counts as
/// finished, so a seek/restart (which keeps the sound Playing/Paused) is never
/// mistaken for a natural end.
fn derive_playback_flags(state: PlaybackState) -> (bool, bool) {
    match state {
        PlaybackState::Playing => (false, false),
        PlaybackState::Stopped => (false, true),
        _ => (true, false),
    }
}

fn poll_active_playback(
    state: &SharedPlaybackState,
    active: &mut Option<ActivePlayback>,
    event_tx: &UnboundedSender<PlaybackWorkerEvent>,
) {
    let Some(active_playback) = active.as_ref() else {
        return;
    };

    let (playback_session_id, queue_item_id, position_ms, paused, finished) = match active_playback
    {
        ActivePlayback::Simulated {
            playback_session_id,
            anchor_at,
            base_position_ms,
            duration_ms,
            paused,
        } => {
            let position_ms = if *paused {
                *base_position_ms
            } else {
                simulated_position_ms(*anchor_at, *base_position_ms, *duration_ms)
            };
            let finished = position_ms >= *duration_ms;
            (
                playback_session_id.clone(),
                recover_lock(&state.inner)
                    .current_item
                    .as_ref()
                    .map(|item| item.id.clone())
                    .unwrap_or_default(),
                position_ms.min(*duration_ms),
                *paused,
                finished,
            )
        }
        ActivePlayback::Kira {
            playback_session_id,
            queue_item_id,
            handle,
        } => {
            let (paused, finished) = derive_playback_flags(handle.state());
            let position_ms = (handle.position() * 1000.0) as u64;
            (
                playback_session_id.clone(),
                queue_item_id.clone(),
                position_ms,
                paused,
                finished,
            )
        }
    };

    {
        let mut snapshot = recover_lock(&state.inner);
        snapshot.position_ms = position_ms;
        snapshot.paused = paused;
    }

    let _ = event_tx.send(PlaybackWorkerEvent::Progress {
        playback_session_id: playback_session_id.clone(),
        position_ms,
        paused,
    });

    if finished {
        let _ = event_tx.send(PlaybackWorkerEvent::Ended {
            playback_session_id,
            queue_item_id,
            position_ms,
        });
        clear_finished_playback(state, position_ms);
        *active = None;
    }
}

fn clear_finished_playback(state: &SharedPlaybackState, position_ms: u64) {
    let mut snapshot = recover_lock(&state.inner);
    snapshot.current_item = None;
    snapshot.playback_session_id = None;
    snapshot.position_ms = position_ms;
    snapshot.paused = false;
}

fn clear_active_playback(
    state: &SharedPlaybackState,
    active: &mut Option<ActivePlayback>,
    record_stop: bool,
) {
    drop_active_output(active);

    let mut snapshot = recover_lock(&state.inner);
    snapshot.current_item = None;
    snapshot.playback_session_id = None;
    snapshot.position_ms = 0;
    snapshot.paused = false;
    if record_stop {
        snapshot.history.push(PlaybackCommand::Stop);
    }
}

fn drop_active_output(active: &mut Option<ActivePlayback>) {
    if let Some(ActivePlayback::Kira { mut handle, .. }) = active.take() {
        handle.stop(Tween::default());
    }
}

fn simulated_position_ms(anchor_at: Instant, base_position_ms: u64, duration_ms: u64) -> u64 {
    base_position_ms
        .saturating_add(anchor_at.elapsed().as_millis() as u64)
        .min(duration_ms)
}

fn open_real_playback(
    manager: &mut Option<AudioManager<DefaultBackend>>,
    item: &QueueItem,
    playback_session_id: &str,
    position_ms: u64,
    playback_runtime: &mut Option<Runtime>,
    resolved_playback_urls: &mut HashMap<String, ResolvedPlaybackUrl>,
    yt_dlp: &LocalYtDlpManager,
    volume_percent: u8,
) -> Result<ActivePlayback, PortError> {
    let load_started_at = Instant::now();
    let manager = audio_manager_instance(manager)?;
    let runtime = playback_runtime_instance(playback_runtime)?;

    let reader = if is_youtube_url(&item.song.source_url) {
        match runtime.block_on(open_youtube_playback_stream(&item.song.source_url, yt_dlp)) {
            Ok(reader) => reader,
            Err(_) => {
                let playback_url =
                    resolve_playback_url(&item.song.source_url, resolved_playback_urls, yt_dlp)?;
                runtime.block_on(open_http_playback_stream(&playback_url))?
            }
        }
    } else {
        let playback_url =
            resolve_playback_url(&item.song.source_url, resolved_playback_urls, yt_dlp)?;
        runtime.block_on(open_http_playback_stream(&playback_url))?
    };

    let byte_len = reader.content_length();
    let source = StreamMediaSource {
        inner: reader,
        byte_len,
    };

    let sound_data = StreamingSoundData::from_media_source(source)
        .map_err(|error| {
            PortError::new(
                "playback_decode_failed",
                format!("failed to decode the playback stream: {error}"),
            )
        })?
        .start_position(position_ms as f64 / 1000.0)
        .volume(percent_to_decibels(volume_percent));

    let handle = manager.play(sound_data).map_err(|error| {
        PortError::new(
            "playback_start_failed",
            format!("failed to start playback: {error}"),
        )
    })?;

    eprintln!(
        "[nocturne][playback] open_real_playback queue_item_id={} session_id={} total_ms={}",
        item.id,
        playback_session_id,
        load_started_at.elapsed().as_millis(),
    );

    Ok(ActivePlayback::Kira {
        playback_session_id: playback_session_id.to_owned(),
        queue_item_id: item.id.clone(),
        handle,
    })
}

fn audio_manager_instance(
    manager: &mut Option<AudioManager<DefaultBackend>>,
) -> Result<&mut AudioManager<DefaultBackend>, PortError> {
    if manager.is_none() {
        *manager = Some(
            AudioManager::<DefaultBackend>::new(AudioManagerSettings::default()).map_err(
                |error| {
                    PortError::new(
                        "audio_output_unavailable",
                        format!("failed to initialize the audio output: {error}"),
                    )
                },
            )?,
        );
    }

    Ok(manager
        .as_mut()
        .expect("audio manager should be initialized"))
}

/// Adapts a `stream-download` reader into a symphonia `MediaSource` so Kira can
/// decode and seek streamed audio without buffering the whole file up front.
struct StreamMediaSource {
    inner: StreamDownload<TempStorageProvider>,
    byte_len: Option<u64>,
}

impl Read for StreamMediaSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Seek for StreamMediaSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl MediaSource for StreamMediaSource {
    fn is_seekable(&self) -> bool {
        // Only advertise seeking when we know the total length. Some sources
        // (notably the yt-dlp process stream) have no content length; claiming
        // seekability there makes symphonia attempt an end-relative seek during
        // probing, which fails on a length-unknown stream. Matches the previous
        // rodio decoder, which only enabled seeking when a byte length was known.
        self.byte_len.is_some()
    }

    fn byte_len(&self) -> Option<u64> {
        self.byte_len
    }
}

fn playback_runtime_instance(
    playback_runtime: &mut Option<Runtime>,
) -> Result<&Runtime, PortError> {
    if playback_runtime.is_none() {
        *playback_runtime = Some(
            RuntimeBuilder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .map_err(|error| {
                    PortError::new(
                        "playback_runtime_failed",
                        format!("failed to build playback runtime: {error}"),
                    )
                })?,
        );
    }

    Ok(playback_runtime
        .as_ref()
        .expect("playback runtime should be initialized"))
}

async fn open_http_playback_stream(
    playback_url: &str,
) -> Result<StreamDownload<TempStorageProvider>, PortError> {
    let stream_url = playback_url.parse().map_err(|error| {
        PortError::new(
            "playback_url_invalid",
            format!("playback URL could not be parsed: {error}"),
        )
    })?;

    StreamDownload::new_http(
        stream_url,
        TempStorageProvider::new(),
        Settings::default().prefetch_bytes(PLAYBACK_STREAM_PREFETCH_BYTES),
    )
    .await
    .map_err(|error| {
        PortError::new(
            "playback_stream_open_failed",
            format!("failed to open the playback stream: {error}"),
        )
    })
}

async fn open_youtube_playback_stream(
    source_url: &str,
    yt_dlp: &LocalYtDlpManager,
) -> Result<StreamDownload<TempStorageProvider>, PortError> {
    let process_params = build_youtube_process_stream_params(source_url, yt_dlp)?;

    StreamDownload::new_process(
        process_params,
        TempStorageProvider::new(),
        Settings::default().prefetch_bytes(PLAYBACK_STREAM_PREFETCH_BYTES),
    )
    .await
    .map_err(|error| {
        PortError::new(
            "playback_stream_open_failed",
            format!("failed to open the playback stream: {error}"),
        )
    })
}

fn build_youtube_process_stream_params(
    source_url: &str,
    yt_dlp: &LocalYtDlpManager,
) -> Result<ProcessStreamParams, PortError> {
    let command = YtDlpCommand::new(source_url)
        .yt_dlp_path(yt_dlp.executable_path())
        .extract_audio(true)
        .format(YT_DLP_PROCESS_AUDIO_FORMAT);
    let params = ProcessStreamParams::new(command).map_err(|error| {
        PortError::new(
            "yt_dlp_spawn_failed",
            format!("failed to spawn yt-dlp playback stream: {error}"),
        )
    })?;

    Ok(params)
}

fn resolve_playback_url(
    source_url: &str,
    resolved_playback_urls: &mut HashMap<String, ResolvedPlaybackUrl>,
    yt_dlp: &LocalYtDlpManager,
) -> Result<String, PortError> {
    if is_youtube_url(source_url) {
        if let Some(playback_url) = cached_playback_url(source_url, resolved_playback_urls) {
            return Ok(playback_url);
        }

        let playback_url = resolve_youtube_playback_url(source_url, yt_dlp)?;
        insert_cached_playback_url(source_url, playback_url.clone(), resolved_playback_urls);
        Ok(playback_url)
    } else {
        Ok(source_url.to_owned())
    }
}

fn cached_playback_url(
    source_url: &str,
    resolved_playback_urls: &mut HashMap<String, ResolvedPlaybackUrl>,
) -> Option<String> {
    prune_cached_playback_urls(resolved_playback_urls);
    resolved_playback_urls
        .get(source_url)
        .map(|cached| cached.playback_url.clone())
}

fn insert_cached_playback_url(
    source_url: &str,
    playback_url: String,
    resolved_playback_urls: &mut HashMap<String, ResolvedPlaybackUrl>,
) {
    prune_cached_playback_urls(resolved_playback_urls);

    if resolved_playback_urls.len() >= PLAYBACK_URL_CACHE_LIMIT
        && let Some(oldest_key) = resolved_playback_urls
            .iter()
            .min_by_key(|(_, cached)| cached.resolved_at)
            .map(|(key, _)| key.clone())
    {
        resolved_playback_urls.remove(&oldest_key);
    }

    resolved_playback_urls.insert(
        source_url.to_owned(),
        ResolvedPlaybackUrl {
            playback_url,
            resolved_at: Instant::now(),
        },
    );
}

fn prune_cached_playback_urls(resolved_playback_urls: &mut HashMap<String, ResolvedPlaybackUrl>) {
    resolved_playback_urls
        .retain(|_, cached| cached.resolved_at.elapsed() <= PLAYBACK_URL_CACHE_TTL);
}

fn resolve_youtube_playback_url(
    source_url: &str,
    yt_dlp: &LocalYtDlpManager,
) -> Result<String, PortError> {
    let output = yt_dlp
        .execute([
            "--get-url",
            "--format",
            YT_DLP_AUDIO_FORMAT,
            "--no-playlist",
            "--no-warnings",
            "--quiet",
            source_url,
        ])
        .map_err(map_yt_dlp_spawn_error)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(PortError::new(
            "audio_source_resolve_failed",
            if stderr.is_empty() {
                String::from("yt-dlp did not return a playable audio stream URL")
            } else {
                format!("yt-dlp failed to resolve a playable audio stream URL: {stderr}")
            },
        ));
    }

    first_non_empty_line(&output.stdout).ok_or_else(|| {
        PortError::new(
            "audio_source_resolve_failed",
            "yt-dlp did not return a playable audio stream URL",
        )
    })
}

fn percent_to_decibels(volume_percent: u8) -> Decibels {
    let gain = f32::from(volume_percent.min(100)) / 100.0;
    if gain <= 0.0 {
        Decibels::SILENCE
    } else {
        Decibels(20.0 * gain.log10())
    }
}

fn gain_to_percent(gain: f32) -> u8 {
    (gain.clamp(0.0, 1.0) * 100.0).round() as u8
}

fn map_yt_dlp_spawn_error(error: std::io::Error) -> PortError {
    let code = if error.kind() == std::io::ErrorKind::NotFound {
        "yt_dlp_missing"
    } else {
        "yt_dlp_spawn_failed"
    };

    PortError::new(code, format!("yt-dlp is unavailable for playback: {error}"))
}

fn first_non_empty_line(stdout: &[u8]) -> Option<String> {
    String::from_utf8_lossy(stdout)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_owned)
}

fn should_simulate_source(source_url: &str) -> bool {
    !is_real_playback_source(source_url)
}

fn is_youtube_url(source_url: &str) -> bool {
    let normalized = source_url.trim().to_ascii_lowercase();
    normalized.contains("youtube.com") || normalized.contains("youtu.be")
}

fn is_real_playback_source(source_url: &str) -> bool {
    let normalized = source_url.trim().to_ascii_lowercase();
    extract_youtube_video_id(&normalized).is_some()
}

fn extract_youtube_video_id(source_url: &str) -> Option<&str> {
    if let Some(index) = source_url.find("youtube.com/watch?v=") {
        let video_id = &source_url[index + "youtube.com/watch?v=".len()..];
        return valid_youtube_video_id(video_id);
    }

    if let Some(index) = source_url.find("youtu.be/") {
        let video_id = &source_url[index + "youtu.be/".len()..];
        return valid_youtube_video_id(video_id);
    }

    None
}

fn valid_youtube_video_id(candidate: &str) -> Option<&str> {
    let video_id = candidate
        .split(['&', '?', '/', '#'])
        .next()
        .unwrap_or_default();

    if video_id.len() == 11
        && video_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        Some(video_id)
    } else {
        None
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
        let (event_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let mut adapter = LocalPlaybackAdapter::new(shared.clone(), event_tx);

        adapter
            .start(&queue_item("queue_item_1"), "playback_session_1", 0)
            .unwrap();
        thread::sleep(Duration::from_millis(20));
        adapter.seek(42_000).unwrap();
        adapter.pause().unwrap();
        adapter.resume().unwrap();

        let snapshot = shared.snapshot();
        assert_eq!(snapshot.position_ms, 42_000);
        assert!(!snapshot.paused);
        assert_eq!(snapshot.history.len(), 4);
        assert_eq!(
            snapshot.playback_session_id.as_deref(),
            Some("playback_session_1")
        );
    }

    #[test]
    fn simulated_sources_can_be_stopped() {
        let shared = SharedPlaybackState::default();
        let (event_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let mut adapter = LocalPlaybackAdapter::new(shared.clone(), event_tx);

        adapter
            .start(&queue_item("queue_item_1"), "playback_session_1", 1_500)
            .unwrap();
        thread::sleep(Duration::from_millis(20));
        adapter.stop().unwrap();

        let snapshot = shared.snapshot();
        assert!(snapshot.current_item.is_none());
        assert_eq!(snapshot.position_ms, 0);
        assert_eq!(snapshot.history.len(), 2);
        assert_eq!(snapshot.playback_session_id, None);
    }

    #[test]
    fn simulated_sources_honor_pause_resume_and_seek() {
        let shared = SharedPlaybackState::default();
        let (event_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let mut adapter = LocalPlaybackAdapter::new(shared.clone(), event_tx);

        adapter
            .start(&queue_item("queue_item_1"), "playback_session_1", 0)
            .unwrap();
        thread::sleep(Duration::from_millis(300));
        adapter.pause().unwrap();
        thread::sleep(Duration::from_millis(50));
        let paused_snapshot = shared.snapshot();
        assert!(paused_snapshot.paused);
        assert!(paused_snapshot.position_ms > 0);

        thread::sleep(Duration::from_millis(300));
        let still_paused_snapshot = shared.snapshot();
        assert!(still_paused_snapshot.paused);
        assert!(
            still_paused_snapshot
                .position_ms
                .abs_diff(paused_snapshot.position_ms)
                <= 50,
            "paused playback advanced too far: {} -> {}",
            paused_snapshot.position_ms,
            still_paused_snapshot.position_ms
        );

        adapter.seek(42_000).unwrap();
        thread::sleep(Duration::from_millis(50));
        let sought_snapshot = shared.snapshot();
        assert_eq!(sought_snapshot.position_ms, 42_000);
        assert!(sought_snapshot.paused);

        thread::sleep(Duration::from_millis(300));
        let still_sought_snapshot = shared.snapshot();
        assert_eq!(still_sought_snapshot.position_ms, 42_000);

        adapter.resume().unwrap();
        thread::sleep(Duration::from_millis(300));
        let resumed_snapshot = shared.snapshot();
        assert!(!resumed_snapshot.paused);
        assert!(resumed_snapshot.position_ms > 42_000);
    }

    #[test]
    fn percent_to_decibels_maps_full_and_silence() {
        assert_eq!(percent_to_decibels(0), Decibels::SILENCE);
        // 100% is unity gain (0 dB).
        assert!((percent_to_decibels(100).0 - 0.0).abs() < 1e-4);
    }

    #[test]
    fn percent_to_decibels_is_monotonic_and_attenuates_below_full() {
        let quiet = percent_to_decibels(25).0;
        let mid = percent_to_decibels(50).0;
        let loud = percent_to_decibels(100).0;
        assert!(quiet < mid);
        assert!(mid < loud);
        // Below 100% attenuates (negative dB).
        assert!(mid < 0.0);
    }

    #[test]
    fn derive_playback_flags_only_treats_stopped_as_finished() {
        assert_eq!(derive_playback_flags(PlaybackState::Playing), (false, false));
        assert_eq!(derive_playback_flags(PlaybackState::Paused), (true, false));
        assert_eq!(
            derive_playback_flags(PlaybackState::Pausing),
            (true, false)
        );
        assert_eq!(derive_playback_flags(PlaybackState::Stopped), (false, true));
    }

    #[test]
    fn finished_simulated_sources_clear_shared_snapshot() {
        let shared = SharedPlaybackState::default();
        let (event_tx, _) = tokio::sync::mpsc::unbounded_channel();
        let mut adapter = LocalPlaybackAdapter::new(shared.clone(), event_tx);
        let mut short_item = queue_item("queue_item_short");
        short_item.song.duration_ms = 50;

        adapter.start(&short_item, "playback_session_1", 0).unwrap();
        thread::sleep(Duration::from_millis(400));

        let snapshot = shared.snapshot();
        assert!(snapshot.current_item.is_none());
        assert_eq!(snapshot.playback_session_id, None);
        assert_eq!(snapshot.position_ms, 50);
        assert!(!snapshot.paused);
    }

    #[test]
    fn first_non_empty_line_skips_blank_output() {
        let line = first_non_empty_line(b"\n\nhttps://example.com/audio\nignored\n");
        assert_eq!(line.as_deref(), Some("https://example.com/audio"));
    }

    #[test]
    fn fixture_like_urls_stay_simulated() {
        assert!(!is_real_playback_source("https://example.com/queue_item_1"));
        assert!(!is_real_playback_source(
            "https://www.youtube.com/watch?v=current-song"
        ));
        assert!(is_real_playback_source(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        ));
        assert!(is_real_playback_source("https://youtu.be/dQw4w9WgXcQ?t=43"));
    }

    #[test]
    fn youtube_format_prefers_backend_decodable_audio() {
        assert_eq!(
            YT_DLP_AUDIO_FORMAT,
            "140/139/bestaudio[ext=m4a]/bestaudio[acodec^=mp4a]/bestaudio[ext=mp3]/bestaudio[acodec^=mp3]/best[ext=mp4]/best"
        );
    }

    #[test]
    fn current_thread_runtime_drops_background_tasks_after_block_on_returns() {
        let runtime = RuntimeBuilder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (tx, rx) = mpsc::channel();

        runtime.block_on(async move {
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(20)).await;
                let _ = tx.send(());
            });
        });

        thread::sleep(Duration::from_millis(80));

        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn multi_thread_runtime_keeps_background_tasks_alive_after_block_on_returns() {
        let runtime = RuntimeBuilder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let (tx, rx) = mpsc::channel();

        let join = thread::spawn(move || {
            handle.block_on(async move {
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let _ = tx.send(());
                });
            });
        });

        join.join().unwrap();

        assert_eq!(rx.recv_timeout(Duration::from_millis(200)), Ok(()));
    }
}
