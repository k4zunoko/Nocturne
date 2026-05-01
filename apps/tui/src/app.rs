use std::net::SocketAddr;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nocturne_api::{BackendStatus, SearchJobStatus, SearchJobSummary, StateSnapshot};
use nocturne_domain::{
    AudioSettings, PlaybackState, PlaybackStatus, QueueItem, QueueItemStatus, Song,
};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::backend::{
    BackendEvent, BackendEventKind, CommandAction, CommandOutcome, SearchResultsOutcome,
    SnapshotSeed,
};

const MAX_SEARCH_RESULTS: usize = 5;

#[derive(Debug)]
pub enum AppEvent {
    Input(KeyEvent),
    Backend(BackendEvent),
    Snapshot(SnapshotSeed),
    CommandResult(Result<CommandOutcome, String>),
    SearchResults(SearchResultsOutcome),
    ConnectionStatus(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Queue,
    Search,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SearchOverlayState {
    Loading,
    Results,
    Empty,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchOverlay {
    job_id: String,
    query: String,
    state: SearchOverlayState,
    results: Vec<Song>,
    selected: usize,
    error_message: Option<String>,
    closed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PendingSearchTerminalState {
    Completed(Vec<Song>),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InputState {
    text: String,
    cursor: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppAction {
    None,
    Quit,
    Command(CommandAction),
}

pub struct App {
    backend_addr: SocketAddr,
    backend: BackendStatus,
    playback: PlaybackState,
    audio: AudioSettings,
    current_song: Option<Song>,
    queue: Vec<QueueItem>,
    search_jobs: Vec<SearchJobSummary>,
    overlay: Overlay,
    queue_selection: usize,
    pending_delete_confirmation: bool,
    input: InputState,
    search_overlay: Option<SearchOverlay>,
    pending_search_terminal: Option<(String, PendingSearchTerminalState)>,
    pending_search_results_request: Option<String>,
    status_message: String,
    status_level: StatusLevel,
    last_event_id: Option<String>,
    display_position_ms: u64,
    playback_anchor_ms: u64,
    playback_anchor_at: Instant,
    playback_progress_confirmed: bool,
}

impl App {
    pub fn new(backend_addr: SocketAddr) -> Self {
        Self {
            backend_addr,
            backend: BackendStatus {
                ready: false,
                version: None,
            },
            playback: PlaybackState {
                state: PlaybackStatus::Stopped,
                position_ms: 0,
                current_queue_item_id: None,
                playback_session_id: None,
            },
            audio: AudioSettings::default(),
            current_song: None,
            queue: Vec::new(),
            search_jobs: Vec::new(),
            overlay: Overlay::None,
            queue_selection: 0,
            pending_delete_confirmation: false,
            input: InputState::default(),
            search_overlay: None,
            pending_search_terminal: None,
            pending_search_results_request: None,
            status_message: String::from("Starting backend..."),
            status_level: StatusLevel::Info,
            last_event_id: None,
            display_position_ms: 0,
            playback_anchor_ms: 0,
            playback_anchor_at: Instant::now(),
            playback_progress_confirmed: false,
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: StateSnapshot, current_event_id: Option<String>) {
        self.backend = snapshot.backend;
        self.playback = snapshot.playback;
        self.audio = snapshot.audio;
        self.current_song = snapshot.current_song;
        self.queue = snapshot.queue;
        self.search_jobs = snapshot.search_jobs;
        self.last_event_id = current_event_id;
        self.sync_playback_anchor(self.playback.position_ms);
        self.playback_progress_confirmed =
            self.playback.state == PlaybackStatus::Playing && self.playback.position_ms > 0;
        self.sync_current_song_from_queue();
        self.reconcile_queue_selection();
        self.set_status(
            StatusLevel::Info,
            format!("Connected to backend on http://{}", self.backend_addr),
        );
        self.reconcile_search_overlay_from_jobs();
    }

    pub fn apply_backend_event(&mut self, event: BackendEvent) {
        self.last_event_id = Some(event.event_id);

        match event.kind {
            BackendEventKind::StateUpdated(snapshot) => {
                self.backend = snapshot.backend;
                self.playback = snapshot.playback;
                self.audio = snapshot.audio;
                self.current_song = snapshot.current_song;
                self.queue = snapshot.queue;
                self.search_jobs = snapshot.search_jobs;
                self.sync_playback_anchor(self.playback.position_ms);
                self.playback_progress_confirmed =
                    self.playback.state == PlaybackStatus::Playing && self.playback.position_ms > 0;
                self.sync_current_song_from_queue();
                self.reconcile_queue_selection();
                self.reconcile_search_overlay_from_jobs();
            }
            BackendEventKind::PlaybackProgress(payload) => {
                if self.playback.playback_session_id.is_none() {
                    self.playback.playback_session_id = Some(payload.playback_session_id.clone());
                }
                if self.playback.playback_session_id.as_deref()
                    != Some(payload.playback_session_id.as_str())
                {
                    return;
                }
                self.playback.position_ms = payload.position_ms;
                self.playback_progress_confirmed = true;
                self.sync_playback_anchor(payload.position_ms);
            }
            BackendEventKind::SearchJobStarted(job) => {
                self.upsert_search_job(job.clone());
                self.apply_search_job_started(job);
            }
            BackendEventKind::SearchJobCompleted(payload) => {
                self.upsert_search_job(payload.job.clone());
                self.apply_search_job_completed(payload.job.job_id, payload.results);
            }
            BackendEventKind::SearchJobFailed(payload) => {
                if let Some(job) = self
                    .search_jobs
                    .iter_mut()
                    .find(|job| job.job_id == payload.job_id)
                {
                    job.status = SearchJobStatus::Failed;
                }
                self.apply_search_job_failed(payload.job_id, payload.message);
            }
            BackendEventKind::SystemError(payload) => {
                let level = match payload.severity {
                    nocturne_api::SystemErrorSeverity::Warning => StatusLevel::Warning,
                    nocturne_api::SystemErrorSeverity::Error => StatusLevel::Error,
                };
                self.set_status(level, payload.message);
            }
        }
    }

    pub fn apply_command_result(&mut self, result: Result<CommandOutcome, String>) {
        match result {
            Ok(CommandOutcome::StatusMessage(message)) => {
                self.set_status(StatusLevel::Info, message)
            }
            Ok(CommandOutcome::SearchSubmitted { job_id, query }) => {
                self.open_search_overlay(job_id, query);
                self.apply_pending_terminal_state();
                self.reconcile_search_overlay_from_jobs();
                if self.search_overlay_state() == Some(SearchOverlayState::Loading) {
                    self.set_status(StatusLevel::Info, String::from("Searching backend..."));
                }
            }
            Ok(CommandOutcome::SongQueued(message)) => {
                self.overlay = Overlay::None;
                self.search_overlay = None;
                self.input.clear();
                self.set_status(StatusLevel::Info, message);
            }
            Err(message) => self.set_status(StatusLevel::Error, message),
        }
    }

    pub fn note_connection_status(&mut self, message: String) {
        self.set_status(StatusLevel::Warning, message);
    }

    pub fn apply_search_results(&mut self, outcome: SearchResultsOutcome) {
        self.pending_search_results_request = None;
        match outcome {
            SearchResultsOutcome::Loaded { job, results } => {
                self.upsert_search_job(job.clone());
                self.apply_search_job_completed(job.job_id, results);
            }
            SearchResultsOutcome::Failed { job_id, message } => {
                self.apply_search_job_failed(job_id, message);
            }
        }
    }

    pub fn tick(&mut self) {
        self.display_position_ms = self.current_display_position_ms();
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> AppAction {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => return AppAction::Quit,
                KeyCode::Char('q') => {
                    self.open_queue_overlay();
                    return AppAction::None;
                }
                KeyCode::Char('p') => return AppAction::Command(CommandAction::PlayPause),
                KeyCode::Up => return self.adjust_volume(5),
                KeyCode::Down => return self.adjust_volume(-5),
                KeyCode::Left => return AppAction::Command(CommandAction::Previous),
                KeyCode::Right => return AppAction::Command(CommandAction::Next),
                _ => {}
            }
        }

        if matches!(key.code, KeyCode::F(1)) {
            self.toggle_help_overlay();
            return AppAction::None;
        }

        match self.overlay {
            Overlay::None => self.handle_input_key(key),
            Overlay::Queue => self.handle_queue_overlay_key(key),
            Overlay::Search => self.handle_search_overlay_key(key),
            Overlay::Help => self.handle_help_overlay_key(key),
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5),
                Constraint::Length(4),
                Constraint::Min(6),
                Constraint::Length(3),
                Constraint::Length(1),
            ])
            .split(area);

        self.render_now_playing_widget(frame, layout[0]);
        frame.render_widget(self.search_input_widget(), layout[1]);
        frame.render_widget(self.queue_summary_widget(), layout[2]);
        frame.render_widget(self.status_widget(), layout[3]);
        frame.render_widget(self.shortcuts_widget(), layout[4]);

        match self.overlay {
            Overlay::None => {}
            Overlay::Queue => self.render_queue_overlay(frame, area),
            Overlay::Search => self.render_search_overlay(frame, area),
            Overlay::Help => self.render_help_overlay(frame, area),
        }
    }

    pub fn last_event_id(&self) -> Option<String> {
        self.last_event_id.clone()
    }

    pub fn take_search_results_request(&mut self) -> Option<String> {
        self.pending_search_results_request.take()
    }

    fn handle_input_key(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Enter => self.submit_input(),
            KeyCode::Backspace => {
                self.input.delete_before_cursor();
                AppAction::None
            }
            KeyCode::Delete => {
                self.input.delete_at_cursor();
                AppAction::None
            }
            KeyCode::Left => {
                self.input.move_left();
                AppAction::None
            }
            KeyCode::Right => {
                self.input.move_right();
                AppAction::None
            }
            KeyCode::Home => {
                self.input.move_home();
                AppAction::None
            }
            KeyCode::End => {
                self.input.move_end();
                AppAction::None
            }
            KeyCode::Char(ch)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.input.insert(ch);
                AppAction::None
            }
            _ => AppAction::None,
        }
    }

    fn handle_queue_overlay_key(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.overlay = Overlay::None;
                self.pending_delete_confirmation = false;
                AppAction::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.pending_delete_confirmation = false;
                self.move_queue_selection(-1);
                AppAction::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.pending_delete_confirmation = false;
                self.move_queue_selection(1);
                AppAction::None
            }
            KeyCode::Char('d') => {
                if let Some(item) = self.selected_queue_item() {
                    if self.is_current_item(&item.id) {
                        self.set_status(
                            StatusLevel::Warning,
                            String::from("The currently playing item cannot be removed."),
                        );
                    } else {
                        let title = item.song.title.clone();
                        self.pending_delete_confirmation = true;
                        self.set_status(
                            StatusLevel::Warning,
                            format!("Press Enter to remove '{}'.", title),
                        );
                    }
                }
                AppAction::None
            }
            KeyCode::Enter => {
                if self.pending_delete_confirmation {
                    self.pending_delete_confirmation = false;
                    if let Some(item) = self.selected_queue_item() {
                        return AppAction::Command(CommandAction::RemoveQueueItem(item.id.clone()));
                    }
                }
                AppAction::None
            }
            _ => AppAction::None,
        }
    }

