use std::env;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{ETAG, IF_NONE_MATCH};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::recover_lock;

pub const DEFAULT_YT_DLP_BINARY: &str = if cfg!(windows) {
    "yt-dlp.exe"
} else {
    "yt-dlp"
};
pub const DEFAULT_WINDOWS_YT_DLP_PATH: &str = r"C:\tools\yt-dlp\yt-dlp.exe";
pub const YT_DLP_PATH_ENV: &str = "NOCTURNE_YT_DLP_PATH";
pub const YT_DLP_COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
pub const YT_DLP_UPDATE_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

const CONFIG_DIR_ENV: &str = "NOCTURNE_CONFIG_DIR";
const SETTINGS_FILE_NAME: &str = "yt-dlp-settings.json";
const MANAGED_ROOT_DIR_NAME: &str = "yt-dlp";
const STATE_FILE_NAME: &str = "state.json";
const VERSIONS_DIR_NAME: &str = "versions";
const STAGING_DIR_NAME: &str = "staging";
const STABLE_RELEASE_URL: &str = "https://api.github.com/repos/yt-dlp/yt-dlp/releases/latest";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum YtDlpReleaseChannel {
    #[default]
    Stable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalYtDlpSettings {
    pub auto_update: bool,
    pub channel: YtDlpReleaseChannel,
    pub last_checked_unix_s: Option<u64>,
    pub release_etag: Option<String>,
}

impl Default for LocalYtDlpSettings {
    fn default() -> Self {
        Self {
            auto_update: true,
            channel: YtDlpReleaseChannel::Stable,
            last_checked_unix_s: None,
            release_etag: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
struct ManagedBinaryState {
    current_version: Option<String>,
    previous_version: Option<String>,
    pending_version: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LocalYtDlpSettingsStore {
    path: PathBuf,
}

impl LocalYtDlpSettingsStore {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            path: default_settings_path()?,
        })
    }

    #[must_use]
    pub fn from_path(path: PathBuf) -> Self {
        Self { path }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> io::Result<LocalYtDlpSettings> {
        match fs::read_to_string(&self.path) {
            Ok(contents) => serde_json::from_str::<LocalYtDlpSettings>(&contents)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                Ok(LocalYtDlpSettings::default())
            }
            Err(error) => Err(error),
        }
    }

    pub fn save(&self, settings: &LocalYtDlpSettings) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let payload = serde_json::to_vec_pretty(settings)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        fs::write(&self.path, payload)
    }
}

#[derive(Debug, Clone)]
pub struct LocalYtDlpManager {
    client: reqwest::Client,
    inner: std::sync::Arc<std::sync::Mutex<ManagerState>>,
}

#[derive(Debug, Clone)]
struct ManagerState {
    settings_store: LocalYtDlpSettingsStore,
    managed_root: PathBuf,
    state_path: PathBuf,
    settings: LocalYtDlpSettings,
    state: ManagedBinaryState,
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    prerelease: bool,
    draft: bool,
    assets: Vec<GitHubReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubReleaseAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateCheckResult {
    Skipped,
    UpToDate,
    Staged,
}

impl Default for LocalYtDlpManager {
    fn default() -> Self {
        let settings_store = LocalYtDlpSettingsStore::new().unwrap_or_else(|_| {
            LocalYtDlpSettingsStore::from_path(fallback_config_dir().join(SETTINGS_FILE_NAME))
        });
        Self::new(reqwest::Client::new(), settings_store)
    }
}

impl LocalYtDlpManager {
    #[must_use]
    pub fn new(client: reqwest::Client, settings_store: LocalYtDlpSettingsStore) -> Self {
        let managed_root = settings_store
            .path()
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(fallback_config_dir)
            .join(MANAGED_ROOT_DIR_NAME);
        let state_path = managed_root.join(STATE_FILE_NAME);
        let settings = settings_store.load().unwrap_or_default();
        let state = load_state(&state_path).unwrap_or_default();

        Self {
            client,
            inner: std::sync::Arc::new(std::sync::Mutex::new(ManagerState {
                settings_store,
                managed_root,
                state_path,
                settings,
                state,
            })),
        }
    }

    pub fn prepare(&self) -> io::Result<()> {
        self.ensure_directories()?;
        self.reload_persisted_state();
        self.promote_pending_if_ready()?;
        self.bootstrap_managed_binary_if_missing()?;
        Ok(())
    }

    pub fn execute<'a>(&self, args: impl IntoIterator<Item = &'a str>) -> io::Result<Output> {
        let mut child = Command::new(self.executable_path())
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("yt-dlp stdout pipe unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("yt-dlp stderr pipe unavailable"))?;

        let stdout_thread = std::thread::spawn(move || read_stream(stdout));
        let stderr_thread = std::thread::spawn(move || read_stream(stderr));
        let started_at = std::time::Instant::now();

        let status = loop {
            match child.try_wait()? {
                Some(status) => break status,
                None if started_at.elapsed() >= YT_DLP_COMMAND_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "yt-dlp command timed out",
                    ));
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        };

        let stdout = stdout_thread
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("stdout reader panicked")))?;
        let stderr = stderr_thread
            .join()
            .unwrap_or_else(|_| Err(io::Error::other("stderr reader panicked")))?;

        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }

    #[must_use]
    pub fn executable_path(&self) -> OsString {
        if let Some(path) = env::var_os(YT_DLP_PATH_ENV) {
            return path;
        }

        if let Some(path) = self.current_managed_binary_path() {
            return path.into_os_string();
        }

        if let Some(path) = self.bootstrap_source_path() {
            return path.into_os_string();
        }

        DEFAULT_YT_DLP_BINARY.into()
    }

    pub async fn check_for_updates(&self) -> Result<UpdateCheckResult, String> {
        let snapshot = self.snapshot();
        if !snapshot.settings.auto_update {
            return Ok(UpdateCheckResult::Skipped);
        }

        let mut request = self
            .client
            .get(release_url_for_channel(snapshot.settings.channel))
            .header(reqwest::header::USER_AGENT, "Nocturne/yt-dlp-updater");
        if let Some(etag) = snapshot.settings.release_etag.as_deref() {
            request = request.header(IF_NONE_MATCH, etag);
        }

        let response = request
            .send()
            .await
            .map_err(|error| format!("failed to query yt-dlp releases: {error}"))?;

        if response.status() == reqwest::StatusCode::NOT_MODIFIED {
            self.mark_checked(snapshot.settings.release_etag.clone())
                .map_err(|error| format!("failed to persist yt-dlp check timestamp: {error}"))?;
            return Ok(UpdateCheckResult::UpToDate);
        }

        response
            .error_for_status_ref()
            .map_err(|error| format!("yt-dlp release endpoint returned an error: {error}"))?;

        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let release = response
            .json::<GitHubRelease>()
            .await
            .map_err(|error| format!("failed to decode yt-dlp release metadata: {error}"))?;

        if release.draft || release.prerelease {
            self.mark_checked(etag)
                .map_err(|error| format!("failed to persist yt-dlp check timestamp: {error}"))?;
            return Ok(UpdateCheckResult::UpToDate);
        }

        let current_or_pending = snapshot
            .state
            .pending_version
            .clone()
            .or(snapshot.state.current_version.clone());
        if current_or_pending.as_deref() == Some(release.tag_name.as_str()) {
            self.mark_checked(etag)
                .map_err(|error| format!("failed to persist yt-dlp check timestamp: {error}"))?;
            return Ok(UpdateCheckResult::UpToDate);
        }

        let binary_asset_name = release_asset_name();
        let binary_asset = release
            .assets
            .iter()
            .find(|asset| asset.name == binary_asset_name)
            .ok_or_else(|| format!("yt-dlp release did not include asset {binary_asset_name}"))?;
        let checksum_asset = release
            .assets
            .iter()
            .find(|asset| asset.name == "SHA2-256SUMS")
            .ok_or_else(|| String::from("yt-dlp release did not include SHA2-256SUMS"))?;

        let checksum_body = self
            .client
            .get(&checksum_asset.browser_download_url)
            .header(reqwest::header::USER_AGENT, "Nocturne/yt-dlp-updater")
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| format!("failed to download yt-dlp checksums: {error}"))?
            .bytes()
            .await
            .map_err(|error| format!("failed to read yt-dlp checksums: {error}"))?;
        let expected_hash = parse_checksum_file(&checksum_body, binary_asset_name)
            .ok_or_else(|| format!("SHA2-256SUMS did not contain {binary_asset_name}"))?;

        let binary_bytes = self
            .client
            .get(&binary_asset.browser_download_url)
            .header(reqwest::header::USER_AGENT, "Nocturne/yt-dlp-updater")
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .map_err(|error| format!("failed to download yt-dlp binary: {error}"))?
            .bytes()
            .await
            .map_err(|error| format!("failed to read yt-dlp binary: {error}"))?;

        let actual_hash = compute_sha256(&binary_bytes);
        if actual_hash != expected_hash {
            return Err(format!(
                "yt-dlp checksum mismatch: expected {expected_hash}, got {actual_hash}"
            ));
        }

        let staged_path = self
            .stage_binary(&release.tag_name, &binary_bytes)
            .map_err(|error| format!("failed to stage yt-dlp update: {error}"))?;
        self.smoke_test_binary(&staged_path)
            .map_err(|error| format!("staged yt-dlp binary failed smoke test: {error}"))?;
        self.commit_staged_version(release.tag_name, etag)
            .map_err(|error| format!("failed to persist staged yt-dlp version: {error}"))?;
        Ok(UpdateCheckResult::Staged)
    }

    pub fn check_for_updates_if_due(
        &self,
    ) -> impl std::future::Future<Output = Result<UpdateCheckResult, String>> + Send + 'static {
        let manager = self.clone();
        async move {
            if !manager.should_check_now() {
                return Ok(UpdateCheckResult::Skipped);
            }
            manager.check_for_updates().await
        }
    }

    pub fn apply_pending_update(&self) -> io::Result<bool> {
        let previous_version = self.current_version();
        self.promote_pending_if_ready()?;
        Ok(self.current_version() != previous_version)
    }

    fn should_check_now(&self) -> bool {
        let snapshot = self.snapshot();
        let Some(last_checked) = snapshot.settings.last_checked_unix_s else {
            return true;
        };

        let now = unix_timestamp_now();
        now.saturating_sub(last_checked) >= YT_DLP_UPDATE_INTERVAL.as_secs()
    }

    fn current_managed_binary_path(&self) -> Option<PathBuf> {
        let snapshot = self.snapshot();
        snapshot.state.current_version.and_then(|version| {
            let path = managed_binary_path(&snapshot.managed_root, &version);
            path.is_file().then_some(path)
        })
    }

    fn reload_persisted_state(&self) {
        let mut state = recover_lock(&self.inner);
        state.settings = state.settings_store.load().unwrap_or_default();
        state.state = load_state(&state.state_path).unwrap_or_default();
    }

    fn ensure_directories(&self) -> io::Result<()> {
        let snapshot = self.snapshot();
        fs::create_dir_all(snapshot.managed_root.join(VERSIONS_DIR_NAME))?;
        fs::create_dir_all(snapshot.managed_root.join(STAGING_DIR_NAME))
    }

    fn promote_pending_if_ready(&self) -> io::Result<()> {
        let snapshot = self.snapshot();
        let Some(pending_version) = snapshot.state.pending_version.clone() else {
            return Ok(());
        };
        let pending_path = managed_binary_path(&snapshot.managed_root, &pending_version);
        if !pending_path.is_file() {
            return Ok(());
        }

        let mut next_state = snapshot.state;
        next_state.previous_version = next_state.current_version.clone();
        next_state.current_version = Some(pending_version);
        next_state.pending_version = None;
        self.persist_state(next_state)
    }

    fn bootstrap_managed_binary_if_missing(&self) -> io::Result<()> {
        if self.current_managed_binary_path().is_some() {
            return Ok(());
        }

        let Some(source_path) = self.bootstrap_source_path() else {
            return Ok(());
        };

        let snapshot = self.snapshot();
        let version = self
            .read_binary_version(&source_path)
            .unwrap_or_else(|| String::from("bootstrap"));
        let target_path = managed_binary_path(&snapshot.managed_root, &version);
        if source_path != target_path {
            if let Some(parent) = target_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source_path, &target_path)?;
        }

        let mut next_state = snapshot.state;
        next_state.current_version = Some(version);
        self.persist_state(next_state)
    }

    fn bootstrap_source_path(&self) -> Option<PathBuf> {
        if let Some(path) = env::var_os(YT_DLP_PATH_ENV) {
            let path = PathBuf::from(path);
            if path.is_file() {
                return Some(path);
            }
        }

        if let Some(path) = sibling_binary_candidate() {
            return Some(path);
        }

        let windows_fallback = PathBuf::from(DEFAULT_WINDOWS_YT_DLP_PATH);
        if windows_fallback.is_file() {
            return Some(windows_fallback);
        }

        resolve_binary_from_path()
    }

    fn read_binary_version(&self, binary_path: &Path) -> Option<String> {
        let output = Command::new(binary_path)
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?
            .wait_with_output()
            .ok()?;
        if !output.status.success() {
            return None;
        }

        String::from_utf8(output.stdout)
            .ok()?
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(str::to_owned)
    }

    fn stage_binary(&self, version: &str, bytes: &[u8]) -> io::Result<PathBuf> {
        let snapshot = self.snapshot();
        let staging_dir = snapshot.managed_root.join(STAGING_DIR_NAME).join(version);
        fs::create_dir_all(&staging_dir)?;
        let staging_path = staging_dir.join(managed_binary_file_name());
        fs::write(&staging_path, bytes)?;
        Ok(staging_path)
    }

    fn smoke_test_binary(&self, binary_path: &Path) -> io::Result<()> {
        let mut child = Command::new(binary_path)
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let started_at = std::time::Instant::now();

        loop {
            match child.try_wait()? {
                Some(status) if status.success() => return Ok(()),
                Some(status) => {
                    return Err(io::Error::other(format!(
                        "process exited unsuccessfully: {status}"
                    )));
                }
                None if started_at.elapsed() >= YT_DLP_COMMAND_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "yt-dlp smoke test timed out",
                    ));
                }
                None => std::thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn commit_staged_version(&self, version: String, etag: Option<String>) -> io::Result<()> {
        let snapshot = self.snapshot();
        let staging_path = snapshot
            .managed_root
            .join(STAGING_DIR_NAME)
            .join(&version)
            .join(managed_binary_file_name());
        let version_path = managed_binary_path(&snapshot.managed_root, &version);
        if let Some(parent) = version_path.parent() {
            fs::create_dir_all(parent)?;
        }
        if staging_path != version_path {
            fs::copy(&staging_path, &version_path)?;
        }

        let mut next_state = snapshot.state;
        next_state.pending_version = Some(version);
        self.persist_state(next_state)?;
        self.mark_checked(etag)
    }

    fn mark_checked(&self, etag: Option<String>) -> io::Result<()> {
        let mut state = recover_lock(&self.inner);
        state.settings.last_checked_unix_s = Some(unix_timestamp_now());
        if etag.is_some() {
            state.settings.release_etag = etag;
        }
        state.settings_store.save(&state.settings)
    }

    fn persist_state(&self, next_state: ManagedBinaryState) -> io::Result<()> {
        let mut state = recover_lock(&self.inner);
        if let Some(parent) = state.state_path.parent() {
            fs::create_dir_all(parent)?;
        }
        save_state(&state.state_path, &next_state)?;
        state.state = next_state;
        Ok(())
    }

    fn snapshot(&self) -> ManagerSnapshot {
        let state = recover_lock(&self.inner);
        ManagerSnapshot {
            managed_root: state.managed_root.clone(),
            settings: state.settings.clone(),
            state: state.state.clone(),
        }
    }
}

