use std::env;
use std::ffi::OsStr;
use std::fmt::{self, Display, Formatter};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use nocturne_api::{
    CURRENT_EVENT_ID_HEADER, CommandAccepted, EVENTS_PATH, EmptyPayload, EventEnvelope,
    HEALTH_PATH, LAST_EVENT_ID_HEADER, PlaybackProgress, PlaybackVolumeRequest, ProblemDetails,
    QueueAddRequest, QueueRemoveRequest, STATE_PATH, SearchCommandRequest, SearchJobCompleted,
    SearchJobFailed, SearchJobSummary, SearchResultsResponse, StateSnapshot, SystemError,
};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;

use crate::app::AppEvent;

const EVENT_CURSOR_NOT_FOUND_TYPE: &str = "https://nocturne.local/problems/event-cursor-not-found";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandAction {
    Search(String),
    AddSong(String),
    PlayPause,
    Next,
    Previous,
    SetVolume(u8),
    RemoveQueueItem(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    StatusMessage(String),
    SearchSubmitted { job_id: String, query: String },
    SongQueued(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchResultsOutcome {
    Loaded {
        job: SearchJobSummary,
        results: Vec<nocturne_domain::Song>,
    },
    Failed {
        job_id: String,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct BackendEvent {
    pub event_id: String,
    pub kind: BackendEventKind,
}

#[derive(Debug, Clone)]
pub enum BackendEventKind {
    StateUpdated(StateSnapshot),
    PlaybackProgress(PlaybackProgress),
    SearchJobStarted(SearchJobSummary),
    SearchJobCompleted(SearchJobCompleted),
    SearchJobFailed(SearchJobFailed),
    SystemError(SystemError),
}

#[derive(Debug)]
pub struct SnapshotSeed {
    pub snapshot: StateSnapshot,
    pub current_event_id: Option<String>,
}

#[derive(Debug)]
pub struct ManagedBackend {
    child: Child,
    addr: SocketAddr,
}

#[derive(Debug, Clone)]
pub struct BackendClient {
    http: reqwest::Client,
    base_url: String,
}

#[derive(Debug)]
pub struct TuiError {
    message: String,
}

impl TuiError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl Display for TuiError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TuiError {}

impl ManagedBackend {
    pub fn spawn() -> Result<Self, TuiError> {
        let addr = reserve_loopback_addr()?;
        let executable = ensure_backend_binary()?;
        let child = Command::new(executable)
            .env("NOCTURNE_BACKEND_ADDR", addr.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(configured_backend_stderr())
            .spawn()
            .map_err(|error| TuiError::new(format!("failed to launch backend: {error}")))?;

        Ok(Self { child, addr })
    }

    pub async fn wait_until_ready(&mut self) -> Result<(), TuiError> {
        let client = reqwest::Client::new();
        let health_url = format!("http://{}{}", self.addr, HEALTH_PATH);

        for _attempt in 0..50 {
            if let Some(status) = self.child.try_wait().map_err(|error| {
                TuiError::new(format!("failed to inspect backend process: {error}"))
            })? {
                return Err(TuiError::new(format!(
                    "backend exited before becoming ready: {status}"
                )));
            }

            if let Ok(response) = client.get(&health_url).send().await
                && response.status().is_success()
            {
                let payload = response
                    .json::<nocturne_api::HealthResponse>()
                    .await
                    .map_err(|error| {
                        TuiError::new(format!("backend health response was invalid: {error}"))
                    })?;
                if payload.ready {
                    return Ok(());
                }
            }

            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        Err(TuiError::new(
            "backend did not become ready within 5 seconds",
        ))
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn configured_backend_stderr() -> Stdio {
    match env::var("NOCTURNE_BACKEND_STDERR") {
        Ok(value)
            if matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "inherit"
            ) =>
        {
            Stdio::inherit()
        }
        _ => Stdio::null(),
    }
}

impl BackendClient {
    pub fn new(addr: SocketAddr) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: format!("http://{addr}"),
        }
    }

    pub async fn fetch_snapshot(&self) -> Result<SnapshotSeed, TuiError> {
        let response = self
            .http
            .get(format!("{}{}", self.base_url, STATE_PATH))
            .send()
            .await
            .map_err(|error| TuiError::new(format!("failed to fetch state snapshot: {error}")))?;

        let current_event_id = header_value(response.headers(), CURRENT_EVENT_ID_HEADER);
        let snapshot = response
            .json::<StateSnapshot>()
            .await
            .map_err(|error| TuiError::new(format!("failed to decode state snapshot: {error}")))?;

        Ok(SnapshotSeed {
            snapshot,
            current_event_id,
        })
    }

    pub async fn send_command(&self, action: CommandAction) -> Result<CommandOutcome, String> {
        match action {
            CommandAction::Search(query) => self
                .post_json(
                    "/api/v1/commands/search",
                    &SearchCommandRequest {
                        query: query.clone(),
                    },
                )
                .await
                .and_then(|accepted| {
                    accepted
                        .job_id
                        .map(|job_id| CommandOutcome::SearchSubmitted { job_id, query })
                        .ok_or_else(|| {
                            String::from("search command was accepted but no job id was returned")
                        })
                }),
            CommandAction::AddSong(song_id) => self
                .post_json("/api/v1/commands/queue/add", &QueueAddRequest { song_id })
                .await
                .map(|_| CommandOutcome::SongQueued(String::from("Added selection to queue."))),
            CommandAction::PlayPause => self
                .post_json(
                    "/api/v1/commands/playback/play-pause",
                    &EmptyPayload::default(),
                )
                .await
                .map(|_| CommandOutcome::StatusMessage(String::from("Playback toggle accepted."))),
            CommandAction::Next => self
                .post_json("/api/v1/commands/playback/next", &EmptyPayload::default())
                .await
                .map(|_| {
                    CommandOutcome::StatusMessage(String::from("Skip to next track accepted."))
                }),
            CommandAction::Previous => self
                .post_json(
                    "/api/v1/commands/playback/previous",
                    &EmptyPayload::default(),
                )
                .await
                .map(|_| {
                    CommandOutcome::StatusMessage(String::from("Restart current track accepted."))
                }),
            CommandAction::SetVolume(volume_percent) => {
                self.post_json(
                    "/api/v1/commands/playback/volume",
                    &PlaybackVolumeRequest { volume_percent },
                )
                .await
                .map(|_| CommandOutcome::StatusMessage(format!("Volume set to {volume_percent}%.")))
            }
            CommandAction::RemoveQueueItem(queue_item_id) => self
                .post_json(
                    "/api/v1/commands/queue/remove",
                    &QueueRemoveRequest { queue_item_id },
                )
                .await
                .map(|_| CommandOutcome::StatusMessage(String::from("Queue removal accepted."))),
        }
    }

    pub async fn fetch_search_results(&self, job_id: &str) -> SearchResultsOutcome {
        let path = format!("/api/v1/search/results/{job_id}");
        let response = match self
            .http
            .get(format!("{}{}", self.base_url, path))
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) => {
                return SearchResultsOutcome::Failed {
                    job_id: job_id.to_owned(),
                    message: format!("request to {path} failed: {error}"),
                };
            }
        };

        if response.status().is_success() {
            match response.json::<SearchResultsResponse>().await {
                Ok(payload) => SearchResultsOutcome::Loaded {
                    job: payload.job,
                    results: payload.results,
                },
                Err(error) => SearchResultsOutcome::Failed {
                    job_id: job_id.to_owned(),
                    message: format!("search results response from {path} was invalid: {error}"),
                },
            }
        } else {
            let problem = match response.json::<ProblemDetails>().await {
                Ok(problem) => problem,
                Err(error) => {
                    return SearchResultsOutcome::Failed {
                        job_id: job_id.to_owned(),
                        message: format!(
                            "request to {path} failed with undecodable error: {error}"
                        ),
                    };
                }
            };
            SearchResultsOutcome::Failed {
                job_id: job_id.to_owned(),
                message: problem.detail,
            }
        }
    }

    async fn post_json<T>(&self, path: &str, payload: &T) -> Result<CommandAccepted, String>
    where
        T: Serialize + ?Sized,
    {
        let response = self
            .http
            .post(format!("{}{}", self.base_url, path))
            .json(payload)
            .send()
            .await
            .map_err(|error| format!("request to {path} failed: {error}"))?;

        if response.status().is_success() {
            response
                .json::<CommandAccepted>()
                .await
                .map_err(|error| format!("command response from {path} was invalid: {error}"))
        } else {
            let problem = response.json::<ProblemDetails>().await.map_err(|error| {
                format!("request to {path} failed with undecodable error: {error}")
            })?;
            Err(problem.detail)
        }
    }

    async fn stream_once(
        &self,
        last_event_id: Option<String>,
        tx: &UnboundedSender<AppEvent>,
    ) -> Result<Option<String>, TuiError> {
        let mut headers = HeaderMap::new();
        if let Some(last_event_id) = &last_event_id {
            let value = HeaderValue::from_str(last_event_id).map_err(|error| {
                TuiError::new(format!("invalid Last-Event-ID header value: {error}"))
            })?;
            headers.insert(LAST_EVENT_ID_HEADER, value);
        }

        let response = self
            .http
            .get(format!("{}{}", self.base_url, EVENTS_PATH))
            .headers(headers)
            .send()
            .await
            .map_err(|error| {
                TuiError::new(format!("failed to connect to event stream: {error}"))
            })?;

        if !response.status().is_success() {
            let status = response.status();
            let problem = response.json::<ProblemDetails>().await.map_err(|error| {
                TuiError::new(format!(
                    "event stream failed with status {} and undecodable error: {error}",
                    status
                ))
            })?;

            if problem.r#type == EVENT_CURSOR_NOT_FOUND_TYPE {
                let seed = self.fetch_snapshot().await.map_err(|error| {
                    TuiError::new(format!(
                        "event cursor expired and snapshot refresh failed: {error}"
                    ))
                })?;
                let next_cursor = seed.current_event_id.clone();
                if tx.send(AppEvent::Snapshot(seed)).is_err() {
                    return Ok(next_cursor);
                }
                return Ok(next_cursor);
            }

            return Err(TuiError::new(format!(
                "event stream failed with status {}: {}",
                status, problem.detail
            )));
        }

        let mut stream = response.bytes_stream().eventsource();
        let mut latest_event_id = last_event_id;

        while let Some(event) = stream.next().await {
            let event =
                event.map_err(|error| TuiError::new(format!("event stream error: {error}")))?;
            let envelope =
                serde_json::from_str::<EventEnvelope<Value>>(&event.data).map_err(|error| {
                    TuiError::new(format!(
                        "failed to decode SSE payload '{}': {error}",
                        event.data
                    ))
                })?;

            latest_event_id = Some(envelope.event_id.clone());
            if let Some(decoded) = decode_backend_event(envelope)?
                && tx.send(AppEvent::Backend(decoded)).is_err()
            {
                return Ok(latest_event_id);
            }
        }

        Ok(latest_event_id)
    }
}

pub fn spawn_sse_task(
    client: BackendClient,
    last_event_id: Option<String>,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let mut cursor = last_event_id;

        loop {
            match client.stream_once(cursor.clone(), &tx).await {
                Ok(next_cursor) => {
                    cursor = next_cursor;
                    if tx
                        .send(AppEvent::ConnectionStatus(String::from(
                            "Event stream disconnected. Reconnecting...",
                        )))
                        .is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    if tx
                        .send(AppEvent::ConnectionStatus(format!(
                            "Event stream error: {error}. Reconnecting..."
                        )))
                        .is_err()
                    {
                        break;
                    }
                }
            }

            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });
}

pub fn spawn_command_task(
    client: BackendClient,
    action: CommandAction,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let result = client.send_command(action).await;
        let _ = tx.send(AppEvent::CommandResult(result));
    });
}

