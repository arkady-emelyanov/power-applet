mod desktop;
mod icon;
mod power;
mod tray;
mod ui;

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tao::dpi::PhysicalPosition;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};

use crate::icon::Look;
use crate::power::{Device, Inhibitor, Monitor, Snapshot};
use crate::tray::{Tray, TrayAction, TrayState};
use crate::ui::{DeviceView, InhibitorView, MainView, ProfileView, StateView, UiMsg, View};

/// Display name, used for the tray and the application entry. Not just "Power": Cinnamon
/// hides tray icons named after one of its own applets (power, battery) while that applet
/// is in the panel.
pub const APP_NAME: &str = "Power Applet";
/// A click on the tray icon right after the popup lost focus is the same click that
/// dismissed it; don't reopen.
const REOPEN_GUARD: Duration = Duration::from_millis(300);

#[derive(Debug)]
pub enum AppEvent {
    TrayClick(PhysicalPosition<i32>),
    TrayAction(TrayAction),
    Ui(UiMsg),
    Power(Box<Snapshot>),
    /// Switching the power mode failed; the message is shown in the popup.
    ProfileFailed(String),
}

struct App {
    proxy: EventLoopProxy<AppEvent>,
    tray: Option<Tray>,
    popup: Option<View>,
    monitor: Option<Monitor>,
    snapshot: Snapshot,
    profile_error: Option<String>,
    popup_hidden_at: Option<Instant>,
    popup_anchor: Option<PhysicalPosition<i32>>,
}

fn main() -> Result<()> {
    match std::env::args().nth(1).as_deref() {
        Some("--version") => {
            println!("power-applet {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        Some("--help" | "-h") => {
            println!("power-applet {}\n{}\n\nRun without arguments to start the tray applet.\nRUST_LOG=power_applet=debug for detailed logs.", env!("CARGO_PKG_VERSION"), env!("CARGO_PKG_DESCRIPTION"));
            return Ok(());
        }
        Some(arg) => anyhow::bail!("unknown argument {arg:?} (try --help)"),
        None => {}
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let _lock = match lock_instance() {
        Ok(f) => f,
        Err(e) => {
            log::error!("{e}");
            std::process::exit(1);
        }
    };

    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let mut app = App {
        proxy: event_loop.create_proxy(),
        tray: None,
        popup: None,
        monitor: None,
        snapshot: Snapshot::default(),
        profile_error: None,
        popup_hidden_at: None,
        popup_anchor: None,
    };
    event_loop.run(move |event, target, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => app.start(target, control_flow),
            Event::WindowEvent { window_id, event, .. } if app.popup.as_ref().is_some_and(|p| p.id() == window_id) => {
                match event {
                    WindowEvent::Focused(false) => app.hide_popup("lost focus"),
                    WindowEvent::CloseRequested => app.hide_popup("close requested"),
                    _ => {}
                }
            }
            Event::UserEvent(ev) => app.user_event(ev, control_flow),
            _ => {}
        }
    });
}

impl App {
    fn start(&mut self, target: &tao::event_loop::EventLoopWindowTarget<AppEvent>, control_flow: &mut ControlFlow) {
        match Tray::new(self.proxy.clone()) {
            Ok(t) => self.tray = Some(t),
            Err(e) => {
                log::error!("cannot create tray icon: {e:#}");
                *control_flow = ControlFlow::Exit;
                return;
            }
        }
        // Create the popup up front so it opens instantly.
        match View::popup(target, self.proxy.clone()) {
            Ok(p) => self.popup = Some(p),
            Err(e) => log::error!("creating popup: {e:#}"),
        }
        if let Err(e) = desktop::install() {
            log::warn!("installing the application entry: {e:#}");
        }
        let proxy = self.proxy.clone();
        match Monitor::spawn(move |s| {
            let _ = proxy.send_event(AppEvent::Power(Box::new(s)));
        }) {
            Ok(m) => self.monitor = Some(m),
            Err(e) => log::error!("cannot read power devices: {e:#}"),
        }
        self.refresh();
    }

    fn user_event(&mut self, ev: AppEvent, control_flow: &mut ControlFlow) {
        match ev {
            AppEvent::TrayClick(pos) => self.toggle_popup(pos),
            AppEvent::TrayAction(action) => match action {
                TrayAction::PowerSettings => desktop::open_power_settings(),
                TrayAction::Quit => *control_flow = ControlFlow::Exit,
            },
            AppEvent::Ui(msg) => self.ui_message(msg, control_flow),
            AppEvent::Power(snapshot) => {
                // A switch that went through clears the error from an earlier attempt.
                if snapshot.profiles.as_ref().map(|p| &p.active) != self.snapshot.profiles.as_ref().map(|p| &p.active) {
                    self.profile_error = None;
                }
                self.snapshot = *snapshot;
                self.refresh();
            }
            AppEvent::ProfileFailed(msg) => {
                self.profile_error = Some(msg);
                self.refresh();
            }
        }
    }

