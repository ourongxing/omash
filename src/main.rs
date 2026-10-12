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
mod tun;
mod ui;

use anyhow::Result;
use app::{App, restore_terminal, setup_terminal};
use clap::Parser;
use config::{Cli, Command, Config};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    if cli.config.is_some() && matches!(cli.command, Some(Command::Tun { .. })) {
        anyhow::bail!(
            "managed TUN uses the supervisor's default configuration; use XDG_CONFIG_HOME consistently instead of --config"
        );
    }
    // Privileged/internal entry points must never load or create root's desktop configuration.
    match &cli.command {
        Some(Command::InternalTunService) => return tun::service::run().await,
        Some(Command::InternalTunInstall {
            uid,
            gid,
            data_dir,
            digest,
        }) => return tun::install::root_install(*uid, *gid, data_dir, digest),
        Some(Command::InternalTunUninstall { uid }) => return tun::install::root_uninstall(*uid),
        Some(Command::Tun {
            command: config::TunCommand::Setup,
        }) => return tun::install::setup().await,
        _ => {}
    }
    let config = Config::load(&cli)?;
    match &cli.command {
        Some(Command::Bar(args)) => return statusbar::run(&config, &args.command).await,
        Some(Command::Stop) => return core::cli_stop().await,
        Some(Command::Start) => return core::cli_start().await,
        Some(Command::Restart) => return core::cli_restart().await,
        Some(Command::Tun { command }) => return tun::cli::run(config, command).await,
        _ => {}
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
