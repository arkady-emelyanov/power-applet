//! Everything the applet shows, read over D-Bus: battery devices from UPower, the power
//! mode from power-profiles-daemon, and sleep inhibitors from logind and the session
//! manager.
//!
//! A monitor thread re-reads the whole snapshot whenever one of these services signals a
//! change, after resume, and on a timer as a safety net. Reading everything every time
//! is cheap (a few dozen small calls) and means the applet can never get stuck on a value
//! one service forgot to announce, which is what Cinnamon's own applet does with
//! peripherals.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const UPOWER: &str = "org.freedesktop.UPower";
const UPOWER_PATH: &str = "/org/freedesktop/UPower";
const UPOWER_DEVICE: &str = "org.freedesktop.UPower.Device";
const DISPLAY_DEVICE: &str = "/org/freedesktop/UPower/devices/DisplayDevice";
/// power-profiles-daemon: the new name first, then the one used up to 0.20.
const PROFILES: [(&str, &str); 2] = [
    ("org.freedesktop.UPower.PowerProfiles", "/org/freedesktop/UPower/PowerProfiles"),
    ("net.hadess.PowerProfiles", "/net/hadess/PowerProfiles"),
];
const LOGIND: &str = "org.freedesktop.login1";
const LOGIND_PATH: &str = "/org/freedesktop/login1";
const SESSION_MANAGER: &str = "org.gnome.SessionManager";

/// A device at or below this charge gets the warning badge on the tray icon.
pub const LOW_PERCENT: f64 = 20.0;

/// How often to re-read everything without being told to.
const SLOW_POLL: Duration = Duration::from_secs(30);
/// While the popup is open: logind doesn't announce new inhibitors.
const FAST_POLL: Duration = Duration::from_secs(3);
/// Signals tend to come in bursts (one per changed property); wait for the rest.
const DEBOUNCE: Duration = Duration::from_millis(150);

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Snapshot {
    /// UPower's summary of what powers the computer (the laptop battery or a UPS), if any.
    pub main: Option<Device>,
    /// Every battery UPower knows about: laptop batteries, UPSes and peripherals.
    pub devices: Vec<Device>,
    /// `None` if power-profiles-daemon isn't running.
    pub profiles: Option<Profiles>,
    pub inhibitors: Vec<Inhibitor>,
    /// Programs that only postpone sleep for a moment to clean up.
    pub delays: Vec<Inhibitor>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub path: String,
    pub kind: Kind,
    pub model: String,
    pub vendor: String,
    pub percentage: f64,
    pub state: State,
    /// UPower's own assessment: 3 low, 4 critical, 5 action (see `low`).
    pub warning_level: u32,
    /// Whether it powers the computer (laptop battery, UPS) rather than a peripheral.
    pub power_supply: bool,
    pub time_to_empty: i64,
    pub time_to_full: i64,
    /// Unix time of the last reading.
    pub updated: u64,
}

impl Device {
    pub fn low(&self) -> bool {
        self.percentage <= LOW_PERCENT || self.warning_level >= 3
    }

    pub fn name(&self) -> String {
        if !self.model.is_empty() {
            self.model.clone()
        } else if !self.vendor.is_empty() {
            format!("{} {}", self.vendor, self.kind.label())
        } else {
            self.kind.label().to_owned()
        }
    }
}

/// UPower's device types (`Type`), the ones that matter here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    LinePower,
    Battery,
    Ups,
    Mouse,
    Keyboard,
    Phone,
    Tablet,
    Gaming,
    Pen,
    Touchpad,
    Audio,
    Other,
}

