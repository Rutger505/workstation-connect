//! tracing setup. Daemon: stdout → journal; CLI: stderr. Under systemd (JOURNAL_STREAM set) every
//! line gets a `<N>` syslog priority prefix, so `journalctl -p warning` works.
//! Filter: RUST_LOG, else config `log_level` (daemon only), else the default (daemon `info`, CLI
//! `warn`, CLI under systemd e.g. the update unit `info`).

use std::io::IsTerminal;
use tracing::{Level, error, warn};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::Writer;
use tracing_subscriber::fmt::writer::BoxMakeWriter;
use tracing_subscriber::fmt::{self, FmtContext, FormatEvent, FormatFields};
use tracing_subscriber::registry::LookupSpan;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Daemon,
    Cli,
}

const LEVELS: [&str; 6] = ["trace", "debug", "info", "warn", "error", "off"];

/// Bare level ("debug") → only our crate at that level; anything else is EnvFilter syntax as-is.
pub fn expand(level: &str) -> String {
    let l = level.trim();
    if LEVELS.contains(&l.to_ascii_lowercase().as_str()) {
        format!("workstation_connect={}", l.to_ascii_lowercase())
    } else {
        l.to_string()
    }
}

/// (directives, where they came from): RUST_LOG > config `log_level` > `default`.
pub fn directives(
    env: Option<&str>,
    config: Option<&str>,
    default: &str,
) -> (String, &'static str) {
    match (
        env.filter(|s| !s.trim().is_empty()),
        config.filter(|s| !s.trim().is_empty()),
    ) {
        (Some(e), _) => (e.trim().to_string(), "RUST_LOG"),
        (None, Some(c)) => (expand(c), "config log_level"),
        (None, None) => (default.to_string(), "default"),
    }
}

fn priority(level: Level) -> u8 {
    match level {
        Level::ERROR => 3,
        Level::WARN => 4,
        Level::INFO => 6,
        _ => 7,
    }
}

/// Wraps a formatter with the sd-daemon `<N>` priority prefix when writing to the journal.
struct Priority<F> {
    inner: F,
    journal: bool,
}

impl<S, N, F> FormatEvent<S, N> for Priority<F>
where
    S: tracing::Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
    F: FormatEvent<S, N>,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        if self.journal {
            write!(writer, "<{}>", priority(*event.metadata().level()))?;
        }
        self.inner.format_event(ctx, writer, event)
    }
}

/// Installs the global subscriber and a panic hook that logs through it. Returns the active
/// filter (for the daemon's start line).
pub fn init(kind: Kind, config_level: Option<&str>) -> String {
    let (journal, terminal) = match kind {
        Kind::Daemon => (
            std::env::var_os("JOURNAL_STREAM").is_some() && !std::io::stdout().is_terminal(),
            std::io::stdout().is_terminal(),
        ),
        Kind::Cli => (
            std::env::var_os("JOURNAL_STREAM").is_some() && !std::io::stderr().is_terminal(),
            std::io::stderr().is_terminal(),
        ),
    };
    let default = match kind {
        Kind::Daemon => "workstation_connect=info",
        Kind::Cli if journal => "workstation_connect=info",
        Kind::Cli => "workstation_connect=warn",
    };
    let config_level = config_level.filter(|_| kind == Kind::Daemon);
    let env = std::env::var("RUST_LOG").ok();
    let (wanted, source) = directives(env.as_deref(), config_level, default);
    let (filter, active, bad) = match EnvFilter::try_new(&wanted) {
        Ok(f) => (f, format!("{wanted} ({source})"), None),
        Err(e) => (
            EnvFilter::new(default),
            format!("{default} (default)"),
            Some(format!("{source} {wanted:?}: {e}")),
        ),
    };
    let writer = match kind {
        Kind::Daemon => BoxMakeWriter::new(std::io::stdout),
        Kind::Cli => BoxMakeWriter::new(std::io::stderr),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(terminal)
        .event_format(Priority {
            // journald adds timestamps; the CLI doesn't need them
            inner: fmt::format().without_time(),
            journal,
        })
        .init();
    if let Some(b) = bad {
        warn!("invalid log filter, using the default: {b}");
    }
    std::panic::set_hook(Box::new(|info| {
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(non-string payload)".into());
        let location = info.location().map(|l| l.to_string()).unwrap_or_default();
        let thread = std::thread::current().name().unwrap_or("?").to_string();
        let bt = std::backtrace::Backtrace::capture();
        if bt.status() == std::backtrace::BacktraceStatus::Captured {
            error!(%location, %thread, "panic: {msg}\n{bt}");
        } else {
            error!(%location, %thread, "panic: {msg}");
        }
    }));
    active
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_filter_source() {
        assert_eq!(
            directives(
                Some("workstation_connect=trace"),
                Some("debug"),
                "workstation_connect=info"
            ),
            ("workstation_connect=trace".into(), "RUST_LOG")
        );
        assert_eq!(
            directives(Some(" "), Some("DEBUG"), "workstation_connect=info"),
            ("workstation_connect=debug".into(), "config log_level")
        );
        assert_eq!(
            directives(
                None,
                Some("workstation_connect=debug,zbus=info"),
                "workstation_connect=info"
            ),
            (
                "workstation_connect=debug,zbus=info".into(),
                "config log_level"
            )
        );
        assert_eq!(
            directives(None, None, "workstation_connect=info"),
            ("workstation_connect=info".into(), "default")
        );
    }

    #[test]
    fn priorities() {
        assert_eq!(priority(Level::ERROR), 3);
        assert_eq!(priority(Level::WARN), 4);
        assert_eq!(priority(Level::INFO), 6);
        assert_eq!(priority(Level::TRACE), 7);
    }
}
