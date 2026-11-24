mod actions;
mod btsock;
mod config;
mod ctl;
mod daemon;
mod kde;
mod logging;
mod logic;
mod logs;
mod migrate;
mod net;
mod phone;
mod proto;
mod sdp;
mod selftest;
mod sigv4;
mod update;

use anyhow::Result;
use clap::{CommandFactory, Parser, Subcommand};
use tracing::{debug, error, warn};

/// Laptop side of workstation-connect: phone internet on demand over Bluetooth.
#[derive(Parser)]
#[command(version = update::VERSION)]
struct Cli {
    /// Print the full build commit (updater's stale-build guard).
    #[arg(long, hide = true, exclusive = true)]
    build_commit: bool,
    #[command(subcommand)]
    cmd: Option<Command>,
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
    /// Check daemon, phone link, hotspot and KDE Connect step by step; upload the report + logs
    /// and hand it to Rocky (someclaw-thread).
    Selftest {
        /// Skip the hotspot test (turns the phone's hotspot on and off).
        #[arg(long)]
        no_hotspot: bool,
        /// Don't hand the report to Rocky.
        #[arg(long)]
        no_send: bool,
    },
    /// Store the write-only key for log uploads (~/.config/workstation-connect/logs.env).
    LogsKey { access: String, secret: String },
    /// Reinstall workstation-connect from GitHub main (cargo install), restart the daemon.
    Update {
        /// Wait for Enter at the end (menu runs it in a fresh terminal).
        #[arg(long, hide = true)]
        pause: bool,
    },
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Command::Daemon => "daemon",
            Command::Status => "status",
            Command::Pause => "pause",
            Command::Resume => "resume",
            Command::Toggle => "toggle",
            Command::Waybar => "waybar",
            Command::Menu => "menu",
            Command::Clip => "clip",
            Command::Send { .. } => "send",
            Command::Browse => "browse",
            Command::Screen => "screen",
            Command::Setup => "setup",
            Command::Selftest { .. } => "selftest",
            Command::LogsKey { .. } => "logs-key",
            Command::Update { .. } => "update",
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // TRANSITIONAL: see update::called_by_updater.
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if matches!(&args[..], [a] if a == "--version" || a == "-V") && update::called_by_updater() {
        println!("workstation-connect {}", update::VERSION_LONG);
        return Ok(());
    }
    let cli = Cli::parse();
    if cli.build_commit {
        println!("{}", update::COMMIT);
        return Ok(());
    }
    let Some(cmd) = cli.cmd else {
        Cli::command()
            .error(
                clap::error::ErrorKind::MissingSubcommand,
                "a subcommand is required",
            )
            .exit()
    };
    if let Command::Daemon = cmd {
        // Config first: it may set the log level. Its error is logged right after.
        let cfg = config::Config::load();
        let filter = logging::init(
            logging::Kind::Daemon,
            cfg.as_ref().ok().and_then(|c| c.log_level.as_deref()),
        );
        if let migrate::Outcome::HandedOff = migrate::run() {
            // Conflicts= stops this unit any moment now; keep running if it somehow doesn't.
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            warn!("migrate: still running in the old unit after the handoff");
        }
        let res = match cfg {
            Ok(cfg) => daemon::run(cfg, &filter).await,
            Err(e) => Err(e),
        };
        if let Err(e) = res {
            error!("daemon stopped: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    logging::init(logging::Kind::Cli, None);
    debug!(cmd = cmd.name(), version = update::VERSION, "cli");
    match cmd {
        Command::Daemon => unreachable!("handled above"),
        Command::Waybar => ctl::waybar().await,
        Command::Menu => actions::menu().await,
        Command::Clip => actions::run(actions::Action::Clipboard, vec![]).await,
        Command::Send { files } => actions::run(actions::Action::SendFiles, files).await,
        Command::Browse => actions::run(actions::Action::Browse, vec![]).await,
        Command::Screen => actions::run(actions::Action::Screen, vec![]).await,
        Command::Setup => actions::setup().await,
        Command::Update { pause } => update::run(pause).await,
        Command::Selftest {
            no_hotspot,
            no_send,
        } => selftest::run(no_hotspot, no_send).await,
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