pub fn spawn_search_results_task(
    client: BackendClient,
    job_id: String,
    tx: UnboundedSender<AppEvent>,
) {
    tokio::spawn(async move {
        let result = client.fetch_search_results(&job_id).await;
        let _ = tx.send(AppEvent::SearchResults(result));
    });
}

fn decode_backend_event(envelope: EventEnvelope<Value>) -> Result<Option<BackendEvent>, TuiError> {
    let event_id = envelope.event_id;
    let kind = match envelope.event.as_str() {
        "state.updated" => BackendEventKind::StateUpdated(
            serde_json::from_value::<StateSnapshot>(envelope.data).map_err(|error| {
                TuiError::new(format!("failed to decode state.updated: {error}"))
            })?,
        ),
        "playback.progress" => BackendEventKind::PlaybackProgress(
            serde_json::from_value::<PlaybackProgress>(envelope.data).map_err(|error| {
                TuiError::new(format!("failed to decode playback.progress: {error}"))
            })?,
        ),
        "search.job.started" => BackendEventKind::SearchJobStarted(
            serde_json::from_value::<SearchJobSummary>(envelope.data).map_err(|error| {
                TuiError::new(format!("failed to decode search.job.started: {error}"))
            })?,
        ),
        "search.job.completed" => BackendEventKind::SearchJobCompleted(
            serde_json::from_value::<SearchJobCompleted>(envelope.data).map_err(|error| {
                TuiError::new(format!("failed to decode search.job.completed: {error}"))
            })?,
        ),
        "search.job.failed" => BackendEventKind::SearchJobFailed(
            serde_json::from_value::<SearchJobFailed>(envelope.data).map_err(|error| {
                TuiError::new(format!("failed to decode search.job.failed: {error}"))
            })?,
        ),
        "system.error" => BackendEventKind::SystemError(
            serde_json::from_value::<SystemError>(envelope.data).map_err(|error| {
                TuiError::new(format!("failed to decode system.error: {error}"))
            })?,
        ),
        _ => return Ok(None),
    };

    Ok(Some(BackendEvent { event_id, kind }))
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn reserve_loopback_addr() -> Result<SocketAddr, TuiError> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| TuiError::new(format!("failed to reserve backend port: {error}")))?;
    listener
        .local_addr()
        .map_err(|error| TuiError::new(format!("failed to read reserved backend address: {error}")))
}