#[derive(Debug, Clone)]
struct ManagerSnapshot {
    managed_root: PathBuf,
    settings: LocalYtDlpSettings,
    state: ManagedBinaryState,
}

impl LocalYtDlpManager {
    #[must_use]
    pub fn current_version(&self) -> Option<String> {
        self.snapshot().state.current_version
    }
}

fn load_state(path: &Path) -> io::Result<ManagedBinaryState> {
    match fs::read_to_string(path) {
        Ok(contents) => serde_json::from_str::<ManagedBinaryState>(&contents)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(ManagedBinaryState::default()),
        Err(error) => Err(error),
    }
}

fn save_state(path: &Path, state: &ManagedBinaryState) -> io::Result<()> {
    let payload = serde_json::to_vec_pretty(state)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    fs::write(path, payload)
}

fn default_settings_path() -> io::Result<PathBuf> {
    Ok(default_config_dir()?.join(SETTINGS_FILE_NAME))
}

fn default_config_dir() -> io::Result<PathBuf> {
    let config_dir = if let Some(path) = env::var_os(CONFIG_DIR_ENV) {
        PathBuf::from(path)
    } else if cfg!(windows) {
        env::var_os("APPDATA")
            .map(PathBuf::from)
            .unwrap_or(env::current_dir()?.join(".nocturne"))
            .join("Nocturne")
    } else if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(path).join("nocturne")
    } else if let Some(path) = env::var_os("HOME") {
        PathBuf::from(path).join(".config").join("nocturne")
    } else {
        env::current_dir()?.join(".nocturne")
    };

    Ok(config_dir)
}