    fn ui_message(&mut self, msg: UiMsg, control_flow: &mut ControlFlow) {
        match msg {
            UiMsg::Ready => {
                if let Some(p) = self.popup.as_mut() {
                    p.ready = true;
                }
                self.refresh();
            }
            UiMsg::Profile { id } => self.set_profile(id),
            UiMsg::PowerSettings => {
                self.hide_popup("opening power settings");
                desktop::open_power_settings();
            }
            UiMsg::Autostart { on } => {
                if let Err(e) = desktop::set_autostart(on) {
                    log::warn!("updating start at login: {e:#}");
                }
                self.refresh();
            }
            UiMsg::Quit => *control_flow = ControlFlow::Exit,
            UiMsg::Size { height } => {
                if let Some(p) = &self.popup {
                    p.set_height(height);
                    if let (true, Some(anchor)) = (p.is_visible(), self.popup_anchor) {
                        p.place_near(anchor);
                    }
                }
            }
            UiMsg::Hide => self.hide_popup("requested by the page"),
        }
    }

    fn set_profile(&mut self, id: String) {
        let Some(profiles) = self.snapshot.profiles.as_mut() else { return };
        if profiles.active == id {
            return;
        }
        // Optimistic; the daemon's PropertiesChanged confirms it (or the snapshot puts
        // the old one back).
        profiles.active = id.clone();
        self.profile_error = None;
        self.refresh();
        let proxy = self.proxy.clone();
        thread::spawn(move || {
            if let Err(e) = power::set_profile(&id) {
                log::warn!("switching to {id}: {e:#}");
                let _ = proxy.send_event(AppEvent::ProfileFailed(format!("Could not switch to {}: {e}", profile_label(&id))));
            }
        });
        if let Some(m) = &self.monitor {
            m.refresh();
        }
    }

    fn toggle_popup(&mut self, anchor: PhysicalPosition<i32>) {
        let Some(popup) = &self.popup else { return };
        if popup.is_visible() {
            self.hide_popup("tray click");
            return;
        }
        if self.popup_hidden_at.is_some_and(|t| t.elapsed() < REOPEN_GUARD) {
            return;
        }
        self.popup_anchor = Some(anchor);
        // Fresh "updated … ago" texts, then the latest readings a moment later.
        self.refresh();
        let popup = self.popup.as_ref().unwrap();
        popup.place_near(anchor);
        popup.show();
        if let Some(m) = &self.monitor {
            m.set_fast(true);
            m.refresh();
        }
    }

    fn hide_popup(&mut self, why: &str) {
        if let Some(p) = &self.popup
            && p.is_visible()
        {
            log::debug!("hiding popup: {why}");
            p.call("closeMenu", &());
            p.hide();
            self.popup_hidden_at = Some(Instant::now());
            if let Some(m) = &self.monitor {
                m.set_fast(false);
            }
        }
    }

    /// Pushes the current state to the tray and the popup.
    fn refresh(&mut self) {
        let s = &self.snapshot;
        let now = unix_now();
        let warning = s.devices.iter().any(Device::low) || s.main.as_ref().is_some_and(Device::low);
        // The icon shows what powers the computer; without a battery or UPS, the lowest
        // peripheral.
        let level = s.main.as_ref().or_else(|| lowest(&s.devices)).map(|d| d.percentage);
        if let Some(t) = self.tray.as_mut() {
            t.update(TrayState { look: Look::new(level, warning), tooltip: tooltip(s) });
        }

        let Some(popup) = &self.popup else { return };
        let supplies = s.devices.iter().filter(|d| d.power_supply).count();
        let main = s.main.as_ref().map(|m| {
            // UPower's summary has no name; borrow it from the single battery or UPS.
            let only = s.devices.iter().find(|d| d.power_supply).filter(|_| supplies == 1);
            let name = only.map(Device::name).unwrap_or_else(|| m.kind.label().to_owned());
            MainView { kind: m.kind.key(), name, percent: m.percentage.round() as u32, low: m.low(), detail: main_detail(m) }
        });
        let devices = s
            .devices
            .iter()
            // With a single battery or UPS, the header already shows it.
            .filter(|d| !(d.power_supply && supplies == 1 && main.is_some()))
            .map(|d| DeviceView {
                kind: d.kind.key(),
                kind_label: d.kind.label(),
                name: d.name(),
                percent: d.percentage.round() as u32,
                low: d.low(),
                detail: device_detail(d, now),
            })
            .collect();
        let mut profile_notes = Vec::new();
        if let Some(p) = &s.profiles {
            if !p.degraded.is_empty() {
                profile_notes.push(format!("Performance is limited ({})", p.degraded.replace('-', " ")));
            }
            profile_notes.extend(p.holds.iter().cloned());
        }
        profile_notes.extend(self.profile_error.iter().cloned());
        let state = StateView {
            main,
            devices,
            profiles: s.profiles.as_ref().map(|p| {
                p.available.iter().map(|id| ProfileView { id: id.clone(), label: profile_label(id) }).collect()
            }),
            profile: s.profiles.as_ref().map(|p| p.active.clone()).unwrap_or_default(),
            profile_notes,
            inhibitors: s.inhibitors.iter().map(inhibitor_view).collect(),
            delays: (!s.delays.is_empty()).then(|| {
                let n = s.delays.len();
                let text = format!("{n} program{} briefly delay sleep to clean up", if n == 1 { "" } else { "s" });
                let list = s.delays.iter().map(|d| format!("{}: {}", d.who, d.why)).collect::<Vec<_>>().join("\n");
                (text, list)
            }),
            autostart: desktop::autostart(),
        };
        popup.call("render", &state);
    }
}

