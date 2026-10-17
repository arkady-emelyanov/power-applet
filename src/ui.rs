//! The tray popup: a bundled HTML page in a webview. It talks to the app through wry's
//! IPC bridge (`window.ipc.postMessage(json)`) and is updated by calling `window.app.*`.

use std::cell::Cell;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tao::dpi::{LogicalSize, PhysicalPosition};
use tao::event_loop::{EventLoopProxy, EventLoopWindowTarget};
use tao::window::{Window, WindowBuilder, WindowId};
use wry::{WebView, WebViewBuilder};

use crate::AppEvent;

const POPUP_HTML: &str = include_str!("ui/popup.html");

pub const POPUP_WIDTH: f64 = 360.0;
/// Gap between the tray icon and the popup.
const POPUP_MARGIN: i32 = 6;

/// Messages sent by the page.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum UiMsg {
    Ready,
    Profile { id: String },
    PowerSettings,
    Autostart { on: bool },
    Quit,
    Size { height: f64 },
    Hide,
}

/// Everything the page shows, pushed on every change.
#[derive(Debug, Clone, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StateView {
    pub main: Option<MainView>,
    pub devices: Vec<DeviceView>,
    /// `None` without power-profiles-daemon.
    pub profiles: Option<Vec<ProfileView>>,
    pub profile: String,
    /// Degraded performance, holds and errors from switching.
    pub profile_notes: Vec<String>,
    pub inhibitors: Vec<InhibitorView>,
    /// "3 programs…", with the list for a tooltip.
    pub delays: Option<(String, String)>,
    pub autostart: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MainView {
    pub kind: &'static str,
    pub name: String,
    pub percent: u32,
    pub low: bool,
    /// e.g. "Fully charged · 29 min on battery"
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceView {
    pub kind: &'static str,
    pub kind_label: &'static str,
    pub name: String,
    pub percent: u32,
    pub low: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileView {
    pub id: String,
    pub label: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct InhibitorView {
    pub who: String,
    pub why: String,
    /// "Sleep", "Screensaver" or "Sleep, screensaver".
    pub what: String,
}

pub struct View {
    // The webview must be dropped before its window.
    webview: WebView,
    window: Window,
    pub ready: bool,
    /// Requested logical height; the window's reported size lags behind resizes.
    height: Cell<f64>,
}

impl View {
    pub fn popup(target: &EventLoopWindowTarget<AppEvent>, proxy: EventLoopProxy<AppEvent>) -> Result<Self> {
        use tao::platform::unix::WindowBuilderExtUnix;
        let window = WindowBuilder::new()
            .with_title(crate::APP_NAME)
            .with_inner_size(LogicalSize::new(POPUP_WIDTH, 360.0))
            .with_decorations(false)
            .with_always_on_top(true)
            .with_visible(false)
            .with_skip_taskbar(true)
            .build(target)
            .context("creating popup window")?;
        menu_like::setup(&window, proxy.clone());
        let builder = WebViewBuilder::new().with_html(POPUP_HTML).with_ipc_handler(move |req| {
            match serde_json::from_str::<UiMsg>(req.body()) {
                Ok(msg) => {
                    let _ = proxy.send_event(AppEvent::Ui(msg));
                }
                Err(e) => log::warn!("bad message from the popup: {e}: {}", req.body()),
            }
        });
        let webview = {
            use tao::platform::unix::WindowExtUnix;
            use wry::WebViewBuilderExtUnix;
            builder.build_gtk(window.default_vbox().context("window has no GTK container")?)?
        };
        let height = Cell::new(window.inner_size().to_logical(window.scale_factor()).height);
        Ok(Self { webview, window, ready: false, height })
    }

    pub fn id(&self) -> WindowId {
        self.window.id()
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn show(&self) {
        self.window.set_visible(true);
        self.window.set_focus();
        let _ = self.webview.focus();
        menu_like::grab(&self.window);
    }

    pub fn hide(&self) {
        menu_like::ungrab();
        self.window.set_visible(false);
    }

    pub fn call(&self, function: &str, arg: &impl Serialize) {
        if !self.ready {
            return;
        }
        let arg = serde_json::to_string(arg).unwrap_or_else(|_| "null".into());
        let js = format!("window.app && window.app.{function}({arg});");
        if let Err(e) = self.webview.evaluate_script(&js) {
            log::warn!("updating the popup: {e}");
        }
    }

    pub fn set_height(&self, height: f64) {
        self.height.set(height.ceil());
        self.window.set_inner_size(LogicalSize::new(POPUP_WIDTH, height.ceil()));
    }

    /// Places the popup next to a tray click at `anchor` (physical screen coordinates),
    /// opening towards the centre of the screen and staying on the monitor.
    pub fn place_near(&self, anchor: PhysicalPosition<i32>) {
        let scale = self.window.scale_factor();
        let (w, h) = ((POPUP_WIDTH * scale).round() as i32, (self.height.get() * scale).round() as i32);
        let monitor = self
            .window
            .available_monitors()
            .find(|m| {
                let (p, s) = (m.position(), m.size());
                anchor.x >= p.x && anchor.x < p.x + s.width as i32 && anchor.y >= p.y && anchor.y < p.y + s.height as i32
            })
            .or_else(|| self.window.primary_monitor());
        let Some(monitor) = monitor else {
            self.window.set_outer_position(anchor);
            return;
        };
        let (mp, ms) = (monitor.position(), monitor.size());
        let (mw, mh) = (ms.width as i32, ms.height as i32);
        let below = anchor.y < mp.y + mh / 2;
        let mut x = anchor.x - w / 2;
        let mut y = if below { anchor.y + POPUP_MARGIN } else { anchor.y - h - POPUP_MARGIN };
        // Panels usually sit at the very edge; keep clear of them.
        let edge = (32.0 * monitor.scale_factor()) as i32;
        x = x.clamp(mp.x + POPUP_MARGIN, (mp.x + mw - w - POPUP_MARGIN).max(mp.x));
        y = y.clamp(mp.y + if below { edge } else { 0 }, (mp.y + mh - h - if below { 0 } else { edge }).max(mp.y));
        self.window.set_outer_position(PhysicalPosition::new(x, y));
    }
}

/// Makes the popup behave like a native menu on X11: the window manager neither places
/// nor manages it (override-redirect, so it opens exactly at the tray icon), and while
/// open it holds a pointer grab so that any click outside closes it. The keyboard is left
/// alone: an active keyboard grab would swallow global shortcuts such as media keys.
mod menu_like {
    use gtk::gdk::{self, prelude::*};
    use gtk::glib;
    use gtk::prelude::*;
    use tao::event_loop::EventLoopProxy;
    use tao::platform::unix::WindowExtUnix;
    use tao::window::Window;

    use super::UiMsg;
    use crate::AppEvent;

    /// The panel may still hold its own grab right after the click; retry briefly.
    const GRAB_ATTEMPTS: u32 = 20;

    pub fn setup(window: &Window, proxy: EventLoopProxy<AppEvent>) {
        let gw = window.gtk_window();
        gw.set_type_hint(gdk::WindowTypeHint::PopupMenu);
        gw.set_skip_pager_hint(true);
        gw.realize();
        if let Some(gdk_window) = gw.window() {
            gdk_window.set_override_redirect(true);
        }
        gw.add_events(gdk::EventMask::BUTTON_PRESS_MASK);

        let hide = move || {
            let _ = proxy.send_event(AppEvent::Ui(UiMsg::Hide));
        };
        let on_press = hide.clone();
        // With owner_events, clicks inside go to the webview as usual; clicks anywhere
        // else are reported to the toplevel with coordinates outside of it.
        gw.connect_button_press_event(move |w, ev| {
            let (x, y) = ev.position();
            let (width, height) = (w.allocated_width() as f64, w.allocated_height() as f64);
            if x < 0.0 || y < 0.0 || x >= width || y >= height {
                log::debug!("click outside popup at {x},{y}");
                on_press();
            }
            glib::Propagation::Proceed
        });
        gw.connect_grab_broken_event(move |_, _| {
            log::debug!("popup grab broken");
            hide();
            glib::Propagation::Proceed
        });
    }

    pub fn grab(window: &Window) {
        let gw = window.gtk_window().clone();
        let mut attempts = 0;
        glib::timeout_add_local(std::time::Duration::from_millis(25), move || {
            attempts += 1;
            let (Some(gdk_window), Some(seat)) = (gw.window(), gdk::Display::default().and_then(|d| d.default_seat()))
            else {
                return glib::ControlFlow::Break;
            };
            if !gw.is_visible() {
                return glib::ControlFlow::Break;
            }
            let status = seat.grab(&gdk_window, gdk::SeatCapabilities::ALL_POINTING, true, None, None, None);
            if status == gdk::GrabStatus::Success {
                log::debug!("popup grabbed the pointer after {attempts} attempt(s)");
                return glib::ControlFlow::Break;
            }
            if attempts >= GRAB_ATTEMPTS {
                log::warn!("could not grab the pointer for the popup: {status:?}");
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    pub fn ungrab() {
        if let Some(seat) = gdk::Display::default().and_then(|d| d.default_seat()) {
            seat.ungrab();
        }
    }
}
