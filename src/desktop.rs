//! Desktop integration: an application entry (so the applet can be started again from the
//! menu after quitting), starting at login, and opening the desktop's own settings.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// File name of both desktop entries.
const ENTRY: &str = "power-applet.desktop";

fn applications_path() -> PathBuf {
    data_home().join("applications").join(ENTRY)
}

fn autostart_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::home().join(".config"))
        .join("autostart")
        .join(ENTRY)
}

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::home().join(".local/share"))
}

/// Writes the application entry, rewritten on every start so it follows the binary if it
/// moves. The first time, also starts the applet at login.
pub fn install() -> Result<()> {
    let first_run = !applications_path().exists();
    write(&applications_path(), entry(false)?.as_bytes())?;
    if first_run {
        set_autostart(true)?;
    } else if autostart() {
        // Keep Exec= pointing at the current binary.
        set_autostart(true)?;
    }
    Ok(())
}

pub fn autostart() -> bool {
    autostart_path().exists()
}

/// Writes or removes the autostart entry.
pub fn set_autostart(on: bool) -> Result<()> {
    let path = autostart_path();
    if !on {
        return match fs::remove_file(&path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        };
    }
    write(&path, entry(true)?.as_bytes())
}

fn entry(autostart: bool) -> Result<String> {
    let exe = std::env::current_exe().context("locating the applet binary")?;
    let mut text = format!(
        "[Desktop Entry]\nType=Application\nName={name}\nComment=Battery levels, power mode and sleep inhibitors in the tray\nExec={exec}\nIcon=power-applet-100-symbolic\nTerminal=false\nCategories=System;Settings;\n",
        name = crate::APP_NAME,
        exec = quote(&exe.to_string_lossy()),
    );
    if autostart {
        text.push_str("X-GNOME-Autostart-enabled=true\n");
    }
    Ok(text)
}

/// Desktop entry quoting: wrap in double quotes, escape `"`, `` ` ``, `$` and `\`.
fn quote(arg: &str) -> String {
    let mut quoted = String::from('"');
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

fn write(path: &Path, data: &[u8]) -> Result<()> {
    // Leave the file alone when unchanged, so desktops watching it don't reload.
    if fs::read(path).is_ok_and(|old| old == data) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(path, data).with_context(|| format!("writing {}", path.display()))
}

/// Power settings by desktop: the first whose program is installed is used.
const POWER_SETTINGS: &[&[&str]] = &[
    &["cinnamon-settings", "power"],
    &["gnome-control-center", "power"],
    &["systemsettings", "kcm_powerdevilprofilesconfig"],
    &["mate-power-preferences"],
    &["xfce4-power-manager-settings"],
];

pub fn open_power_settings() {
    run_first(POWER_SETTINGS);
}

fn run_first(candidates: &[&[&str]]) {
    let Some(cmd) = candidates.iter().find(|c| which(c[0])) else {
        log::warn!("no settings program found (tried {})", candidates.iter().map(|c| c[0]).collect::<Vec<_>>().join(", "));
        return;
    };
    match Command::new(cmd[0]).args(&cmd[1..]).spawn() {
        // Reap it when it exits, so it doesn't linger as a zombie.
        Ok(mut child) => {
            std::thread::spawn(move || child.wait());
        }
        Err(e) => log::warn!("starting {}: {e}", cmd[0]),
    }
}

fn which(cmd: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
}
