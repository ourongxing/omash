mod api;
mod app;
mod backup;
mod config;
mod core;
mod enhance;
mod omarchy;
mod profiles;
mod statusbar;
mod theme;
mod ui;

use anyhow::Result;
use app::App;
use clap::Parser;
use config::{Cli, Command, Config};
use crossterm::{
    event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::io::{self, stdout};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let config = Config::load(&cli)?;
    if let Some(Command::Bar(args)) = &cli.command {
        return statusbar::run(&config, &args.command).await;
    }
    core::ensure_system_core()?;
    if cli.daemon {
        return core::run_supervisor(config).await;
    }
    core::ensure_supervisor(config.auto_start).await?;
    let mut app = App::new(config)?;
    let mut terminal = setup_terminal()?;
    let result = app.run(&mut terminal).await;
    restore_terminal(&mut terminal)?;
    result
}

fn setup_terminal() -> Result<Terminal<CrosstermBackend<io::Stdout>>> {
    enable_raw_mode()?;
    execute!(
        stdout(),
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    Ok(Terminal::new(CrosstermBackend::new(stdout()))?)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> Result<()> {
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    Ok(())
}
