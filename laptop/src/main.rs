mod config;
mod ctl;
mod daemon;
mod logic;
mod net;
mod phone;
mod proto;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Laptop side of workstation-connect: phone internet on demand over BLE.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon (systemd user service).
    Daemon,
    /// Print the daemon status as JSON.
    Status,
    /// Pause automation (turns the phone off if in use).
    Pause,
    /// Resume automation.
    Resume,
    /// Toggle pause (waybar on-click).
    Toggle,
    /// Waybar custom module (exec, return-type json).
    Waybar,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Command::Daemon => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "wsconnect=info".into()),
                )
                .without_time() // journald adds timestamps
                .init();
            daemon::run(config::Config::load()?).await
        }
        Command::Waybar => ctl::waybar().await,
        cmd => {
            let req = match cmd {
                Command::Pause => "pause",
                Command::Resume => "resume",
                Command::Toggle => "toggle",
                _ => "status",
            };
            println!("{}", ctl::request(req).await?);
            Ok(())
        }
    }
}
