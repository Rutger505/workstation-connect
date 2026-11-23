mod actions;
mod btsock;
mod config;
mod ctl;
mod daemon;
mod kde;
mod logic;
mod logs;
mod net;
mod phone;
mod proto;
mod sdp;
mod sigv4;
mod update;

use anyhow::Result;
use clap::{Parser, Subcommand};

/// Laptop side of workstation-connect: phone internet on demand over Bluetooth.
#[derive(Parser)]
#[command(version = update::VERSION)]
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
    /// Action menu (walker/fuzzel/wofi/rofi).
    Menu,
    /// Send the laptop clipboard to the phone.
    Clip,
    /// Send files to the phone (file picker without arguments).
    Send { files: Vec<std::path::PathBuf> },
    /// Mount the phone's storage (sftp) and open it.
    Browse,
    /// Phone screen in a window (scrcpy over wireless debugging).
    Screen,
    /// One-time kdeconnectd setup: notifications to phone, Bluetooth link, share folder.
    Setup,
    /// Store the write-only key for log uploads (~/.config/workstation-connect/logs.env).
    LogsKey { access: String, secret: String },
    /// Reinstall wsconnect from GitHub main (cargo install), restart the daemon.
    Update {
        /// Wait for Enter at the end (menu runs it in a fresh terminal).
        #[arg(long, hide = true)]
        pause: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Command::Daemon => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "wsconnect=info".into()),
                )
                .without_time() // journald adds timestamps
                .init();
            daemon::run(config::Config::load()?).await
        }
        Command::Waybar => ctl::waybar().await,
        Command::Menu => actions::menu().await,
        Command::Clip => actions::run(actions::Action::Clipboard, vec![]).await,
        Command::Send { files } => actions::run(actions::Action::SendFiles, files).await,
        Command::Browse => actions::run(actions::Action::Browse, vec![]).await,
        Command::Screen => actions::run(actions::Action::Screen, vec![]).await,
        Command::Setup => actions::setup().await,
        Command::Update { pause } => update::run(pause).await,
        Command::LogsKey { access, secret } => logs::write_key(&access, &secret),
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
