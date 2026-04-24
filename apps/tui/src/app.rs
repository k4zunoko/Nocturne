use std::net::SocketAddr;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use nocturne_api::{BackendStatus, StateSnapshot};
use nocturne_domain::{PlaybackState, PlaybackStatus, QueueItem, QueueItemStatus, Song};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::backend::{BackendEvent, BackendEventKind, CommandAction, SnapshotSeed};

#[derive(Debug)]
pub enum AppEvent {
    Input(KeyEvent),
    Backend(BackendEvent),
    Snapshot(SnapshotSeed),
    CommandResult(Result<String, String>),
    ConnectionStatus(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Overlay {
    None,
    Queue,
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
    current_song: Option<Song>,
    queue: Vec<QueueItem>,
    overlay: Overlay,
    queue_selection: usize,
    pending_delete_confirmation: bool,
    status_message: String,
    status_level: StatusLevel,
    last_event_id: Option<String>,
    display_position_ms: u64,
    playback_anchor_ms: u64,
    playback_anchor_at: Instant,
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
            },
            current_song: None,
            queue: Vec::new(),
            overlay: Overlay::None,
            queue_selection: 0,
            pending_delete_confirmation: false,
            status_message: String::from("Starting backend..."),
            status_level: StatusLevel::Info,
            last_event_id: None,
            display_position_ms: 0,
            playback_anchor_ms: 0,
            playback_anchor_at: Instant::now(),
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: StateSnapshot, current_event_id: Option<String>) {
        self.backend = snapshot.backend;
        self.playback = snapshot.playback;
        self.current_song = snapshot.current_song;
        self.queue = snapshot.queue;
        self.last_event_id = current_event_id;
        self.sync_playback_anchor(self.playback.position_ms);
        self.sync_current_song_from_queue();
        self.reconcile_queue_selection();
        self.set_status(
            StatusLevel::Info,
            format!("Connected to backend on http://{}", self.backend_addr),
        );
    }

    pub fn apply_backend_event(&mut self, event: BackendEvent) {
        self.last_event_id = Some(event.event_id);

        match event.kind {
            BackendEventKind::PlaybackStateChanged(payload) => {
                self.playback.state = payload.state;
                self.playback.current_queue_item_id = payload.current_queue_item_id;
                self.playback.position_ms = payload.position_ms;
                self.sync_playback_anchor(payload.position_ms);
                self.sync_current_song_from_queue();
            }
            BackendEventKind::PlaybackTrackChanged(payload) => {
                self.playback.current_queue_item_id = Some(payload.queue_item_id);
                self.current_song = Some(payload.song);
                self.sync_playback_anchor(0);
            }
            BackendEventKind::PlaybackPositionUpdated(payload) => {
                self.playback.position_ms = payload.position_ms;
                self.sync_playback_anchor(payload.position_ms);
            }
            BackendEventKind::QueueUpdated(payload) => {
                self.queue = payload.items;
                self.sync_current_song_from_queue();
                self.reconcile_queue_selection();
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

    pub fn apply_command_result(&mut self, result: Result<String, String>) {
        match result {
            Ok(message) => self.set_status(StatusLevel::Info, message),
            Err(message) => self.set_status(StatusLevel::Error, message),
        }
    }

    pub fn note_connection_status(&mut self, message: String) {
        self.set_status(StatusLevel::Warning, message);
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
                KeyCode::Left => return AppAction::Command(CommandAction::Previous),
                KeyCode::Right => return AppAction::Command(CommandAction::Next),
                _ => {}
            }
        }

        match self.overlay {
            Overlay::None => AppAction::None,
            Overlay::Queue => self.handle_queue_overlay_key(key),
        }
    }

    pub fn render(&self, frame: &mut Frame<'_>) {
        let area = frame.area();
        let layout = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(6),
                Constraint::Min(6),
                Constraint::Length(3),
                Constraint::Length(2),
            ])
            .split(area);

        frame.render_widget(self.now_playing_widget(), layout[0]);
        frame.render_widget(self.queue_summary_widget(), layout[1]);
        frame.render_widget(self.status_widget(), layout[2]);
        frame.render_widget(self.shortcuts_widget(), layout[3]);