    fn handle_search_overlay_key(&mut self, key: KeyEvent) -> AppAction {
        let Some(search) = self.search_overlay.as_mut() else {
            self.overlay = Overlay::None;
            return AppAction::None;
        };

        match search.state {
            SearchOverlayState::Loading => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    search.closed = true;
                    self.overlay = Overlay::None;
                    AppAction::None
                }
                _ => AppAction::None,
            },
            SearchOverlayState::Results => match key.code {
                KeyCode::Esc | KeyCode::Char('q') => {
                    search.closed = true;
                    self.overlay = Overlay::None;
                    AppAction::None
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    search.move_selection(-1);
                    AppAction::None
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    search.move_selection(1);
                    AppAction::None
                }
                KeyCode::Enter => search
                    .selected_song()
                    .map(|song| AppAction::Command(CommandAction::AddSong(song.id.clone())))
                    .unwrap_or(AppAction::None),
                _ => AppAction::None,
            },
            SearchOverlayState::Empty | SearchOverlayState::Error => match key.code {
                KeyCode::Enter | KeyCode::Esc | KeyCode::Char('q') => {
                    search.closed = true;
                    self.overlay = Overlay::None;
                    self.input.clear();
                    AppAction::None
                }
                _ => AppAction::None,
            },
        }
    }

    fn handle_help_overlay_key(&mut self, key: KeyEvent) -> AppAction {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::F(1) => {
                self.overlay = Overlay::None;
                AppAction::None
            }
            _ => AppAction::None,
        }
    }

    fn open_queue_overlay(&mut self) {
        self.dismiss_search_overlay();
        self.overlay = Overlay::Queue;
        self.pending_delete_confirmation = false;
        self.queue_selection = self.initial_queue_selection();
    }

    fn open_search_overlay(&mut self, job_id: String, query: String) {
        self.search_overlay = Some(SearchOverlay {
            job_id,
            query,
            state: SearchOverlayState::Loading,
            results: Vec::new(),
            selected: 0,
            error_message: None,
            closed: false,
        });
        self.overlay = Overlay::Search;
    }

    fn search_overlay_state(&self) -> Option<SearchOverlayState> {
        self.search_overlay
            .as_ref()
            .map(|search| search.state.clone())
    }

    fn toggle_help_overlay(&mut self) {
        if self.overlay != Overlay::Help {
            self.dismiss_search_overlay();
        }
        self.overlay = if self.overlay == Overlay::Help {
            Overlay::None
        } else {
            Overlay::Help
        };
    }

    fn initial_queue_selection(&self) -> usize {
        if self.queue.is_empty() {
            return 0;
        }

        match self.current_queue_index() {
            Some(index) if index + 1 < self.queue.len() => index + 1,
            Some(index) => index,
            None => 0,
        }
    }

    fn reconcile_queue_selection(&mut self) {
        if self.queue.is_empty() {
            self.queue_selection = 0;
            self.pending_delete_confirmation = false;
        } else if self.queue_selection >= self.queue.len() {
            self.queue_selection = self.queue.len() - 1;
        }
    }

    fn move_queue_selection(&mut self, delta: isize) {
        if self.queue.is_empty() {
            self.queue_selection = 0;
            return;
        }

        let max_index = self.queue.len() as isize - 1;
        let next_index = (self.queue_selection as isize + delta).clamp(0, max_index);
        self.queue_selection = next_index as usize;
    }

    fn current_queue_index(&self) -> Option<usize> {
        let current_id = self.playback.current_queue_item_id.as_deref()?;
        self.queue.iter().position(|item| item.id == current_id)
    }

    fn selected_queue_item(&self) -> Option<&QueueItem> {
        self.queue.get(self.queue_selection)
    }

    fn is_current_item(&self, queue_item_id: &str) -> bool {
        self.playback.current_queue_item_id.as_deref() == Some(queue_item_id)
    }

    fn sync_current_song_from_queue(&mut self) {
        let current_id = self.playback.current_queue_item_id.as_deref();
        self.current_song = current_id.and_then(|id| {
            self.queue
                .iter()
                .find(|item| item.id == id)
                .map(|item| item.song.clone())
        });
    }

    fn submit_input(&mut self) -> AppAction {
        let text = self.input.text.trim().to_owned();
        if text.is_empty() {
            self.set_status(
                StatusLevel::Warning,
                String::from("Enter a search query first."),
            );
            return AppAction::None;
        }

        if let Some(command) = text.strip_prefix('/') {
            return self.handle_slash_command(command.trim());
        }

        AppAction::Command(CommandAction::Search(text))
    }

    fn handle_slash_command(&mut self, command: &str) -> AppAction {
        match command {
            "exit" => AppAction::Quit,
            "help" => {
                self.overlay = Overlay::Help;
                AppAction::None
            }
            _ => {
                self.set_status(
                    StatusLevel::Warning,
                    String::from("Unknown slash command. Try /help or /exit."),
                );
                AppAction::None
            }
        }
    }

    fn upsert_search_job(&mut self, job: SearchJobSummary) {
        if let Some(existing) = self
            .search_jobs
            .iter_mut()
            .find(|existing| existing.job_id == job.job_id)
        {
            *existing = job;
        } else {
            self.search_jobs.push(job);
        }
    }

    fn apply_search_job_started(&mut self, job: SearchJobSummary) {
        if let Some(search) = self.search_overlay.as_mut()
            && search.job_id == job.job_id
            && !search.closed
        {
            search.state = SearchOverlayState::Loading;
            search.error_message = None;
        }
    }

    fn apply_search_job_completed(&mut self, job_id: String, results: Vec<Song>) {
        if let Some(search) = self.search_overlay.as_mut()
            && search.job_id == job_id
        {
            if search.closed {
                return;
            }
            search.results = results.into_iter().take(MAX_SEARCH_RESULTS).collect();
            search.selected = 0;
            search.error_message = None;
            search.state = if search.results.is_empty() {
                SearchOverlayState::Empty
            } else {
                SearchOverlayState::Results
            };
            self.overlay = Overlay::Search;
            self.pending_search_terminal = None;
            self.pending_search_results_request = None;
            let status_message = if search.results.is_empty() {
                String::from("No matches found.")
            } else {
                format!("Found {} candidate(s).", search.results.len())
            };
            let _ = search;
            self.set_status(StatusLevel::Info, status_message);
        } else {
            self.pending_search_terminal =
                Some((job_id, PendingSearchTerminalState::Completed(results)));
        }
    }

    fn apply_search_job_failed(&mut self, job_id: String, message: String) {
        if let Some(search) = self.search_overlay.as_mut()
            && search.job_id == job_id
        {
            if search.closed {
                return;
            }
            search.results.clear();
            search.error_message = Some(message.clone());
            search.state = SearchOverlayState::Error;
            self.overlay = Overlay::Search;
            self.pending_search_terminal = None;
            self.pending_search_results_request = None;
            self.set_status(StatusLevel::Error, message);
        } else {
            self.pending_search_terminal =
                Some((job_id, PendingSearchTerminalState::Failed(message)));
        }
    }

    fn dismiss_search_overlay(&mut self) {
        if let Some(search) = self.search_overlay.as_mut() {
            search.closed = true;
        }
    }

    fn apply_pending_terminal_state(&mut self) {
        let Some((job_id, state)) = self.pending_search_terminal.clone() else {
            return;
        };

        let Some(search) = self.search_overlay.as_ref() else {
            return;
        };

        if search.job_id != job_id || search.closed {
            return;
        }

        match state {
            PendingSearchTerminalState::Completed(results) => {
                self.apply_search_job_completed(job_id, results);
            }
            PendingSearchTerminalState::Failed(message) => {
                self.apply_search_job_failed(job_id, message);
            }
        }
    }

    fn reconcile_search_overlay_from_jobs(&mut self) {
        let Some(search) = self.search_overlay.as_ref() else {
            return;
        };

        if search.closed {
            return;
        }

        let Some(job) = self
            .search_jobs
            .iter()
            .find(|job| job.job_id == search.job_id)
            .cloned()
        else {
            return;
        };

        match job.status {
            SearchJobStatus::Completed => {
                if matches!(
                    self.search_overlay_state(),
                    Some(SearchOverlayState::Loading)
                ) {
                    self.pending_search_results_request = Some(job.job_id);
                    self.set_status(
                        StatusLevel::Info,
                        String::from("Refreshing search results..."),
                    );
                }
            }
            SearchJobStatus::Failed => {
                self.apply_search_job_failed(
                    job.job_id,
                    String::from("Search failed. Reconnect completed without a detailed error."),
                );
            }
            SearchJobStatus::Queued | SearchJobStatus::Running => {
                self.apply_pending_terminal_state();
            }
        }
    }

    fn sync_playback_anchor(&mut self, position_ms: u64) {
        self.playback_anchor_ms = position_ms;
        self.display_position_ms = position_ms;
        self.playback_anchor_at = Instant::now();
    }

    fn current_display_position_ms(&self) -> u64 {
        if self.playback.state != PlaybackStatus::Playing {
            return self.playback_anchor_ms;
        }

        if !self.playback_progress_confirmed {
            return self.playback_anchor_ms;
        }

        let elapsed_ms = self.playback_anchor_at.elapsed().as_millis() as u64;
        let position_ms = self.playback_anchor_ms.saturating_add(elapsed_ms);
        let duration_ms = self
            .current_song
            .as_ref()
            .map_or(u64::MAX, |song| song.duration_ms);
        position_ms.min(duration_ms)
    }

    fn render_now_playing_widget(&self, frame: &mut Frame<'_>, area: Rect) {
        let title = self.current_song.as_ref().map_or_else(
            || String::from("Nothing queued yet"),
            |song| song.title.clone(),
        );
        let channel = self.current_song.as_ref().map_or_else(
            || String::from("Add a song from search once that flow is wired."),
            |song| song.channel_name.clone(),
        );
        let state_label = match self.playback.state {
            PlaybackStatus::Stopped => "Stopped",
            PlaybackStatus::Loading => "Loading",
            PlaybackStatus::Playing => "Playing",
            PlaybackStatus::Paused => "Paused",
        };
        let duration_ms = self
            .current_song
            .as_ref()
            .map_or(1, |song| song.duration_ms.max(1));
        let block = Block::default().title("Playback").borders(Borders::ALL);
        let inner = block.inner(area);
        frame.render_widget(block, area);

        if inner.width == 0 || inner.height == 0 {
            return;
        }

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .split(inner);

        let state_style = match self.playback.state {
            PlaybackStatus::Stopped => Style::default().fg(Color::Gray),
            PlaybackStatus::Loading => Style::default().fg(Color::Yellow),
            PlaybackStatus::Playing => Style::default().fg(Color::LightGreen),
            PlaybackStatus::Paused => Style::default().fg(Color::Yellow),
        };
        let state_width = text_width(state_label);
        let volume_label = format!("Vol {}%", self.audio.volume_percent);
        let volume_width = text_width(&volume_label);
        let time_label = format!(
            "{} / {}",
            format_duration(self.display_position_ms),
            format_duration(duration_ms)
        );

        let title_row = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(state_width),
            ])
            .split(rows[0]);
        frame.render_widget(
            Line::from(vec![
                Span::styled("Title ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(title),
            ]),
            title_row[0],
        );
        frame.render_widget(
            Paragraph::new(Line::styled(state_label, state_style).right_aligned()),
            title_row[2],
        );

        let channel_row = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(volume_width),
            ])
            .split(rows[1]);
        frame.render_widget(
            Line::from(vec![
                Span::styled("Channel ", Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(channel, Style::default().fg(Color::Gray)),
            ]),
            channel_row[0],
        );
        frame.render_widget(
            Paragraph::new(
                Line::from(vec![
                    Span::styled("Vol ", Style::default().fg(Color::Gray)),
                    Span::raw(format!("{}%", self.audio.volume_percent)),
                ])
                .right_aligned(),
            ),
            channel_row[2],
        );

        let reserved_time_width = text_width(&time_label).saturating_add(1).min(rows[2].width);
        let progress_row = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Length(reserved_time_width), Constraint::Min(0)])
            .split(rows[2]);
        frame.render_widget(Line::from(format!("{time_label} ")), progress_row[0],);
        frame.render_widget(
            Line::from(progress_bar_spans(
                self.display_position_ms,
                duration_ms,
                usize::from(progress_row[1].width),
            )),
            progress_row[1],
        );
    }

    fn search_input_widget(&self) -> Paragraph<'static> {
        let (before_cursor, after_cursor) = self.input.split_for_render();
        let mut lines = vec![Line::from(vec![
            Span::styled("Query ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(before_cursor, Style::default().fg(Color::White)),
            Span::styled("▏", Style::default().fg(Color::Rgb(132, 146, 166))),
            Span::styled(after_cursor, Style::default().fg(Color::White)),
        ])];

        if self.current_song.is_none() && self.queue.is_empty() {
            lines.push(Line::from(
                "Summon a song by name, artist, or a few remembered words.",
            ));
        } else {
            lines.push(Line::from(
                "Enter searches • /help shows commands • results open in the center overlay",
            ));
        }

        Paragraph::new(lines)
            .block(Block::default().title("Search").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn queue_summary_widget(&self) -> Paragraph<'static> {
        let mut lines = Vec::new();
        if self.queue.is_empty() {
            lines.push(Line::from("Queue is empty."));
        } else {
            for item in self.queue.iter().take(8) {
                lines.push(Line::from(self.format_queue_row(item, false)));
            }
            if self.queue.len() > 8 {
                lines.push(Line::from(format!(
                    "… and {} more items. Open Queue with Ctrl+Q.",
                    self.queue.len() - 8
                )));
            }
        }

        Paragraph::new(lines)
            .block(
                Block::default()
                    .title("Queue Snapshot")
                    .borders(Borders::ALL),
            )
            .wrap(Wrap { trim: false })
    }

    fn status_widget(&self) -> Paragraph<'static> {
        let color = match self.status_level {
            StatusLevel::Info => Color::Gray,
            StatusLevel::Warning => Color::Yellow,
            StatusLevel::Error => Color::LightRed,
        };
        let version = self
            .backend
            .version
            .clone()
            .unwrap_or_else(|| String::from("unknown"));
        let readiness = if self.backend.ready {
            "ready"
        } else {
            "starting"
        };
        let message = Line::from(vec![
            Span::styled("Backend ", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(format!("{} ({})", readiness, version)),
            Span::raw("  •  "),
            Span::styled(self.status_message.clone(), Style::default().fg(color)),
        ]);

        Paragraph::new(vec![message])
            .block(Block::default().title("Status").borders(Borders::ALL))
            .wrap(Wrap { trim: true })
    }

    fn shortcuts_widget(&self) -> Paragraph<'static> {
        Paragraph::new(Line::from(
            "Enter search  Ctrl+P play/pause  Ctrl+Up/Down volume  Ctrl+Left previous  Ctrl+Right next  Ctrl+Q queue  F1 help  /exit quit",
        ))
        .style(Style::default().fg(Color::DarkGray))
    }

    fn render_queue_overlay(&self, frame: &mut Frame<'_>, area: Rect) {
        let overlay_area = centered_rect(70, 70, area);
        frame.render_widget(Clear, overlay_area);

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(4), Constraint::Length(2)])
            .split(overlay_area);

        let items = if self.queue.is_empty() {
            vec![ListItem::new("Queue is empty.")]
        } else {
            self.queue
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    ListItem::new(self.format_queue_row(item, index == self.queue_selection))
                })
                .collect()
        };

        let list = List::new(items)
            .block(Block::default().title("Queue").borders(Borders::ALL))
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(28, 36, 48))
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");

        let mut state = ListState::default();
        if !self.queue.is_empty() {
            state.select(Some(self.queue_selection));
        }
        frame.render_stateful_widget(list, layout[0], &mut state);

        let footer = if self.pending_delete_confirmation {
            "Enter confirms delete • Esc or q closes • Ctrl playback keys stay active"
        } else {
            "↑↓ / j k move • d arm delete • Esc or q closes • Ctrl playback keys stay active"
        };
        frame.render_widget(
            Paragraph::new(footer)
                .block(Block::default().borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM))
                .wrap(Wrap { trim: true }),
            layout[1],
        );
    }

    fn render_search_overlay(&self, frame: &mut Frame<'_>, area: Rect) {
        let Some(search) = self.search_overlay.as_ref() else {
            return;
        };

        let overlay_area = centered_rect(72, 72, area);
        frame.render_widget(Clear, overlay_area);

        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(6),
                Constraint::Length(2),
            ])
            .split(overlay_area);

        let header = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("Search ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(search.query.clone()),
            ]),
            Line::from(match search.state {
                SearchOverlayState::Loading => "Looking up candidates on the backend...",
                SearchOverlayState::Results => "Choose one song to add to the queue.",
                SearchOverlayState::Empty => "No matches found for this query.",
                SearchOverlayState::Error => "Search failed before results became available.",
            }),
        ])
        .block(
            Block::default()
                .title("Search Results")
                .borders(Borders::ALL),
        )
        .wrap(Wrap { trim: true });
        frame.render_widget(header, layout[0]);

        match search.state {
            SearchOverlayState::Loading => {
                frame.render_widget(
                    Paragraph::new(
                        "Searching… you can close this overlay, and late results will stay hidden.",
                    )
                    .block(Block::default().borders(Borders::LEFT | Borders::RIGHT))
                    .wrap(Wrap { trim: true }),
                    layout[1],
                );
            }
            SearchOverlayState::Results => {
                let items = search
                    .results
                    .iter()
                    .enumerate()
                    .map(|(index, song)| {
                        let is_selected = index == search.selected;
                        let mut lines = vec![Line::from(vec![Span::styled(
                            song.title.clone(),
                            if is_selected {
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(Color::White)
                            },
                        )])];
                        if is_selected {
                            lines.push(Line::from(vec![
                                Span::styled(
                                    song.channel_name.clone(),
                                    Style::default().fg(Color::Gray),
                                ),
                                Span::raw(" • "),
                                Span::styled(
                                    format_duration(song.duration_ms),
                                    Style::default().fg(Color::Gray),
                                ),
                            ]));
                        }
                        ListItem::new(lines)
                    })
                    .collect::<Vec<_>>();

                let list = List::new(items)
                    .block(Block::default().borders(Borders::LEFT | Borders::RIGHT))
                    .highlight_style(
                        Style::default()
                            .bg(Color::Rgb(28, 36, 48))
                            .add_modifier(Modifier::BOLD),
                    )
                    .highlight_symbol("› ");

                let mut state = ListState::default();
                state.select(Some(search.selected));
                frame.render_stateful_widget(list, layout[1], &mut state);
            }
            SearchOverlayState::Empty => {
                frame.render_widget(
                    Paragraph::new("Try a more specific song title, artist, or lyric fragment.")
                        .block(Block::default().borders(Borders::LEFT | Borders::RIGHT))
                        .wrap(Wrap { trim: true }),
                    layout[1],
                );
            }
            SearchOverlayState::Error => {
                frame.render_widget(
                    Paragraph::new(
                        search
                            .error_message
                            .clone()
                            .unwrap_or_else(|| String::from("Search failed.")),
                    )
                    .style(Style::default().fg(Color::LightRed))
                    .block(Block::default().borders(Borders::LEFT | Borders::RIGHT))
                    .wrap(Wrap { trim: true }),
                    layout[1],
                );
            }
        }

        let footer = match search.state {
            SearchOverlayState::Loading => "Esc or q closes • backend search keeps running",
            SearchOverlayState::Results => "↑↓ / j k move • Enter adds to queue • Esc or q closes",
            SearchOverlayState::Empty | SearchOverlayState::Error => {
                "Enter, Esc, or q closes • query clears on close"
            }
        };
        frame.render_widget(
            Paragraph::new(footer)
                .block(Block::default().borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM))
                .wrap(Wrap { trim: true }),
            layout[2],
        );
    }

    fn render_help_overlay(&self, frame: &mut Frame<'_>, area: Rect) {
        let overlay_area = centered_rect(64, 56, area);
        frame.render_widget(Clear, overlay_area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from("Search"),
                Line::from("  Enter runs the current query"),
                Line::from("  /help opens this overlay"),
                Line::from("  /exit closes the TUI"),
                Line::from(""),
                Line::from("Playback"),
                Line::from("  Ctrl+P play / pause"),
                Line::from("  Ctrl+Up / Ctrl+Down volume +/- 5%"),
                Line::from("  Ctrl+Left / Ctrl+Right previous / next"),
                Line::from(""),
                Line::from("Overlays"),
                Line::from("  Ctrl+Q queue"),
                Line::from("  F1 toggles help"),
                Line::from("  Esc or q closes the active overlay"),
            ])
            .block(Block::default().title("Help").borders(Borders::ALL))
            .wrap(Wrap { trim: true }),
            overlay_area,
        );
    }

    fn format_queue_row(&self, item: &QueueItem, selected: bool) -> Line<'static> {
        let marker = if self.is_current_item(&item.id) {
            "▶"
        } else {
            match item.status {
                QueueItemStatus::Queued => "•",
                QueueItemStatus::Loading => "…",
                QueueItemStatus::Playing => "▶",
                QueueItemStatus::Failed => "!",
            }
        };
        let mut style = match item.status {
            QueueItemStatus::Queued => Style::default().fg(Color::White),
            QueueItemStatus::Loading => Style::default().fg(Color::Yellow),
            QueueItemStatus::Playing => Style::default().fg(Color::LightGreen),
            QueueItemStatus::Failed => Style::default().fg(Color::LightRed),
        };
        if selected {
            style = style.add_modifier(Modifier::BOLD);
        }

        Line::from(vec![
            Span::styled(format!("{} ", marker), style),
            Span::styled(item.song.title.clone(), style),
            Span::raw("  "),
            Span::styled(
                format!(
                    "{} • {}",
                    item.song.channel_name,
                    format_duration(item.song.duration_ms)
                ),
                Style::default().fg(Color::Gray),
            ),
        ])
    }

    fn set_status(&mut self, level: StatusLevel, message: String) {
        self.status_level = level;
        self.status_message = message;
    }

    fn adjust_volume(&mut self, delta: i16) -> AppAction {
        let next = (i16::from(self.audio.volume_percent) + delta).clamp(0, 100) as u8;
        if next == self.audio.volume_percent {
            return AppAction::None;
        }

        self.audio = AudioSettings::new(next);
        AppAction::Command(CommandAction::SetVolume(next))
    }
}

