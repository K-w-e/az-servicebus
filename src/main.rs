mod app;
mod azure;
mod config;
mod purge;
mod ui;
mod worker;

use anyhow::Result;
use app::App;
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as TermEvent, EventStream, KeyEventKind,
    MouseEventKind,
};
use crossterm::execute;
use futures::StreamExt;
use ratatui::DefaultTerminal;
use std::time::Duration;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use worker::{Command, Event};

#[tokio::main]
async fn main() -> Result<()> {
    // Lettura config prima di entrare in modalità TUI.
    let config = config::load()?;

    let (tx_cmd, rx_cmd) = mpsc::unbounded_channel::<Command>();
    let (tx_evt, rx_evt) = mpsc::unbounded_channel::<Event>();
    tokio::spawn(worker::run(rx_cmd, tx_evt));

    // `ratatui::init` installa un panic hook che ripristina il terminale
    // (disable_raw_mode + LeaveAlternateScreen) prima di stampare il panic.
    let mut terminal = ratatui::init();
    // Anche la cattura del mouse va disattivata in caso di panic.
    let ratatui_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        ratatui_hook(info);
    }));
    execute!(std::io::stdout(), EnableMouseCapture)?;

    let result = run(&mut terminal, App::new(config), tx_cmd, rx_evt).await;

    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut DefaultTerminal,
    (mut app, init_cmds): (App, Vec<Command>),
    tx: UnboundedSender<Command>,
    mut rx: UnboundedReceiver<Event>,
) -> Result<()> {
    let send = |cmds: Vec<Command>| {
        for c in cmds {
            let _ = tx.send(c);
        }
    };
    send(init_cmds);

    let mut input = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut dirty = true;
    let mut mouse_on = app.mouse_enabled;

    while !app.should_quit {
        if mouse_on != app.mouse_enabled {
            mouse_on = app.mouse_enabled;
            if mouse_on {
                execute!(std::io::stdout(), EnableMouseCapture)?;
            } else {
                execute!(std::io::stdout(), DisableMouseCapture)?;
            }
        }
        if dirty {
            terminal.draw(|f| ui::draw(f, &mut app))?;
            dirty = false;
        }
        tokio::select! {
            maybe = input.next() => match maybe {
                Some(Ok(TermEvent::Key(k))) if k.kind == KeyEventKind::Press => {
                    send(app.on_key(k));
                    dirty = true;
                }
                Some(Ok(TermEvent::Mouse(m))) => {
                    // I movimenti del mouse non richiedono redraw.
                    if !matches!(m.kind, MouseEventKind::Moved | MouseEventKind::Drag(_)) {
                        send(app.on_mouse(m));
                        dirty = true;
                    }
                }
                Some(Ok(TermEvent::Resize(..))) => dirty = true,
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => break,
            },
            Some(ev) = rx.recv() => {
                send(app.on_backend(ev));
                // Drena gli eventi in coda per ridurre i redraw.
                while let Ok(ev) = rx.try_recv() {
                    send(app.on_backend(ev));
                }
                dirty = true;
            }
            _ = tick.tick() => {
                if app.is_busy() {
                    app.spinner = app.spinner.wrapping_add(1);
                    dirty = true;
                }
            }
        }
    }
    Ok(())
}
