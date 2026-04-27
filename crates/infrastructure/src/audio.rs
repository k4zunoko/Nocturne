use std::env;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use nocturne_core::{PlaybackPort, PortError};
use nocturne_domain::QueueItem;
use rodio::{Decoder, DeviceSinkBuilder, MixerDeviceSink, Player};
use stream_download::storage::temp::TempStorageProvider;
use stream_download::{Settings, StreamDownload};

use crate::recover_lock;

const DEFAULT_YT_DLP_BINARY: &str = "yt-dlp";
const DEFAULT_WINDOWS_YT_DLP_PATH: &str = r"C:\tools\yt-dlp\yt-dlp.exe";
const YT_DLP_PATH_ENV: &str = "NOCTURNE_YT_DLP_PATH";
const YT_DLP_TIMEOUT: Duration = Duration::from_secs(15);
const WORKER_POLL_INTERVAL: Duration = Duration::from_millis(250);

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
    pub finished_queue_item_id: Option<String>,
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

#[derive(Debug)]
pub struct LocalPlaybackAdapter {
    commands: SyncSender<WorkerCommand>,
    worker: Option<JoinHandle<()>>,
}

impl LocalPlaybackAdapter {
    #[must_use]
    pub fn new(state: SharedPlaybackState) -> Self {
        let (commands, receiver) = mpsc::sync_channel(8);
        let worker_state = state.clone();
        let worker = thread::spawn(move || playback_worker_loop(receiver, worker_state));

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
    fn start(&mut self, item: &QueueItem, position_ms: u64) -> Result<(), PortError> {
        self.request(|reply| WorkerCommand::Start {
            item: item.clone(),
            position_ms,
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
        position_ms: u64,
        reply: SyncSender<Result<(), PortError>>,
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
    Shutdown,
}

enum ActivePlayback {
    Simulated,
    Rodio {
        queue_item_id: String,
        _sink: MixerDeviceSink,
        player: Player,
    },
}

fn playback_worker_loop(receiver: mpsc::Receiver<WorkerCommand>, state: SharedPlaybackState) {
    let mut active = None;

    loop {
        match receiver.recv_timeout(WORKER_POLL_INTERVAL) {
            Ok(WorkerCommand::Start {
                item,
                position_ms,
                reply,
            }) => {
                let result = handle_start(&state, &mut active, item, position_ms);
                let _ = reply.send(result);
            }
            Ok(WorkerCommand::Pause { reply }) => {
                let _ = reply.send(handle_pause(&state, &active));
            }
            Ok(WorkerCommand::Resume { reply }) => {
                let _ = reply.send(handle_resume(&state, &active));
            }
            Ok(WorkerCommand::Stop { reply }) => {
                let _ = reply.send(handle_stop(&state, &mut active));
            }
            Ok(WorkerCommand::Seek { position_ms, reply }) => {
                let _ = reply.send(handle_seek(&state, &active, position_ms));
            }
            Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => {
                clear_active_playback(&state, &mut active, false);
                break;
            }
            Err(RecvTimeoutError::Timeout) => poll_active_playback(&state, &mut active),
        }
    }
}

fn handle_start(
    state: &SharedPlaybackState,
    active: &mut Option<ActivePlayback>,
    item: QueueItem,
    position_ms: u64,
) -> Result<(), PortError> {
    clear_active_playback(state, active, false);

    let next_active = if should_simulate_source(&item.song.source_url) {
        ActivePlayback::Simulated
    } else {
        open_real_playback(&item, position_ms)?
    };

    {
        let mut snapshot = recover_lock(&state.inner);
        snapshot.current_item = Some(item.clone());
        snapshot.position_ms = position_ms;
        snapshot.paused = false;
        snapshot.finished_queue_item_id = None;
        snapshot.history.push(PlaybackCommand::Start {
            queue_item_id: item.id.clone(),
            position_ms,
        });
    }

    *active = Some(next_active);
    Ok(())
}

fn handle_pause(
    state: &SharedPlaybackState,
    active: &Option<ActivePlayback>,
) -> Result<(), PortError> {
    if let Some(ActivePlayback::Rodio { player, .. }) = active.as_ref() {
        player.pause();
    }

    let mut snapshot = recover_lock(&state.inner);
    snapshot.paused = true;
    snapshot.history.push(PlaybackCommand::Pause);
    Ok(())
}

fn handle_resume(
    state: &SharedPlaybackState,
    active: &Option<ActivePlayback>,
) -> Result<(), PortError> {
    if let Some(ActivePlayback::Rodio { player, .. }) = active.as_ref() {
        player.play();
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
    active: &Option<ActivePlayback>,
    position_ms: u64,
) -> Result<(), PortError> {
    if let Some(ActivePlayback::Rodio { player, .. }) = active.as_ref() {
        player
            .try_seek(Duration::from_millis(position_ms))
            .map_err(|error| {
                PortError::new(
                    "playback_seek_failed",
                    format!("failed to seek active playback: {error}"),
                )
            })?;
    }

    let mut snapshot = recover_lock(&state.inner);
    snapshot.position_ms = position_ms;
    snapshot.history.push(PlaybackCommand::Seek { position_ms });
    Ok(())
}

fn poll_active_playback(state: &SharedPlaybackState, active: &mut Option<ActivePlayback>) {
    let Some(active_playback) = active.as_ref() else {
        return;
    };

    let (position_ms, paused, finished_queue_item_id) = match active_playback {
        ActivePlayback::Simulated { .. } => return,
        ActivePlayback::Rodio {
            queue_item_id,
            player,
            ..
        } => {
            let finished = player.empty();
            (
                player.get_pos().as_millis() as u64,
                player.is_paused(),
                finished.then(|| queue_item_id.clone()),
            )
        }
    };

    {
        let mut snapshot = recover_lock(&state.inner);
        snapshot.position_ms = position_ms;
        snapshot.paused = paused;
        if let Some(finished_queue_item_id) = finished_queue_item_id.clone() {
            snapshot.finished_queue_item_id = Some(finished_queue_item_id);
            snapshot.paused = false;
        }
    }

    if finished_queue_item_id.is_some() {
        *active = None;
    }
}

fn clear_active_playback(
    state: &SharedPlaybackState,
    active: &mut Option<ActivePlayback>,
    record_stop: bool,
) {
    if let Some(active_playback) = active.take()
        && let ActivePlayback::Rodio { player, .. } = active_playback
    {
        player.stop();
    }

    let mut snapshot = recover_lock(&state.inner);
    snapshot.current_item = None;
    snapshot.position_ms = 0;
    snapshot.paused = false;
    snapshot.finished_queue_item_id = None;
    if record_stop {
        snapshot.history.push(PlaybackCommand::Stop);
    }
}

fn open_real_playback(item: &QueueItem, position_ms: u64) -> Result<ActivePlayback, PortError> {
    let sink = DeviceSinkBuilder::open_default_sink().map_err(|error| {
        PortError::new(
            "audio_output_unavailable",
            format!("failed to open the default audio output device: {error}"),
        )
    })?;
    let player = Player::connect_new(sink.mixer());

    let playback_url = resolve_playback_url(&item.song.source_url)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            PortError::new(
                "playback_runtime_failed",
                format!("failed to build playback runtime: {error}"),
            )
        })?;

    let reader = runtime.block_on(async {
        let stream_url = playback_url.parse().map_err(|error| {
            PortError::new(
                "playback_url_invalid",
                format!("playback URL could not be parsed: {error}"),
            )
        })?;

        StreamDownload::new_http(stream_url, TempStorageProvider::new(), Settings::default())
            .await
            .map_err(|error| {
                PortError::new(
                    "playback_stream_open_failed",
                    format!("failed to open the playback stream: {error}"),
                )
            })
    })?;

    let decoder = Decoder::new(reader).map_err(|error| {
        PortError::new(
            "playback_decode_failed",
            format!("failed to decode the playback stream: {error}"),
        )
    })?;
    player.append(decoder);

    if position_ms > 0 {
        player
            .try_seek(Duration::from_millis(position_ms))
            .map_err(|error| {
                PortError::new(
                    "playback_seek_failed",
                    format!("failed to seek the playback stream: {error}"),
                )
            })?;
    }

    Ok(ActivePlayback::Rodio {
        queue_item_id: item.id.clone(),
        _sink: sink,
        player,
    })
}

fn resolve_playback_url(source_url: &str) -> Result<String, PortError> {
    if is_youtube_url(source_url) {
        resolve_youtube_playback_url(source_url)
    } else {
        Ok(source_url.to_owned())
    }
}

fn resolve_youtube_playback_url(source_url: &str) -> Result<String, PortError> {
    let output = execute_yt_dlp([
        "--get-url",
        "--format",
        "bestaudio/best",
        "--no-playlist",
        "--no-warnings",
        "--quiet",
        source_url,
    ])?;

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

fn execute_yt_dlp<'a>(
    args: impl IntoIterator<Item = &'a str>,
) -> Result<std::process::Output, PortError> {
    let mut child = Command::new(yt_dlp_executable())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(map_yt_dlp_spawn_error)?;

    let stdout = child.stdout.take().ok_or_else(|| {
        PortError::new(
            "yt_dlp_spawn_failed",
            "yt-dlp could not start its stdout pipe",
        )
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        PortError::new(
            "yt_dlp_spawn_failed",
            "yt-dlp could not start its stderr pipe",
        )
    })?;

    let stdout_thread = thread::spawn(move || read_stream(stdout));
    let stderr_thread = thread::spawn(move || read_stream(stderr));
    let started_at = Instant::now();

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started_at.elapsed() >= YT_DLP_TIMEOUT => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(PortError::new(
                    "yt_dlp_timeout",
                    "yt-dlp timed out while preparing playback",
                ));
            }
            Ok(None) => thread::sleep(Duration::from_millis(50)),
            Err(error) => {
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(PortError::new(
                    "yt_dlp_spawn_failed",
                    format!("yt-dlp status check failed: {error}"),
                ));
            }
        }
    };