fn lowest(devices: &[Device]) -> Option<&Device> {
    devices.iter().min_by(|a, b| a.percentage.total_cmp(&b.percentage))
}

fn profile_label(id: &str) -> &'static str {
    match id {
        "performance" => "Performance",
        "balanced" => "Balanced",
        "power-saver" => "Power saver",
        _ => "Other",
    }
}

fn tooltip(s: &Snapshot) -> String {
    let mut lines = Vec::new();
    if let Some(m) = &s.main {
        lines.push(format!("{}: {}%", m.kind.label(), m.percentage.round()));
    }
    for d in s.devices.iter().filter(|d| !d.power_supply) {
        lines.push(format!("{}{}: {}%", if d.low() { "⚠ " } else { "" }, d.name(), d.percentage.round()));
    }
    if lines.is_empty() {
        lines.push("No batteries".to_owned());
    }
    lines.join("\n")
}

fn main_detail(m: &Device) -> String {
    let mut parts = vec![m.state.label().to_owned()];
    use power::State;
    match m.state {
        State::Discharging if m.time_to_empty > 0 => parts.push(format!("{} left", duration(m.time_to_empty))),
        State::Charging if m.time_to_full > 0 => parts.push(format!("full in {}", duration(m.time_to_full))),
        // A UPS on mains power still knows how long it would last.
        _ if m.kind == power::Kind::Ups && m.time_to_empty > 0 => parts.push(format!("{} on battery", duration(m.time_to_empty))),
        _ => {}
    }
    parts.retain(|p| !p.is_empty());
    parts.join(" · ")
}

fn device_detail(d: &Device, now: u64) -> String {
    let mut parts = vec![d.kind.label().to_owned()];
    if matches!(d.state, power::State::Charging) {
        parts.push("charging".to_owned());
    }
    // Peripherals report on their own schedule (or not at all while switched off); say
    // when the reading is old.
    if d.updated > 0 && now.saturating_sub(d.updated) >= 15 * 60 {
        parts.push(format!("updated {} ago", duration((now - d.updated) as i64)));
    }
    parts.join(" · ")
}

fn inhibitor_view(i: &Inhibitor) -> InhibitorView {
    let what = match (i.blocks_sleep, i.blocks_idle) {
        (true, true) => "Sleep, screensaver",
        (true, false) => "Sleep",
        _ => "Screensaver",
    };
    InhibitorView { who: i.who.clone(), why: i.why.clone(), what: what.to_owned() }
}

/// "45 min", "2 h 5 min", "3 days".
fn duration(secs: i64) -> String {
    let mins = (secs + 30) / 60;
    match mins {
        ..1 => "under a minute".to_owned(),
        1..60 => format!("{mins} min"),
        60..1440 if mins % 60 == 0 => format!("{} h", mins / 60),
        60..1440 => format!("{} h {} min", mins / 60, mins % 60),
        _ => {
            let days = mins / 1440;
            format!("{days} day{}", if days == 1 { "" } else { "s" })
        }
    }
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

/// Hold an exclusive lock for the life of the process; fails if the applet already runs.
fn lock_instance() -> Result<File, String> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/tmp/power-applet-{}", unsafe { libc::getuid() })));
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join("power-applet.lock");
    let mut f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&p)
        .map_err(|e| format!("{}: {e}", p.display()))?;
    // SAFETY: flock on a descriptor we own; no memory is involved.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let mut pid = String::new();
        let _ = f.read_to_string(&mut pid);
        return Err(format!("power-applet is already running (pid {})", pid.trim()));
    }
    let _ = f.set_len(0);
    let _ = write!(f, "{}", std::process::id());
    Ok(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(10), "under a minute");
        assert_eq!(duration(29 * 60 + 48), "30 min");
        assert_eq!(duration(3600), "1 h");
        assert_eq!(duration(3 * 3600 + 5 * 60), "3 h 5 min");
        assert_eq!(duration(2 * 86400), "2 days");
    }
}
