//! Tray icon: StatusNotifierItem over D-Bus via ksni. A left click opens the popup (see
//! `ui.rs`); a right click shows a small native menu.

use std::time::Duration;

use anyhow::{Context, Result};
use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem};
use tao::dpi::PhysicalPosition;
use tao::event_loop::EventLoopProxy;

use crate::AppEvent;
use crate::icon::{self, Look};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    PowerSettings,
    Quit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TrayState {
    pub look: Look,
    pub tooltip: String,
}

impl Default for TrayState {
    fn default() -> Self {
        Self { look: Look { level: None, warning: false }, tooltip: crate::APP_NAME.into() }
    }
}

struct Sni {
    proxy: EventLoopProxy<AppEvent>,
    state: TrayState,
    /// Whether to offer the named symbolic icon (false: pixmap only, see `Tray::new`).
    named_icon: bool,
}

impl Sni {
    fn send(&self, event: AppEvent) {
        let _ = self.proxy.send_event(event);
    }

    fn item(&self, label: &str, action: TrayAction) -> MenuItem<Self> {
        StandardItem {
            label: label.into(),
            activate: Box::new(move |t: &mut Self| t.send(AppEvent::TrayAction(action))),
            ..Default::default()
        }
        .into()
    }
}

impl ksni::Tray for Sni {
    fn id(&self) -> String {
        "power-applet".into()
    }

    fn title(&self) -> String {
        crate::APP_NAME.into()
    }

    fn category(&self) -> ksni::Category {
        ksni::Category::Hardware
    }

    /// A symbolic icon by name: panels size it like their other symbolic icons and tint
    /// it with the panel's text colour. The pixmap below is the fallback.
    fn icon_name(&self) -> String {
        if self.named_icon { self.state.look.name() } else { String::new() }
    }

    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        icon::pixmaps(self.state.look)
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip { title: self.state.tooltip.clone(), ..Default::default() }
    }

    fn activate(&mut self, x: i32, y: i32) {
        self.send(AppEvent::TrayClick(PhysicalPosition::new(x, y)));
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            self.item("Power settings…", TrayAction::PowerSettings),
            MenuItem::Separator,
            self.item("Quit", TrayAction::Quit),
        ]
    }
}

pub struct Tray {
    handle: Handle<Sni>,
    state: TrayState,
}

impl Tray {
    pub fn new(proxy: EventLoopProxy<AppEvent>) -> Result<Self> {
        let fresh = icon::install().unwrap_or_else(|e| {
            log::warn!("installing tray icons: {e}");
            false
        });
        // Panels notice new icon files only after their next periodic rescan; until then
        // show the pixmap, then switch to the named icon so the panel looks it up again.
        let sni = Sni { proxy, state: TrayState::default(), named_icon: !fresh };
        let handle = sni.spawn().context("registering StatusNotifierItem (is a tray host running?)")?;
        if fresh {
            let h = handle.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(8));
                h.update(|t| t.named_icon = true);
            });
        }
        Ok(Self { handle, state: TrayState::default() })
    }

    pub fn update(&mut self, state: TrayState) {
        if state == self.state {
            return;
        }
        self.state = state.clone();
        self.handle.update(move |t| t.state = state);
    }
}
