//! TRANSITIONAL: self-migration of a pre-rename install (`wsconnect`) at daemon start.
//! Idempotent and best-effort: failures are logged, the daemon starts anyway.
//! Remove together with src/bin/wsconnect.rs (and the old unit names in logs.rs / update.rs)
//! once every laptop runs workstation-connect.service: no ~/.config/systemd/user/wsconnect.service,
//! no "rutger.wsconnect" in ~/.config/omarchy/shell.json, no `wsconnect` in the cargo bin dir.

use crate::config::xdg_config;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

const OLD: &str = "wsconnect";
const NEW: &str = "workstation-connect";
const OLD_UNIT: &str = "wsconnect.service";
const NEW_UNIT: &str = "workstation-connect.service";
const OLD_PLUGIN: &str = "rutger.wsconnect";
const NEW_PLUGIN: &str = "rutger.workstation-connect";
const UNIT_FILE: &str = include_str!("../dist/workstation-connect.service");

pub enum Outcome {
    Continue,
    /// Started workstation-connect.service from inside the old unit; Conflicts= stops us.
    HandedOff,
}

pub fn run() -> Outcome {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let xdg = |var: &str, default: &str| {
        std::env::var_os(var)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(default))
    };
    let state = xdg("XDG_STATE_HOME", ".local/state");
    let cache = xdg("XDG_CACHE_HOME", ".cache");
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);

    try_step("state dir", || {
        move_if_new_absent(&state.join(OLD), &state.join(NEW))
    });
    try_step("old cache dir", || remove_dir(&cache.join(OLD)));
    try_step("kde paused providers", || {
        move_if_new_absent(
            &runtime.join(format!("{OLD}-paused-providers")),
            &runtime.join(format!("{NEW}-paused-providers")),
        )
    });

    let units = xdg_config().join("systemd/user");
    try_step("install unit", || install_unit(&units));

    let unit = std::fs::read_to_string("/proc/self/cgroup")
        .ok()
        .and_then(|c| cgroup_unit(&c).map(str::to_string));
    if units.join(OLD_UNIT).exists() {
        try_step("retire old unit", || retire_old_unit(&units));
        if unit.as_deref() == Some(OLD_UNIT) {
            match systemctl(&["--no-block", "start", NEW_UNIT]) {
                Ok(()) => {
                    info!("migrate: started {NEW_UNIT}, it replaces this {OLD_UNIT} process");
                    return Outcome::HandedOff;
                }
                Err(e) => warn!("migrate: start {NEW_UNIT}: {e:#}; keep running in {OLD_UNIT}"),
            }
        }
    }

    try_step("omarchy shell.json", || {
        migrate_shell_json(&xdg_config().join("omarchy"))
    });
    if unit.as_deref() == Some(NEW_UNIT) {
        try_step("remove wsconnect shim", remove_shim);
    }
    Outcome::Continue
}

fn try_step(what: &str, f: impl FnOnce() -> Result<()>) {
    if let Err(e) = f() {
        warn!("migrate: {what}: {e:#}");
    }
}

fn move_if_new_absent(old: &Path, new: &Path) -> Result<()> {
    if !old.exists() {
        return Ok(());
    }
    if new.exists() {
        warn!(
            "migrate: both {} and {} exist, keeping the new one",
            old.display(),
            new.display()
        );
        return Ok(());
    }
    std::fs::rename(old, new)
        .with_context(|| format!("move {} → {}", old.display(), new.display()))?;
    info!("migrate: moved {} → {}", old.display(), new.display());
    Ok(())
}

fn remove_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        std::fs::remove_dir_all(dir).with_context(|| format!("remove {}", dir.display()))?;
        info!("migrate: removed {}", dir.display());
    }
    Ok(())
}