        if self.overlay == Overlay::Queue {
            self.render_queue_overlay(frame, area);
        }
    }

    pub fn last_event_id(&self) -> Option<String> {
        self.last_event_id.clone()
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

    fn open_queue_overlay(&mut self) {
        self.overlay = Overlay::Queue;
        self.pending_delete_confirmation = false;
        self.queue_selection = self.initial_queue_selection();
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

    fn sync_playback_anchor(&mut self, position_ms: u64) {
        self.playback_anchor_ms = position_ms;
        self.display_position_ms = position_ms;
        self.playback_anchor_at = Instant::now();
    }

    fn current_display_position_ms(&self) -> u64 {
        if self.playback.state != PlaybackStatus::Playing {
            return self.playback_anchor_ms;
        }

        let elapsed_ms = self.playback_anchor_at.elapsed().as_millis() as u64;
        let position_ms = self.playback_anchor_ms.saturating_add(elapsed_ms);
        let duration_ms = self.current_song.as_ref().map_or(u64::MAX, |song| song.duration_ms);
        position_ms.min(duration_ms)
    }

    fn now_playing_widget(&self) -> Paragraph<'static> {
        let title = self
            .current_song
            .as_ref()
            .map_or_else(|| String::from("Nothing queued yet"), |song| song.title.clone());
        let channel = self.current_song.as_ref().map_or_else(
            || String::from("Add a song from search once that flow is wired."),
            |song| song.channel_name.clone(),
        );
        let state_label = match self.playback.state {
            PlaybackStatus::Stopped => "Stopped",
            PlaybackStatus::Playing => "Playing",
            PlaybackStatus::Paused => "Paused",
        };
        let duration_ms = self.current_song.as_ref().map_or(1, |song| song.duration_ms.max(1));

        let lines = vec![
            Line::from(vec![
                Span::styled("Now Playing ", Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(title),
            ]),
            Line::from(channel),
            Line::from(format!(
                "{}  {} / {}",
                state_label,
                format_duration(self.display_position_ms),
                format_duration(duration_ms)
            )),
            Line::from(progress_bar(self.display_position_ms, duration_ms, 32)),
            Line::from(String::new()),
        ];

        Paragraph::new(lines)
            .block(Block::default().title("Playback").borders(Borders::ALL))
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
            .block(Block::default().title("Queue Snapshot").borders(Borders::ALL))
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
        let readiness = if self.backend.ready { "ready" } else { "starting" };
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
            "Ctrl+P play/pause  Ctrl+Left previous  Ctrl+Right next  Ctrl+Q queue  Ctrl+C quit",
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
                .map(|(index, item)| ListItem::new(self.format_queue_row(item, index == self.queue_selection)))
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

    fn format_queue_row(&self, item: &QueueItem, selected: bool) -> Line<'static> {
        let marker = if self.is_current_item(&item.id) {
            "▶"
        } else {
            match item.status {
                QueueItemStatus::Queued => "•",
                QueueItemStatus::Loading => "…",
                QueueItemStatus::Playing => "▶",
                QueueItemStatus::Finished => "✓",
                QueueItemStatus::Failed => "!",
            }
        };
        let mut style = match item.status {
            QueueItemStatus::Queued => Style::default().fg(Color::White),
            QueueItemStatus::Loading => Style::default().fg(Color::Yellow),
            QueueItemStatus::Playing => Style::default().fg(Color::LightGreen),
            QueueItemStatus::Finished => Style::default().fg(Color::DarkGray),
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
                format!("{} • {}", item.song.channel_name, format_duration(item.song.duration_ms)),
                Style::default().fg(Color::Gray),
            ),
        ])
    }

    fn set_status(&mut self, level: StatusLevel, message: String) {
        self.status_level = level;
        self.status_message = message;
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

fn progress_bar(position_ms: u64, duration_ms: u64, width: usize) -> String {
    let safe_duration = duration_ms.max(1);
    let filled = ((position_ms.min(safe_duration) as f64 / safe_duration as f64) * width as f64)
        .round() as usize;
    let filled = filled.min(width);
    format!(
        "[{}{}]",
        "█".repeat(filled),
        "·".repeat(width.saturating_sub(filled))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