impl Default for InputState {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
        }
    }
}

impl InputState {
    fn clear(&mut self) {
        self.text.clear();
        self.cursor = 0;
    }

    fn insert(&mut self, ch: char) {
        let byte_index = self.cursor_byte_index();
        self.text.insert(byte_index, ch);
        self.cursor += 1;
    }

    fn delete_before_cursor(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let byte_index = self.cursor_byte_index();
        self.text.remove(byte_index);
    }

    fn delete_at_cursor(&mut self) {
        if self.cursor >= self.len_chars() {
            return;
        }
        let byte_index = self.cursor_byte_index();
        self.text.remove(byte_index);
    }

    fn move_left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn move_right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.len_chars());
    }

    fn move_home(&mut self) {
        self.cursor = 0;
    }

    fn move_end(&mut self) {
        self.cursor = self.len_chars();
    }

    fn split_for_render(&self) -> (String, String) {
        let byte_index = self.cursor_byte_index();
        (
            self.text[..byte_index].to_owned(),
            self.text[byte_index..].to_owned(),
        )
    }

    fn len_chars(&self) -> usize {
        self.text.chars().count()
    }

    fn cursor_byte_index(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor)
            .map(|(index, _)| index)
            .unwrap_or(self.text.len())
    }
}

impl SearchOverlay {
    fn move_selection(&mut self, delta: isize) {
        if self.results.is_empty() {
            self.selected = 0;
            return;
        }

        let max_index = self.results.len() as isize - 1;
        self.selected = (self.selected as isize + delta).clamp(0, max_index) as usize;
    }