impl Kind {
    fn from_upower(t: u32) -> Self {
        match t {
            1 => Self::LinePower,
            2 => Self::Battery,
            3 => Self::Ups,
            5 => Self::Mouse,
            6 => Self::Keyboard,
            8 => Self::Phone,
            10 => Self::Tablet,
            12 => Self::Gaming,
            13 => Self::Pen,
            14 => Self::Touchpad,
            17..=21 => Self::Audio,
            _ => Self::Other,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::LinePower => "power",
            Self::Battery => "battery",
            Self::Ups => "ups",
            Self::Mouse => "mouse",
            Self::Keyboard => "keyboard",
            Self::Phone => "phone",
            Self::Tablet => "tablet",
            Self::Gaming => "gaming",
            Self::Pen => "pen",
            Self::Touchpad => "touchpad",
            Self::Audio => "audio",
            Self::Other => "other",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::LinePower => "AC adapter",
            Self::Battery => "Battery",
            Self::Ups => "UPS",
            Self::Mouse => "Mouse",
            Self::Keyboard => "Keyboard",
            Self::Phone => "Phone",
            Self::Tablet => "Tablet",
            Self::Gaming => "Game controller",
            Self::Pen => "Pen",
            Self::Touchpad => "Touchpad",
            Self::Audio => "Headset",
            Self::Other => "Device",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Unknown,
    Charging,
    Discharging,
    Empty,
    Full,
    PendingCharge,
    PendingDischarge,
}

impl State {
    fn from_upower(s: u32) -> Self {
        match s {
            1 => Self::Charging,
            2 => Self::Discharging,
            3 => Self::Empty,
            4 => Self::Full,
            5 => Self::PendingCharge,
            6 => Self::PendingDischarge,
            _ => Self::Unknown,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Unknown => "",
            Self::Charging => "Charging",
            Self::Discharging => "Discharging",
            Self::Empty => "Empty",
            Self::Full => "Fully charged",
            Self::PendingCharge => "Not charging",
            Self::PendingDischarge => "Waiting to discharge",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Profiles {
    /// In the daemon's order: power-saver, balanced, performance (when available).
    pub available: Vec<String>,
    pub active: String,
    /// Why performance mode is running slower than it should, e.g. "high-operating-temperature".
    pub degraded: String,
    /// Programs holding a profile (`HoldProfile`), e.g. a game asking for performance.
    pub holds: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Inhibitor {
    pub who: String,
    pub why: String,
    pub blocks_sleep: bool,
    /// Keeps the screen on and the screensaver from starting.
    pub blocks_idle: bool,
}

/// Reads the snapshot in the background and sends it to `on_change` whenever it changes.
pub struct Monitor {
    tx: Sender<()>,
    fast: Arc<AtomicBool>,
}

impl Monitor {
    pub fn spawn(on_change: impl Fn(Snapshot) + Send + 'static) -> Result<Self> {
        let system = Connection::system().context("connecting to the system bus")?;
        let session = Connection::session().inspect_err(|e| log::warn!("no session bus: {e}")).ok();
        let (tx, rx) = mpsc::channel();
        let fast = Arc::new(AtomicBool::new(false));

        watch(&system, "type='signal',sender='org.freedesktop.UPower'", tx.clone());
        for (name, _) in PROFILES {
            watch(&system, &format!("type='signal',sender='{name}'"), tx.clone());
        }
        // Resume: peripherals report fresh levels shortly after, but re-read right away too.
        watch(&system, "type='signal',interface='org.freedesktop.login1.Manager',member='PrepareForSleep'", tx.clone());
        watch(&system, "type='signal',sender='org.freedesktop.login1',path='/org/freedesktop/login1',member='PropertiesChanged'", tx.clone());
        if let Some(s) = &session {
            watch(s, "type='signal',interface='org.gnome.SessionManager'", tx.clone());
        }

        let poll_fast = fast.clone();
        thread::Builder::new().name("power-monitor".into()).spawn(move || {
            let mut last = None;
            loop {
                let wait = if poll_fast.load(Ordering::Relaxed) { FAST_POLL } else { SLOW_POLL };
                match rx.recv_timeout(wait) {
                    Ok(()) => {
                        thread::sleep(DEBOUNCE);
                        while rx.try_recv().is_ok() {}
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => break,
                }
                let snapshot = read(&system, session.as_ref());
                if last.as_ref() != Some(&snapshot) {
                    log::debug!("snapshot changed: {snapshot:?}");
                    last = Some(snapshot.clone());
                    on_change(snapshot);
                }
            }
        })?;
        let monitor = Self { tx, fast };
        monitor.refresh();
        Ok(monitor)
    }

    /// Re-read now, e.g. when the popup opens.
    pub fn refresh(&self) {
        let _ = self.tx.send(());
    }

    /// Poll often (while the popup is open) or rarely.
    pub fn set_fast(&self, fast: bool) {
        self.fast.store(fast, Ordering::Relaxed);
    }
}

/// Pokes `tx` for every message matching `rule`, from a thread of its own.
fn watch(conn: &Connection, rule: &str, tx: Sender<()>) {
    let iter = match MessageIterator::for_match_rule(rule, conn, Some(64)) {
        Ok(i) => i,
        Err(e) => {
            log::warn!("cannot watch {rule}: {e}");
            return;
        }
    };
    let _ = thread::Builder::new().name("dbus-watch".into()).spawn(move || {
        for _ in iter {
            if tx.send(()).is_err() {
                break;
            }
        }
    });
}

pub fn read(system: &Connection, session: Option<&Connection>) -> Snapshot {
    let (devices, main) = read_devices(system).unwrap_or_else(|e| {
        log::warn!("reading UPower devices: {e:#}");
        (Vec::new(), None)
    });
    let profiles = read_profiles(system).inspect_err(|e| log::debug!("power profiles: {e:#}")).ok();
    let (mut inhibitors, delays) = read_logind_inhibitors(system).unwrap_or_else(|e| {
        log::warn!("reading logind inhibitors: {e:#}");
        Default::default()
    });
    if let Some(s) = session {
        match read_session_inhibitors(s) {
            Ok(list) => inhibitors.extend(list),
            Err(e) => log::debug!("session manager inhibitors: {e:#}"),
        }
    }
    Snapshot { main, devices, profiles, inhibitors, delays }
}

fn props(conn: &Connection, dest: &str, path: &str, iface: &str) -> Result<HashMap<String, OwnedValue>> {
    let proxy = Proxy::new(conn, dest.to_owned(), path.to_owned(), "org.freedesktop.DBus.Properties")?;
    Ok(proxy.call("GetAll", &(iface,))?)
}

fn read_devices(conn: &Connection) -> Result<(Vec<Device>, Option<Device>)> {
    let upower = Proxy::new(conn, UPOWER, UPOWER_PATH, UPOWER)?;
    let paths: Vec<OwnedObjectPath> = upower.call("EnumerateDevices", &())?;
    let mut devices = Vec::new();
    for p in paths {
        match props(conn, UPOWER, p.as_str(), UPOWER_DEVICE) {
            Ok(map) => {
                let d = device(p.as_str(), &map);
                // Line power has no charge to show; absent batteries (empty laptop bays,
                // peripherals UPower still remembers) have nothing to say either.
                if d.kind != Kind::LinePower && present(&map) {
                    devices.push(d);
                }
            }
            Err(e) => log::debug!("reading {}: {e}", p.as_str()),
        }
    }
    // Power supplies first (they're what the computer runs on), then by name.
    devices.sort_by(|a, b| b.power_supply.cmp(&a.power_supply).then_with(|| a.name().cmp(&b.name())));
    let main = props(conn, UPOWER, DISPLAY_DEVICE, UPOWER_DEVICE)
        .ok()
        .filter(|m| present(m) && u32_of(m, "Type") != 0)
        .map(|m| device(DISPLAY_DEVICE, &m));
    Ok((devices, main))
}

fn present(map: &HashMap<String, OwnedValue>) -> bool {
    map.get("IsPresent").and_then(|v| bool::try_from(&**v).ok()).unwrap_or(true)
}

fn device(path: &str, map: &HashMap<String, OwnedValue>) -> Device {
    let mut kind = Kind::from_upower(u32_of(map, "Type"));
    if matches!(kind, Kind::Mouse | Kind::Other) && is_audio(&str_of(map, "NativePath")) {
        kind = Kind::Audio;
    }
    Device {
        path: path.to_owned(),
        kind,
        model: str_of(map, "Model").trim().to_owned(),
        vendor: str_of(map, "Vendor").trim().to_owned(),
        percentage: map.get("Percentage").and_then(|v| f64::try_from(&**v).ok()).unwrap_or(0.0),
        state: State::from_upower(u32_of(map, "State")),
        warning_level: u32_of(map, "WarningLevel"),
        power_supply: map.get("PowerSupply").and_then(|v| bool::try_from(&**v).ok()).unwrap_or(false),
        time_to_empty: i64_of(map, "TimeToEmpty"),
        time_to_full: i64_of(map, "TimeToFull"),
        updated: map.get("UpdateTime").and_then(|v| u64::try_from(&**v).ok()).unwrap_or(0),
    }
}

/// Whether the power supply `native_path` belongs to an audio device that UPower took for
/// a mouse. UPower only reads `SOUND_FORM_FACTOR` from a sibling in the sound subsystem,
/// so a HID battery whose sibling is just an input node (a headset's battery reported
/// through uhid, say) falls back to "mouse", even when udev has tagged that input node
/// as a headset. Look at the input nodes' udev properties ourselves.
fn is_audio(native_path: &str) -> bool {
    if native_path.is_empty() || native_path.contains('/') {
        return false;
    }
    let dir = format!("/sys/class/power_supply/{native_path}/device/input");
    let Ok(entries) = std::fs::read_dir(dir) else { return false };
    entries.flatten().any(|e| {
        let name = e.file_name();
        std::fs::read_to_string(format!("/run/udev/data/+input:{}", name.to_string_lossy()))
            .is_ok_and(|db| audio_input(&db))
    })
}

/// Reads a udev database entry: an input node that udev calls a headset, headphones or
/// speakers, and that isn't also a mouse.
fn audio_input(db: &str) -> bool {
    let prop = |key: &str| db.lines().find_map(|l| l.strip_prefix("E:")?.strip_prefix(key)?.strip_prefix('='));
    prop("ID_INPUT_MOUSE") != Some("1") && matches!(prop("SOUND_FORM_FACTOR"), Some("headset" | "headphone" | "speaker"))
}

fn str_of(map: &HashMap<String, OwnedValue>, key: &str) -> String {
    match map.get(key).map(|v| &**v) {
        Some(Value::Str(s)) => s.as_str().to_owned(),
        _ => String::new(),
    }
}

fn u32_of(map: &HashMap<String, OwnedValue>, key: &str) -> u32 {
    map.get(key).and_then(|v| u32::try_from(&**v).ok()).unwrap_or(0)
}

fn i64_of(map: &HashMap<String, OwnedValue>, key: &str) -> i64 {
    map.get(key).and_then(|v| i64::try_from(&**v).ok()).unwrap_or(0)
}

fn profiles_proxy(conn: &Connection) -> Result<Proxy<'static>> {
    let mut last = None;
    for (name, path) in PROFILES {
        let proxy = Proxy::new(conn, name, path, name)?;
        match proxy.get_property::<String>("ActiveProfile") {
            Ok(_) => return Ok(proxy),
            Err(e) => last = Some(e),
        }
    }
    Err(last.map(Into::into).unwrap_or_else(|| anyhow::anyhow!("power-profiles-daemon not found")))
}

fn read_profiles(conn: &Connection) -> Result<Profiles> {
    let proxy = profiles_proxy(conn)?;
    let list: Vec<HashMap<String, OwnedValue>> = proxy.get_property("Profiles")?;
    let holds: Vec<HashMap<String, OwnedValue>> = proxy.get_property("ActiveProfileHolds").unwrap_or_default();
    Ok(Profiles {
        available: list.iter().map(|p| str_of(p, "Profile")).filter(|p| !p.is_empty()).collect(),
        active: proxy.get_property("ActiveProfile")?,
        degraded: proxy.get_property("PerformanceDegraded").unwrap_or_default(),
        holds: holds
            .iter()
            .map(|h| {
                let (app, profile, reason) = (str_of(h, "ApplicationId"), str_of(h, "Profile"), str_of(h, "Reason"));
                let reason = if reason.is_empty() { String::new() } else { format!(": {reason}") };
                format!("{} holds {profile}{reason}", app_name(&app))
            })
            .collect(),
    })
}

pub fn set_profile(profile: &str) -> Result<()> {
    let conn = Connection::system()?;
    let proxy = profiles_proxy(&conn)?;
    proxy.set_property("ActiveProfile", profile).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(())
}

/// logind's inhibitors that keep the computer from sleeping (`block`), and the ones that
/// merely postpone it (`delay`). Locks on power keys and the lid switch are left out:
/// they're the desktop handling those itself.
fn read_logind_inhibitors(conn: &Connection) -> Result<(Vec<Inhibitor>, Vec<Inhibitor>)> {
    let proxy = Proxy::new(conn, LOGIND, LOGIND_PATH, "org.freedesktop.login1.Manager")?;
    let list: Vec<(String, String, String, String, u32, u32)> = proxy.call("ListInhibitors", &())?;
    let (mut block, mut delay) = (Vec::new(), Vec::new());
    for (what, who, why, mode, _uid, _pid) in list {
        let whats: Vec<&str> = what.split(':').collect();
        let i = Inhibitor {
            who: app_name(&who),
            why,
            blocks_sleep: whats.contains(&"sleep"),
            blocks_idle: whats.contains(&"idle"),
        };
        if !i.blocks_sleep && !i.blocks_idle {
            continue;
        }
        if mode == "block" { block.push(i) } else { delay.push(i) }
    }
    Ok((block, delay))
}

/// Inhibitors registered with the desktop session (what video players and browsers use,
/// including through org.freedesktop.ScreenSaver).
fn read_session_inhibitors(conn: &Connection) -> Result<Vec<Inhibitor>> {
    const SUSPEND: u32 = 4;
    const IDLE: u32 = 8;
    let manager = Proxy::new(conn, SESSION_MANAGER, "/org/gnome/SessionManager", SESSION_MANAGER)?;
    let paths: Vec<OwnedObjectPath> = manager.call("GetInhibitors", &())?;
    let mut out = Vec::new();
    for p in paths {
        let inhibitor = Proxy::new(conn, SESSION_MANAGER, p.as_str().to_owned(), "org.gnome.SessionManager.Inhibitor")?;
        let flags: u32 = inhibitor.call("GetFlags", &()).unwrap_or(0);
        if flags & (SUSPEND | IDLE) == 0 {
            continue;
        }
        let app: String = inhibitor.call("GetAppId", &()).unwrap_or_default();
        out.push(Inhibitor {
            who: app_name(&app),
            why: inhibitor.call("GetReason", &()).unwrap_or_default(),
            blocks_sleep: flags & SUSPEND != 0,
            blocks_idle: flags & IDLE != 0,
        });
    }
    Ok(out)
}

/// A readable name from an app id: `/usr/share/applications/org.gnome.Totem.desktop` and
/// `org.gnome.Totem` become `Totem`.
pub fn app_name(id: &str) -> String {
    let base = id.rsplit('/').next().unwrap_or(id);
    let base = base.strip_suffix(".desktop").unwrap_or(base);
    let last = if base.matches('.').count() >= 2 { base.rsplit('.').next().unwrap_or(base) } else { base };
    if last.is_empty() { "Unknown".to_owned() } else { last.to_owned() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_names() {
        assert_eq!(app_name("/usr/share/applications/org.gnome.Totem.desktop"), "Totem");
        assert_eq!(app_name("firefox"), "firefox");
        assert_eq!(app_name("firefox.desktop"), "firefox");
        assert_eq!(app_name("signal-desktop"), "signal-desktop");
        assert_eq!(app_name(""), "Unknown");
    }

    #[test]
    fn low_battery() {
        let mut d = Device {
            path: String::new(),
            kind: Kind::Mouse,
            model: String::new(),
            vendor: String::new(),
            percentage: 21.0,
            state: State::Discharging,
            warning_level: 1,
            power_supply: false,
            time_to_empty: 0,
            time_to_full: 0,
            updated: 0,
        };
        assert!(!d.low());
        d.percentage = 20.0;
        assert!(d.low());
        d.percentage = 50.0;
        d.warning_level = 3;
        assert!(d.low());
        assert_eq!(d.name(), "Mouse");
    }

    #[test]
    fn audio_inputs() {
        let headset = "I:13174676\nE:SOUND_INITIALIZED=1\nE:SOUND_FORM_FACTOR=headset\nE:ID_INPUT=1\nE:ID_INPUT_KEY=1\nG:seat\n";
        assert!(audio_input(headset));
        assert!(!audio_input("E:ID_INPUT=1\nE:ID_INPUT_MOUSE=1\n"));
        assert!(!audio_input("E:SOUND_FORM_FACTOR=headset\nE:ID_INPUT_MOUSE=1\n"));
        assert!(!audio_input("E:SOUND_FORM_FACTOR=webcam\n"));
        assert!(!is_audio(""));
        assert!(!is_audio("../x"));
    }

    /// Reads the real system; run with `cargo test -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn dump_snapshot() {
        let system = Connection::system().unwrap();
        let session = Connection::session().ok();
        println!("{:#?}", read(&system, session.as_ref()));
    }
}

#[cfg(test)]
mod live_tests {
    /// Switches the real power mode to balanced and back; run with
    /// `cargo test -- --ignored switch_profile`.
    #[test]
    #[ignore]
    fn switch_profile() {
        let system = zbus::blocking::Connection::system().unwrap();
        let before = super::read_profiles(&system).unwrap().active;
        let other = if before == "balanced" { "performance" } else { "balanced" };
        super::set_profile(other).unwrap();
        assert_eq!(super::read_profiles(&system).unwrap().active, other);
        super::set_profile(&before).unwrap();
        assert_eq!(super::read_profiles(&system).unwrap().active, before);
    }
}