fn systemctl(args: &[&str]) -> Result<()> {
    let out = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .context("start systemctl")?;
    if !out.status.success() {
        bail!(
            "systemctl --user {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Writes the unit when missing or outdated; enables it only when it was missing (a later
/// `systemctl --user disable` stays respected).
fn install_unit(units: &Path) -> Result<()> {
    let path = units.join(NEW_UNIT);
    let current = std::fs::read_to_string(&path).ok();
    if current.as_deref() == Some(UNIT_FILE) {
        return Ok(());
    }
    std::fs::create_dir_all(units)?;
    std::fs::write(&path, UNIT_FILE).with_context(|| format!("write {}", path.display()))?;
    systemctl(&["daemon-reload"])?;
    if current.is_none() {
        systemctl(&["enable", NEW_UNIT])?;
    }
    info!("migrate: installed {}", path.display());
    Ok(())
}

fn retire_old_unit(units: &Path) -> Result<()> {
    if let Err(e) = systemctl(&["disable", OLD_UNIT]) {
        warn!("migrate: {e:#}");
    }
    let path = units.join(OLD_UNIT);
    std::fs::remove_file(&path).with_context(|| format!("remove {}", path.display()))?;
    systemctl(&["daemon-reload"])?;
    info!("migrate: removed {}", path.display());
    Ok(())
}

/// The systemd unit of a /proc/self/cgroup (v2: "0::/user.slice/.../app.slice/x.service").
fn cgroup_unit(cgroup: &str) -> Option<&str> {
    cgroup
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .and_then(|path| path.trim().rsplit('/').next())
        .filter(|u| u.ends_with(".service"))
}

/// Bar layout entries with the old plugin id → new id (dropped when the new one is already
/// there). Everything else untouched. None: nothing to change.
fn rename_plugin(shell: &str) -> Result<Option<String>> {
    let mut json: Value = serde_json::from_str(shell).context("parse")?;
    let Some(layout) = json
        .pointer_mut("/bar/layout")
        .and_then(Value::as_object_mut)
    else {
        return Ok(None);
    };
    let id = |v: &Value| v.get("id").and_then(Value::as_str).map(str::to_string);
    let has_new = layout
        .values()
        .filter_map(Value::as_array)
        .flatten()
        .any(|e| id(e).as_deref() == Some(NEW_PLUGIN));
    let mut changed = false;
    for section in layout.values_mut().filter_map(Value::as_array_mut) {
        if has_new {
            let before = section.len();
            section.retain(|e| id(e).as_deref() != Some(OLD_PLUGIN));
            changed |= section.len() != before;
        } else {
            for e in section.iter_mut() {
                if id(e).as_deref() == Some(OLD_PLUGIN) {
                    e["id"] = Value::from(NEW_PLUGIN);
                    changed = true;
                }
            }
        }
    }
    Ok(changed.then(|| serde_json::to_string_pretty(&json).unwrap() + "\n"))
}

/// Only once dev-env provides the new plugin: else the bar would point at a missing widget.
fn migrate_shell_json(omarchy: &Path) -> Result<()> {
    let path = omarchy.join("shell.json");
    let Ok(shell) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    if !omarchy.join("plugins").join(NEW_PLUGIN).exists() {
        if shell.contains(OLD_PLUGIN) {
            info!("migrate: shell.json keeps {OLD_PLUGIN}: plugin {NEW_PLUGIN} not there yet");
        }
        return Ok(());
    }
    let Some(new) = rename_plugin(&shell)? else {
        return Ok(());
    };
    // Atomic replace next to the real file (shell.json may be a symlink).
    let real = std::fs::canonicalize(&path)?;
    let tmp = real.with_extension("json.workstation-connect-tmp");
    std::fs::write(&tmp, new)?;
    std::fs::rename(&tmp, &real)?;
    info!("migrate: {}: {OLD_PLUGIN} → {NEW_PLUGIN}", path.display());
    Ok(())
}

/// argv[0] basename of a /proc/<pid>/cmdline.
fn argv0_name(cmdline: &[u8]) -> &[u8] {
    let arg0 = cmdline.split(|b| *b == 0).next().unwrap_or_default();
    arg0.rsplit(|b| *b == b'/').next().unwrap_or_default()
}

/// Something still started as `wsconnect` (the shim keeps argv[0]), e.g. the old bar widget's
/// `wsconnect waybar` in a shell that hasn't reloaded shell.json yet.
fn old_name_in_use() -> bool {
    let me = std::process::id().to_string();
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return true;
    };
    procs.flatten().any(|p| {
        p.file_name() != me.as_str()
            && std::fs::read(p.path().join("cmdline"))
                .is_ok_and(|c| argv0_name(&c) == OLD.as_bytes())
    })
}

/// The shim next to our binary: every `cargo install` recreates it, so this runs each start.
fn remove_shim() -> Result<()> {
    let exe = std::env::current_exe()?;
    let exe = match exe.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
        Some(s) => PathBuf::from(s),
        None => exe,
    };
    let shim = exe.with_file_name(OLD);
    if !shim.exists() {
        return Ok(());
    }
    let shell = xdg_config().join("omarchy/shell.json");
    if std::fs::read_to_string(&shell).is_ok_and(|s| s.contains(OLD_PLUGIN)) {
        info!(
            "migrate: keep {}: shell.json still uses {OLD_PLUGIN}",
            shim.display()
        );
        return Ok(());
    }
    if old_name_in_use() {
        info!(
            "migrate: keep {}: a `{OLD}` process still runs",
            shim.display()
        );
        return Ok(());
    }
    std::fs::remove_file(&shim).with_context(|| format!("remove {}", shim.display()))?;
    info!("migrate: removed {}", shim.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_v2_unit() {
        let c = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/wsconnect.service\n";
        assert_eq!(cgroup_unit(c), Some(OLD_UNIT));
        let c = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/workstation-connect.service";
        assert_eq!(cgroup_unit(c), Some(NEW_UNIT));
    }

    #[test]
    fn cgroup_not_a_service() {
        let c = "0::/user.slice/user-1000.slice/session-2.scope\n";
        assert_eq!(cgroup_unit(c), None);
        assert_eq!(cgroup_unit("12:pids:/foo.service\n"), None);
        assert_eq!(cgroup_unit(""), None);
    }

    #[test]
    fn shell_json_renames_in_place() {
        let s = r#"{"plugins":[{"id":"rutger.emojis"}],"bar":{"layout":{"left":[{"id":"omarchy.clock"}],"right":[{"id":"x"},{"id":"rutger.wsconnect","extra":1},{"id":"omarchy.bluetooth"}]}},"a":true}"#;
        let out = rename_plugin(s).unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            v["bar"]["layout"]["right"],
            serde_json::json!([{"id":"x"},{"id":"rutger.workstation-connect","extra":1},{"id":"omarchy.bluetooth"}])
        );
        assert_eq!(v["a"], true);
        assert_eq!(v["plugins"], serde_json::json!([{"id":"rutger.emojis"}]));
        // Key order is preserved.
        assert!(out.find("\"plugins\"").unwrap() < out.find("\"bar\"").unwrap());
        assert!(out.find("\"bar\"").unwrap() < out.find("\"a\"").unwrap());
        assert!(!out.contains(OLD_PLUGIN));
    }

    #[test]
    fn shell_json_no_duplicate() {
        let s = r#"{"bar":{"layout":{"center":[{"id":"rutger.workstation-connect"}],"right":[{"id":"rutger.wsconnect"},{"id":"y"}]}}}"#;
        let v: Value = serde_json::from_str(&rename_plugin(s).unwrap().unwrap()).unwrap();
        assert_eq!(v["bar"]["layout"]["right"], serde_json::json!([{"id":"y"}]));
        assert_eq!(
            v["bar"]["layout"]["center"],
            serde_json::json!([{"id":"rutger.workstation-connect"}])
        );
    }

    #[test]
    fn shell_json_unchanged() {
        assert!(
            rename_plugin(r#"{"bar":{"layout":{"right":[{"id":"x"}]}}}"#)
                .unwrap()
                .is_none()
        );
        assert!(rename_plugin(r#"{"bar":{}}"#).unwrap().is_none());
        assert!(rename_plugin("{}").unwrap().is_none());
        assert!(rename_plugin("not json").is_err());
    }

    #[test]
    fn argv0() {
        assert_eq!(argv0_name(b"wsconnect\0waybar\0"), b"wsconnect");
        assert_eq!(
            argv0_name(b"/home/r/.cargo/bin/wsconnect\0menu\0"),
            b"wsconnect"
        );
        assert_eq!(
            argv0_name(b"workstation-connect\0daemon\0"),
            b"workstation-connect"
        );
        assert_eq!(argv0_name(b""), b"");
    }

    #[test]
    fn unit_file_matches_new_name() {
        assert!(UNIT_FILE.contains("ExecStart=/usr/bin/env workstation-connect daemon"));
        assert!(UNIT_FILE.contains("Conflicts=wsconnect.service"));
    }
}