    fn selected_song(&self) -> Option<&Song> {
        self.results.get(self.selected)
    }
}

fn centered_rect(horizontal_percent: u16, vertical_percent: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - vertical_percent) / 2),
            Constraint::Percentage(vertical_percent),
            Constraint::Percentage((100 - vertical_percent) / 2),
        ])
        .split(area);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - horizontal_percent) / 2),
            Constraint::Percentage(horizontal_percent),
            Constraint::Percentage((100 - horizontal_percent) / 2),
        ])
        .flex(Flex::Center)
        .split(vertical[1])[1]
}

fn format_duration(duration_ms: u64) -> String {
    let total_seconds = duration_ms / 1_000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{minutes}:{seconds:02}")
}

fn progress_bar_spans(position_ms: u64, duration_ms: u64, width: usize) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }

    if width == 1 {
        return vec![Span::raw("[")];
    }

    if width == 2 {
        return vec![Span::raw("[]")];
    }

    let safe_duration = duration_ms.max(1);
    let inner_width = width.saturating_sub(2);
    let filled = ((position_ms.min(safe_duration) as f64 / safe_duration as f64)
        * inner_width as f64)
        .round() as usize;
    let filled = filled.min(inner_width);
    vec![
        Span::raw("["),
        Span::styled("━".repeat(filled), Style::default().fg(Color::White)),
        Span::styled(
            "─".repeat(inner_width.saturating_sub(filled)),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw("]"),
    ]
}