fn ensure_backend_binary() -> Result<PathBuf, TuiError> {
    let candidate = backend_binary_candidates()
        .into_iter()
        .find(|path| path.is_file());

    if let Some(path) = candidate {
        return Ok(path);
    }

    let workspace_root = workspace_root()?;
    let status = Command::new("cargo")
        .arg("build")
        .arg("-p")
        .arg("nocturne-backend")
        .current_dir(&workspace_root)
        .status()
        .map_err(|error| TuiError::new(format!("failed to build backend binary: {error}")))?;

    if !status.success() {
        return Err(TuiError::new(format!(
            "building nocturne-backend failed with status {status}"
        )));
    }

    backend_binary_candidates()
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| TuiError::new("backend binary was built but could not be located"))
}

fn backend_binary_candidates() -> Vec<PathBuf> {
    let executable_name = format!("nocturne-backend{}", env::consts::EXE_SUFFIX);
    let mut candidates = Vec::new();
    if let Ok(current_exe) = env::current_exe() {
        candidates.push(current_exe.with_file_name(&executable_name));
        if let Some(parent) = current_exe.parent() {
            candidates.push(parent.join(&executable_name));
            if parent.file_name() == Some(OsStr::new("deps"))
                && let Some(profile_dir) = parent.parent()
            {
                candidates.push(profile_dir.join(&executable_name));
            }
        }
    }
    if let Ok(workspace_root) = workspace_root() {
        candidates.push(
            workspace_root
                .join("target")
                .join("debug")
                .join(&executable_name),
        );
    }
    candidates
}