fn fallback_config_dir() -> PathBuf {
    default_config_dir().unwrap_or_else(|_| PathBuf::from(".nocturne"))
}

fn sibling_binary_candidate() -> Option<PathBuf> {
    let current_exe = env::current_exe().ok()?;
    let candidate = current_exe.parent()?.join(managed_binary_file_name());
    candidate.is_file().then_some(candidate)
}

fn resolve_binary_from_path() -> Option<PathBuf> {
    let locator = if cfg!(windows) { "where" } else { "which" };
    let output = Command::new(locator)
        .arg(DEFAULT_YT_DLP_BINARY)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(PathBuf::from)
        .filter(|path| path.is_file())
}

fn release_url_for_channel(channel: YtDlpReleaseChannel) -> &'static str {
    match channel {
        YtDlpReleaseChannel::Stable => STABLE_RELEASE_URL,
    }
}

fn release_asset_name() -> &'static str {
    if cfg!(windows) {
        match env::consts::ARCH {
            "x86" => "yt-dlp_x86.exe",
            "aarch64" => "yt-dlp_arm64.exe",
            _ => "yt-dlp.exe",
        }
    } else {
        "yt-dlp"
    }
}

fn managed_binary_file_name() -> &'static str {
    if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    }
}

fn managed_binary_path(root: &Path, version: &str) -> PathBuf {
    root.join(VERSIONS_DIR_NAME)
        .join(version)
        .join(managed_binary_file_name())
}