    let stdout = stdout_thread
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("stdout reader panicked")))
        .map_err(|error| {
            PortError::new(
                "yt_dlp_spawn_failed",
                format!("failed to read yt-dlp stdout: {error}"),
            )
        })?;
    let stderr = stderr_thread
        .join()
        .unwrap_or_else(|_| Err(std::io::Error::other("stderr reader panicked")))
        .map_err(|error| {
            PortError::new(
                "yt_dlp_spawn_failed",
                format!("failed to read yt-dlp stderr: {error}"),
            )
        })?;

    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

fn yt_dlp_executable() -> std::ffi::OsString {
    if let Some(path) = env::var_os(YT_DLP_PATH_ENV) {
        return path;
    }

    let windows_fallback = std::path::PathBuf::from(DEFAULT_WINDOWS_YT_DLP_PATH);
    if windows_fallback.is_file() {
        return windows_fallback.into_os_string();
    }

    DEFAULT_YT_DLP_BINARY.into()
}

fn map_yt_dlp_spawn_error(error: std::io::Error) -> PortError {
    let code = if error.kind() == std::io::ErrorKind::NotFound {
        "yt_dlp_missing"
    } else {
        "yt_dlp_spawn_failed"
    };

    PortError::new(code, format!("yt-dlp is unavailable for playback: {error}"))
}

fn read_stream(mut stream: impl Read) -> std::io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer)?;
    Ok(buffer)
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

    #[test]
    fn simulated_sources_can_be_stopped() {
        let shared = SharedPlaybackState::default();
        let mut adapter = LocalPlaybackAdapter::new(shared.clone());

        adapter.start(&queue_item("queue_item_1"), 1_500).unwrap();
        adapter.stop().unwrap();

        let snapshot = shared.snapshot();
        assert!(snapshot.current_item.is_none());
        assert_eq!(snapshot.position_ms, 0);
        assert_eq!(snapshot.history.len(), 2);
    }

    #[test]
    fn first_non_empty_line_skips_blank_output() {
        let line = first_non_empty_line(b"\n\nhttps://example.com/audio\nignored\n");
        assert_eq!(line.as_deref(), Some("https://example.com/audio"));
    }

    #[test]
    fn fixture_like_urls_stay_simulated() {
        assert!(!is_real_playback_source("https://example.com/queue_item_1"));
        assert!(!is_real_playback_source("https://www.youtube.com/watch?v=current-song"));
        assert!(is_real_playback_source(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        ));
        assert!(is_real_playback_source("https://youtu.be/dQw4w9WgXcQ?t=43"));
    }
}