fn text_width(text: &str) -> u16 {
    text.chars().count().min(usize::from(u16::MAX)) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use nocturne_api::PlaybackProgress;

    fn song(id: &str) -> Song {
        Song {
            id: id.to_owned(),
            title: format!("Song {id}"),
            channel_name: String::from("Channel"),
            duration_ms: 180_000,
            source_url: format!("https://example.com/{id}"),
        }
    }

    fn queue_item(id: &str, status: QueueItemStatus) -> QueueItem {
        QueueItem {
            id: id.to_owned(),
            song: song(id),
            added_at: String::from("2026-04-24T00:00:00Z"),
            status,
        }
    }

    #[test]
    fn queue_overlay_selects_next_track_first() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());
        app.queue = vec![
            queue_item("queue_item_1", QueueItemStatus::Playing),
            queue_item("queue_item_2", QueueItemStatus::Queued),
            queue_item("queue_item_3", QueueItemStatus::Queued),
        ];
        app.playback.current_queue_item_id = Some(String::from("queue_item_1"));

        app.open_queue_overlay();

        assert_eq!(app.queue_selection, 1);
    }

    #[test]
    fn queue_selection_is_clamped_after_updates() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());
        app.queue = vec![
            queue_item("queue_item_1", QueueItemStatus::Queued),
            queue_item("queue_item_2", QueueItemStatus::Queued),
        ];
        app.queue_selection = 4;

        app.reconcile_queue_selection();

        assert_eq!(app.queue_selection, 1);
    }

    #[test]
    fn search_submit_opens_loading_overlay() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());
        app.input.text = String::from("utada traveling");
        app.input.move_end();

        let action = app.handle_key(KeyEvent::from(KeyCode::Enter));

        assert_eq!(
            action,
            AppAction::Command(CommandAction::Search(String::from("utada traveling")))
        );

        app.apply_command_result(Ok(CommandOutcome::SearchSubmitted {
            job_id: String::from("job_1"),
            query: String::from("utada traveling"),
        }));

        assert_eq!(app.overlay, Overlay::Search);
        let search = app.search_overlay.as_ref().unwrap();
        assert_eq!(search.state, SearchOverlayState::Loading);
        assert_eq!(search.query, "utada traveling");
    }

    #[test]
    fn completed_search_shows_results_and_enter_queues_selected_song() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());
        app.open_search_overlay(String::from("job_1"), String::from("utada traveling"));

        app.apply_search_job_completed(String::from("job_1"), vec![song("song_1"), song("song_2")]);

        assert_eq!(app.overlay, Overlay::Search);
        let search = app.search_overlay.as_ref().unwrap();
        assert_eq!(search.state, SearchOverlayState::Results);
        assert_eq!(search.results.len(), 2);

        let action = app.handle_key(KeyEvent::from(KeyCode::Enter));
        assert_eq!(
            action,
            AppAction::Command(CommandAction::AddSong(String::from("song_1")))
        );
    }

    #[test]
    fn completed_search_supports_jk_and_arrow_navigation() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());
        app.open_search_overlay(String::from("job_1"), String::from("utada traveling"));

        app.apply_search_job_completed(
            String::from("job_1"),
            vec![song("song_1"), song("song_2"), song("song_3")],
        );

        let _ = app.handle_key(KeyEvent::from(KeyCode::Down));
        assert_eq!(app.search_overlay.as_ref().unwrap().selected, 1);

        let _ = app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(app.search_overlay.as_ref().unwrap().selected, 2);

        let _ = app.handle_key(KeyEvent::from(KeyCode::Char('j')));
        assert_eq!(app.search_overlay.as_ref().unwrap().selected, 2);

        let _ = app.handle_key(KeyEvent::from(KeyCode::Up));
        assert_eq!(app.search_overlay.as_ref().unwrap().selected, 1);

        let _ = app.handle_key(KeyEvent::from(KeyCode::Char('k')));
        assert_eq!(app.search_overlay.as_ref().unwrap().selected, 0);

        let _ = app.handle_key(KeyEvent::from(KeyCode::Char('k')));
        assert_eq!(app.search_overlay.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn completed_search_caps_visible_results_to_five_items() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());
        app.open_search_overlay(String::from("job_1"), String::from("utada traveling"));

        app.apply_search_job_completed(
            String::from("job_1"),
            vec![
                song("song_1"),
                song("song_2"),
                song("song_3"),
                song("song_4"),
                song("song_5"),
                song("song_6"),
            ],
        );

        let search = app.search_overlay.as_ref().unwrap();
        assert_eq!(search.results.len(), MAX_SEARCH_RESULTS);
        assert_eq!(
            search.results.last().map(|song| song.id.as_str()),
            Some("song_5")
        );
    }

    #[test]
    fn completed_search_arriving_before_command_ack_is_applied_after_overlay_opens() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());

        app.apply_search_job_completed(String::from("job_1"), vec![song("song_1")]);
        app.apply_command_result(Ok(CommandOutcome::SearchSubmitted {
            job_id: String::from("job_1"),
            query: String::from("utada traveling"),
        }));

        let search = app.search_overlay.as_ref().unwrap();
        assert_eq!(search.state, SearchOverlayState::Results);
        assert_eq!(search.results.len(), 1);
        assert_eq!(app.status_message, "Found 1 candidate(s).");
    }

    #[test]
    fn failed_search_arriving_before_command_ack_is_applied_after_overlay_opens() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());

        app.apply_search_job_failed(String::from("job_1"), String::from("yt-dlp failed"));
        app.apply_command_result(Ok(CommandOutcome::SearchSubmitted {
            job_id: String::from("job_1"),
            query: String::from("utada traveling"),
        }));

        let search = app.search_overlay.as_ref().unwrap();
        assert_eq!(search.state, SearchOverlayState::Error);
        assert_eq!(search.error_message.as_deref(), Some("yt-dlp failed"));
    }

    #[test]
    fn empty_search_closes_on_enter_esc_and_q_and_clears_query() {
        for key in [
            KeyEvent::from(KeyCode::Enter),
            KeyEvent::from(KeyCode::Esc),
            KeyEvent::from(KeyCode::Char('q')),
        ] {
            let mut app = App::new("127.0.0.1:4100".parse().unwrap());
            app.input.text = String::from("utada traveling");
            app.input.move_end();
            app.open_search_overlay(String::from("job_1"), String::from("utada traveling"));
            app.apply_search_job_completed(String::from("job_1"), Vec::new());

            let action = app.handle_key(key);

            assert_eq!(action, AppAction::None);
            assert_eq!(app.overlay, Overlay::None);
            assert!(app.input.text.is_empty());
            let search = app.search_overlay.as_ref().unwrap();
            assert_eq!(search.state, SearchOverlayState::Empty);
            assert!(search.closed);
        }
    }

    #[test]
    fn error_search_closes_on_enter_esc_and_q_and_clears_query() {
        for key in [
            KeyEvent::from(KeyCode::Enter),
            KeyEvent::from(KeyCode::Esc),
            KeyEvent::from(KeyCode::Char('q')),
        ] {
            let mut app = App::new("127.0.0.1:4100".parse().unwrap());
            app.input.text = String::from("utada traveling");
            app.input.move_end();
            app.open_search_overlay(String::from("job_1"), String::from("utada traveling"));
            app.apply_search_job_failed(String::from("job_1"), String::from("yt-dlp failed"));

            let action = app.handle_key(key);

            assert_eq!(action, AppAction::None);
            assert_eq!(app.overlay, Overlay::None);
            assert!(app.input.text.is_empty());
            let search = app.search_overlay.as_ref().unwrap();
            assert_eq!(search.state, SearchOverlayState::Error);
            assert_eq!(search.error_message.as_deref(), Some("yt-dlp failed"));
            assert!(search.closed);
        }
    }

    #[test]
    fn snapshot_marks_active_overlay_failed_when_job_failed_during_reconnect() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());
        app.open_search_overlay(String::from("job_1"), String::from("utada traveling"));

        app.apply_snapshot(
            StateSnapshot {
                backend: BackendStatus {
                    ready: true,
                    version: Some(String::from("test")),
                },
                playback: PlaybackState {
                    state: PlaybackStatus::Stopped,
                    position_ms: 0,
                    current_queue_item_id: None,
                    playback_session_id: None,
                },
                audio: AudioSettings::default(),
                current_song: None,
                queue: Vec::new(),
                search_jobs: vec![SearchJobSummary {
                    job_id: String::from("job_1"),
                    status: SearchJobStatus::Failed,
                    query: String::from("utada traveling"),
                    created_at: String::from("2026-04-24T00:00:00Z"),
                    completed_at: Some(String::from("2026-04-24T00:00:05Z")),
                    result_count: None,
                }],
                revision: 1,
                snapshot_id: String::from("snap_1"),
                timestamp: String::from("2026-04-24T00:00:06Z"),
            },
            Some(String::from("evt_1")),
        );

        let search = app.search_overlay.as_ref().unwrap();
        assert_eq!(search.state, SearchOverlayState::Error);
        assert_eq!(
            search.error_message.as_deref(),
            Some("Search failed. Reconnect completed without a detailed error.")
        );
    }

    #[test]
    fn snapshot_requests_result_recovery_when_job_completed_during_reconnect() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());
        app.open_search_overlay(String::from("job_1"), String::from("utada traveling"));

        app.apply_snapshot(
            StateSnapshot {
                backend: BackendStatus {
                    ready: true,
                    version: Some(String::from("test")),
                },
                playback: PlaybackState {
                    state: PlaybackStatus::Stopped,
                    position_ms: 0,
                    current_queue_item_id: None,
                    playback_session_id: None,
                },
                audio: AudioSettings::default(),
                current_song: None,
                queue: Vec::new(),
                search_jobs: vec![SearchJobSummary {
                    job_id: String::from("job_1"),
                    status: SearchJobStatus::Completed,
                    query: String::from("utada traveling"),
                    created_at: String::from("2026-04-24T00:00:00Z"),
                    completed_at: Some(String::from("2026-04-24T00:00:05Z")),
                    result_count: Some(2),
                }],
                revision: 1,
                snapshot_id: String::from("snap_1"),
                timestamp: String::from("2026-04-24T00:00:06Z"),
            },
            Some(String::from("evt_1")),
        );

        assert_eq!(app.take_search_results_request().as_deref(), Some("job_1"));
        assert_eq!(app.status_message, "Refreshing search results...");
    }

    #[test]
    fn state_updated_syncs_current_song_from_queue() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());

        app.apply_backend_event(BackendEvent {
            event_id: String::from("evt_1"),
            kind: BackendEventKind::StateUpdated(StateSnapshot {
                backend: BackendStatus {
                    ready: true,
                    version: Some(String::from("test")),
                },
                playback: PlaybackState {
                    state: PlaybackStatus::Playing,
                    position_ms: 0,
                    current_queue_item_id: Some(String::from("queue_item_1")),
                    playback_session_id: Some(String::from("session_1")),
                },
                audio: AudioSettings::default(),
                current_song: None,
                queue: vec![queue_item("queue_item_1", QueueItemStatus::Playing)],
                search_jobs: Vec::new(),
                revision: 1,
                snapshot_id: String::from("snap_1"),
                timestamp: String::from("2026-04-24T00:00:06Z"),
            }),
        });

        assert_eq!(
            app.current_song.as_ref().map(|song| song.id.as_str()),
            Some("queue_item_1")
        );
        assert!(!app.playback_progress_confirmed);
    }

    #[test]
    fn playback_progress_adopts_missing_session_id() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());
        app.playback.state = PlaybackStatus::Playing;

        app.apply_backend_event(BackendEvent {
            event_id: String::from("evt_1"),
            kind: BackendEventKind::PlaybackProgress(PlaybackProgress {
                playback_session_id: String::from("session_1"),
                position_ms: 1_234,
            }),
        });

        assert_eq!(app.playback.playback_session_id.as_deref(), Some("session_1"));
        assert_eq!(app.playback.position_ms, 1_234);
        assert!(app.playback_progress_confirmed);
    }

    #[test]
    fn playback_progress_ignores_mismatched_session_id() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());
        app.playback.playback_session_id = Some(String::from("session_1"));
        app.playback.position_ms = 500;

        app.apply_backend_event(BackendEvent {
            event_id: String::from("evt_1"),
            kind: BackendEventKind::PlaybackProgress(PlaybackProgress {
                playback_session_id: String::from("session_2"),
                position_ms: 1_234,
            }),
        });

        assert_eq!(app.playback.playback_session_id.as_deref(), Some("session_1"));
        assert_eq!(app.playback.position_ms, 500);
        assert!(!app.playback_progress_confirmed);
    }

    #[test]
    fn progress_bar_spans_render_bracketed_bar() {
        let spans = progress_bar_spans(50, 100, 6);

        assert_eq!(spans[0].content.as_ref(), "[");
        assert_eq!(spans[1].content.as_ref(), "━━");
        assert_eq!(spans[2].content.as_ref(), "──");
        assert_eq!(spans[3].content.as_ref(), "]");
    }

    #[test]
    fn progress_bar_spans_handle_narrow_widths() {
        assert_eq!(progress_bar_spans(50, 100, 0).len(), 0);
        assert_eq!(progress_bar_spans(50, 100, 1)[0].content.as_ref(), "[");
        assert_eq!(progress_bar_spans(50, 100, 2)[0].content.as_ref(), "[]");
    }

    #[test]
    fn text_width_counts_display_columns_for_ascii_labels() {
        assert_eq!(text_width("Vol 68%"), 7);
    }

    #[test]
    fn closed_search_does_not_reopen_when_results_arrive_late() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());
        app.open_search_overlay(String::from("job_1"), String::from("late result"));

        let _ = app.handle_key(KeyEvent::from(KeyCode::Esc));
        app.apply_search_job_completed(String::from("job_1"), vec![song("song_1")]);

        assert_eq!(app.overlay, Overlay::None);
        assert!(app.search_overlay.as_ref().unwrap().closed);
    }

    #[test]
    fn adding_song_clears_query_and_closes_search_overlay() {
        let mut app = App::new("127.0.0.1:4100".parse().unwrap());
        app.input.text = String::from("utada traveling");
        app.input.move_end();
        app.open_search_overlay(String::from("job_1"), String::from("utada traveling"));

        app.apply_command_result(Ok(CommandOutcome::SongQueued(String::from(
            "Added selection to queue.",
        ))));

        assert_eq!(app.overlay, Overlay::None);
        assert!(app.search_overlay.is_none());
        assert!(app.input.text.is_empty());
    }
}