fn parse_checksum_file(contents: &[u8], file_name: &str) -> Option<String> {
    let text = std::str::from_utf8(contents).ok()?;
    text.lines().find_map(|line| {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let name = parts.next()?;
        (name == file_name).then(|| hash.to_owned())
    })
}

fn compute_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn read_stream(mut stream: impl io::Read) -> io::Result<Vec<u8>> {
    let mut buffer = Vec::new();
    stream.read_to_end(&mut buffer)?;
    Ok(buffer)
}

fn unix_timestamp_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let unique = format!("{}-{}-{}", name, std::process::id(), unix_timestamp_now());
        std::env::temp_dir().join(unique)
    }

    #[test]
    fn settings_store_round_trips() {
        let path = temp_path("nocturne-yt-dlp-settings.json");
        let store = LocalYtDlpSettingsStore::from_path(path.clone());
        let settings = LocalYtDlpSettings {
            auto_update: false,
            channel: YtDlpReleaseChannel::Stable,
            last_checked_unix_s: Some(123),
            release_etag: Some(String::from("etag")),
        };

        store.save(&settings).unwrap();
        let loaded = store.load().unwrap();
        assert_eq!(loaded, settings);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn prepare_promotes_pending_versions() {
        let config_dir = temp_path("nocturne-yt-dlp-config");
        let store = LocalYtDlpSettingsStore::from_path(config_dir.join(SETTINGS_FILE_NAME));
        let manager = LocalYtDlpManager::new(reqwest::Client::new(), store.clone());
        let managed_root = store.path().parent().unwrap().join(MANAGED_ROOT_DIR_NAME);
        let pending_path = managed_binary_path(&managed_root, "2026.05.01");
        fs::create_dir_all(pending_path.parent().unwrap()).unwrap();
        fs::write(&pending_path, b"binary").unwrap();
        save_state(
            &managed_root.join(STATE_FILE_NAME),
            &ManagedBinaryState {
                current_version: Some(String::from("2026.04.01")),
                previous_version: None,
                pending_version: Some(String::from("2026.05.01")),
            },
        )
        .unwrap();

        manager.prepare().unwrap();

        let state = load_state(&managed_root.join(STATE_FILE_NAME)).unwrap();
        assert_eq!(state.current_version.as_deref(), Some("2026.05.01"));
        assert_eq!(state.previous_version.as_deref(), Some("2026.04.01"));
        assert_eq!(state.pending_version, None);

        let _ = fs::remove_dir_all(config_dir);
    }

    #[test]
    fn executable_path_prefers_managed_current_version() {
        let config_dir = temp_path("nocturne-yt-dlp-managed");
        let store = LocalYtDlpSettingsStore::from_path(config_dir.join(SETTINGS_FILE_NAME));
        let manager = LocalYtDlpManager::new(reqwest::Client::new(), store.clone());
        let managed_root = store.path().parent().unwrap().join(MANAGED_ROOT_DIR_NAME);
        let managed_path = managed_binary_path(&managed_root, "2026.05.01");
        fs::create_dir_all(managed_path.parent().unwrap()).unwrap();
        fs::write(&managed_path, b"binary").unwrap();
        save_state(
            &managed_root.join(STATE_FILE_NAME),
            &ManagedBinaryState {
                current_version: Some(String::from("2026.05.01")),
                previous_version: None,
                pending_version: None,
            },
        )
        .unwrap();

        manager.reload_persisted_state();
        let path = PathBuf::from(manager.executable_path());

        assert_eq!(path, managed_path);
        let _ = fs::remove_dir_all(config_dir);
    }

    #[test]
    fn checksum_parser_returns_named_hash() {
        let checksums = b"abc123  yt-dlp.exe\ndef456  yt-dlp_x86.exe\n";
        assert_eq!(
            parse_checksum_file(checksums, "yt-dlp.exe").as_deref(),
            Some("abc123")
        );
        assert!(parse_checksum_file(checksums, "missing.exe").is_none());
    }

    #[test]
    fn apply_pending_update_promotes_staged_version() {
        let config_dir = temp_path("nocturne-yt-dlp-apply-pending");
        let store = LocalYtDlpSettingsStore::from_path(config_dir.join(SETTINGS_FILE_NAME));
        let manager = LocalYtDlpManager::new(reqwest::Client::new(), store.clone());
        let managed_root = store.path().parent().unwrap().join(MANAGED_ROOT_DIR_NAME);
        let pending_path = managed_binary_path(&managed_root, "2026.05.01");
        fs::create_dir_all(pending_path.parent().unwrap()).unwrap();
        fs::write(&pending_path, b"binary").unwrap();
        save_state(
            &managed_root.join(STATE_FILE_NAME),
            &ManagedBinaryState {
                current_version: Some(String::from("2026.04.01")),
                previous_version: None,
                pending_version: Some(String::from("2026.05.01")),
            },
        )
        .unwrap();

        manager.reload_persisted_state();
        let promoted = manager.apply_pending_update().unwrap();

        assert!(promoted);
        assert_eq!(manager.current_version().as_deref(), Some("2026.05.01"));

        let state = load_state(&managed_root.join(STATE_FILE_NAME)).unwrap();
        assert_eq!(state.current_version.as_deref(), Some("2026.05.01"));
        assert_eq!(state.previous_version.as_deref(), Some("2026.04.01"));
        assert_eq!(state.pending_version, None);

        let _ = fs::remove_dir_all(config_dir);
    }
}