fn workspace_root() -> Result<PathBuf, TuiError> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .map(Path::to_path_buf)
        .ok_or_else(|| TuiError::new("failed to locate workspace root from apps/tui"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::extract::State;
    use axum::http::{HeaderMap as AxumHeaderMap, StatusCode};
    use axum::{Json, Router, routing::get};
    use nocturne_api::{BackendStatus, ProblemDetails};
    use nocturne_domain::{AudioSettings, PlaybackState, PlaybackStatus};
    use tokio::net::TcpListener as TokioTcpListener;
    use tokio::sync::{Mutex, oneshot};

    #[derive(Clone)]
    struct TestState {
        snapshot_calls: Arc<Mutex<u32>>,
    }

    #[tokio::test]
    async fn stream_once_refreshes_snapshot_when_cursor_is_missing() {
        let state = TestState {
            snapshot_calls: Arc::new(Mutex::new(0)),
        };
        let app = Router::new()
            .route(STATE_PATH, get(state_handler))
            .route(EVENTS_PATH, get(events_handler))
            .with_state(state.clone());

        let listener = TokioTcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        let client = BackendClient::new(addr);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let next_cursor = client
            .stream_once(Some(String::from("evt_missing")), &tx)
            .await
            .unwrap();

        let event = rx.recv().await.unwrap();
        let AppEvent::Snapshot(seed) = event else {
            panic!("expected snapshot event");
        };

        assert_eq!(next_cursor.as_deref(), Some("evt_latest"));
        assert_eq!(seed.current_event_id.as_deref(), Some("evt_latest"));
        assert!(seed.snapshot.backend.ready);
        assert_eq!(seed.snapshot.playback.state, PlaybackStatus::Paused);
        assert_eq!(seed.snapshot.snapshot_id, "snap_refresh");
        assert_eq!(*state.snapshot_calls.lock().await, 1);

        let _ = shutdown_tx.send(());
        server.await.unwrap();
    }

    async fn state_handler(State(state): State<TestState>) -> (AxumHeaderMap, Json<StateSnapshot>) {
        *state.snapshot_calls.lock().await += 1;

        let mut headers = AxumHeaderMap::new();
        headers.insert(
            CURRENT_EVENT_ID_HEADER,
            HeaderValue::from_static("evt_latest"),
        );

        (
            headers,
            Json(StateSnapshot {
                backend: BackendStatus {
                    ready: true,
                    version: Some(String::from("test")),
                    yt_dlp_version: Some(String::from("2026.05.01")),
                },
                playback: PlaybackState {
                    state: PlaybackStatus::Paused,
                    position_ms: 42_000,
                    current_queue_item_id: None,
                    playback_session_id: None,
                },
                audio: AudioSettings::default(),
                current_song: None,
                queue: Vec::new(),
                search_jobs: Vec::new(),
                revision: 1,
                snapshot_id: String::from("snap_refresh"),
                timestamp: String::from("2026-04-24T09:00:00Z"),
            }),
        )
    }

    async fn events_handler() -> (StatusCode, Json<ProblemDetails>) {
        (
            StatusCode::CONFLICT,
            Json(ProblemDetails {
                r#type: String::from(EVENT_CURSOR_NOT_FOUND_TYPE),
                title: String::from("event cursor no longer available"),
                status: StatusCode::CONFLICT.as_u16(),
                detail: String::from(
                    "The requested Last-Event-ID is no longer present in the replay buffer. Refresh state and reconnect to the event stream.",
                ),
                instance: Some(String::from(EVENTS_PATH)),
                errors: Vec::new(),
            }),
        )
    }
}
