mod app;
mod backend;

use std::io;
use std::time::Duration;

use app::{App, AppAction, AppEvent};
use backend::{BackendClient, ManagedBackend, spawn_command_task, spawn_sse_task};
use crossterm::event::{Event, EventStream};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut backend = ManagedBackend::spawn()?;
    backend.wait_until_ready().await?;

    let client = BackendClient::new(backend.addr());
    let seed = client.fetch_snapshot().await?;

    let mut app = App::new(backend.addr());
    app.apply_snapshot(seed.snapshot, seed.current_event_id.clone());

    let (tx, mut rx) = mpsc::unbounded_channel();
    spawn_input_task(tx.clone());
    spawn_sse_task(client.clone(), app.last_event_id(), tx.clone());

    let mut terminal = setup_terminal()?;
    let result = run_app(&mut terminal, &mut app, &client, &tx, &mut rx).await;

    restore_terminal(&mut terminal)?;
    backend.shutdown();
    result.map_err(Into::into)
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    client: &BackendClient,
    tx: &mpsc::UnboundedSender<AppEvent>,
    rx: &mut mpsc::UnboundedReceiver<AppEvent>,
) -> Result<(), backend::TuiError> {
    let mut render_interval = tokio::time::interval(Duration::from_millis(33));
    let mut tick_interval = tokio::time::interval(Duration::from_millis(250));

    loop {
        tokio::select! {
            _ = render_interval.tick() => {
                terminal
                    .draw(|frame| app.render(frame))
                    .map_err(|error| backend::TuiError::new(format!("failed to draw TUI: {error}")))?;
            }
            _ = tick_interval.tick() => {
                app.tick();
            }
            maybe_event = rx.recv() => {
                let Some(event) = maybe_event else {
                    break;
                };

                match event {
                    AppEvent::Input(key) => {
                        match app.handle_key(key) {
                            AppAction::None => {}
                            AppAction::Quit => break,
                            AppAction::Command(action) => {
                                spawn_command_task(client.clone(), action, tx.clone());
                            }
                        }
                    }
                    AppEvent::Backend(event) => app.apply_backend_event(event),
                    AppEvent::Snapshot(seed) => app.apply_snapshot(seed.snapshot, seed.current_event_id),
                    AppEvent::CommandResult(result) => app.apply_command_result(result),
                    AppEvent::ConnectionStatus(message) => app.note_connection_status(message),
                }
            }
        }
    }

    Ok(())
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>, io::Error> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<(), io::Error> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()
}

fn spawn_input_task(tx: mpsc::UnboundedSender<AppEvent>) {
    tokio::spawn(async move {
        let mut reader = EventStream::new();
        while let Some(event) = reader.next().await {
            match event {
                Ok(Event::Key(key)) if key.kind == crossterm::event::KeyEventKind::Press => {
                    if tx.send(AppEvent::Input(key)).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = tx.send(AppEvent::ConnectionStatus(format!(
                        "Input stream error: {error}"
                    )));
                    break;
                }
            }
        }
    });
}
