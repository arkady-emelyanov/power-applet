//! The tray icon: a battery filled to the main power source's level, with an amber
//! warning badge in the corner while any device is low. Charging isn't shown.
//!
//! The same geometry (16x16 units) is written out as symbolic SVGs, which panels size and
//! tint like their other status icons, and rasterised into pixmaps as a fallback.

use std::path::{Path, PathBuf};

/// Charge levels get rounded to this step, which keeps the number of icon files small.
const STEP: u8 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Look {
    /// Rounded charge in percent; `None` without any battery.
    pub level: Option<u8>,
    pub warning: bool,
}

impl Look {
    pub fn new(percentage: Option<f64>, warning: bool) -> Self {
        let level = percentage.map(|p| ((p.clamp(0.0, 100.0) / STEP as f64).round() as u8) * STEP);
        Self { level, warning }
    }

    pub fn name(self) -> String {
        let level = match self.level {
            Some(l) => format!("{l:03}"),
            None => "none".to_owned(),
        };
        format!("power-applet-{level}{}-symbolic", if self.warning { "-warning" } else { "" })
    }

    fn all() -> impl Iterator<Item = Look> {
        let levels = (0..=100 / STEP).map(|i| Some(i * STEP)).chain([None]);
        levels.flat_map(|level| [false, true].map(|warning| Look { level, warning }))
    }
}

// Geometry, in 16x16 units, the same as Cinnamon's own battery icons
// (xsi-battery-level-*): a rounded body with a 2-unit wall and the terminal on top, a
// charge bar inside, and for warnings a rounded badge with a "!" in the bottom right
// corner, with the body cut away around it.

/// Body and terminal, outer edge then the hole, as drawn by Cinnamon's icons.
const BODY_PATH: &str = "m 7 0 c -1 0 -1 1 -1 1 v 1 h -1 s -0.707031 -0.015625 -1.449219 0.355469 \
c -0.738281 0.371093 -1.550781 1.3125 -1.550781 2.644531 v 8 s -0.015625 0.707031 0.355469 1.445312 \
c 0.371093 0.742188 1.3125 1.554688 2.644531 1.554688 h 6 s 0.707031 0.015625 1.449219 -0.355469 \
c 0.738281 -0.371093 1.550781 -1.3125 1.550781 -2.644531 v -8 c 0 -1.332031 -0.8125 -2.273438 -1.550781 -2.644531 \
c -0.742188 -0.371094 -1.449219 -0.355469 -1.449219 -0.355469 h -1 v -1 c 0 -1 -1 -1 -1 -1 z \
m 4 4 c 0.554688 0 1 0.445312 1 1 v 8 c 0 0.554688 -0.445312 1 -1 1 h -6 c -0.554688 0 -1 -0.445312 -1 -1 v -8 \
c 0 -0.554688 0.445312 -1 1 -1 z";
/// The warning badge with its "!" cut out.
const BADGE_PATH: &str = "m 9.5 8 c -0.832031 0 -1.5 0.667969 -1.5 1.5 v 5 c 0 0.832031 0.667969 1.5 1.5 1.5 h 5 \
c 0.832031 0 1.5 -0.667969 1.5 -1.5 v -5 c 0 -0.832031 -0.667969 -1.5 -1.5 -1.5 z \
m 1.5 1 h 2 v 2.5 s 0 0.5 -0.5 0.5 h -1 c -0.5 0 -0.5 -0.5 -0.5 -0.5 z \
m 0.5 4 h 1 c 0.277344 0 0.5 0.222656 0.5 0.5 v 1 c 0 0.277344 -0.222656 0.5 -0.5 0.5 h -1 \
c -0.277344 0 -0.5 -0.222656 -0.5 -0.5 v -1 c 0 -0.277344 0.222656 -0.5 0.5 -0.5 z";

// The same shapes for the pixmaps, as (rounded) rectangles.
const OUTER: Rect = Rect { x0: 2.0, y0: 2.0, x1: 14.0, y1: 16.0, r: 3.0 };
const INNER: Rect = Rect { x0: 4.0, y0: 4.0, x1: 12.0, y1: 14.0, r: 1.0 };
const NUB: Rect = Rect { x0: 6.0, y0: 0.0, x1: 10.0, y1: 3.0, r: 1.0 };
/// The charge bar at 100%; it drains from the top.
const FILL: Rect = Rect { x0: 5.0, y0: 5.0, x1: 11.0, y1: 13.0, r: 0.0 };
/// Where the body is cut away for the badge (a unit of space around it).
const CUT: Rect = Rect { x0: 7.0, y0: 7.0, x1: 16.0, y1: 16.0, r: 0.0 };
const BADGE: Rect = Rect { x0: 8.0, y0: 8.0, x1: 16.0, y1: 16.0, r: 1.5 };
const BANG: Rect = Rect { x0: 11.0, y0: 9.0, x1: 13.0, y1: 12.0, r: 0.5 };
const DOT: Rect = Rect { x0: 11.0, y0: 13.0, x1: 13.0, y1: 15.0, r: 0.5 };

#[derive(Clone, Copy)]
struct Rect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    /// Corner radius.
    r: f32,
}

impl Rect {
    fn contains(self, x: f32, y: f32) -> bool {
        if !(x >= self.x0 && x < self.x1 && y >= self.y0 && y < self.y1) {
            return false;
        }
        // Distance into a corner's square, measured from the corner circle's centre.
        let dx = (self.x0 + self.r - x).max(x - (self.x1 - self.r)).max(0.0);
        let dy = (self.y0 + self.r - y).max(y - (self.y1 - self.r)).max(0.0);
        dx * dx + dy * dy <= self.r * self.r
    }

    fn svg_points(self) -> String {
        format!("{0},{1} {2},{1} {2},{3} {0},{3} {0},{1}", self.x0, self.y0, self.x1, self.y1)
    }

    fn svg_rect(self) -> String {
        format!("<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"/>", self.x0, self.y0, self.x1 - self.x0, self.y1 - self.y0)
    }
}

fn fill_rect(level: u8) -> Option<Rect> {
    let h = (FILL.y1 - FILL.y0) * level as f32 / 100.0;
    (level > 0).then_some(Rect { y0: FILL.y1 - h.max(1.0), ..FILL })
}

/// A symbolic SVG: `#bebebe` and the `warning` class are what panels recolour. GTK
/// repaints the fill of every rect, path, ellipse, circle and polygon, so the mask for the
/// cut-out is drawn with a (closed) polyline, which it leaves alone.
pub fn svg(look: Look) -> String {
    let mut battery = format!("<path d=\"{BODY_PATH}\"/>");
    if let Some(r) = look.level.and_then(fill_rect) {
        battery += &r.svg_rect();
    }
    let opacity = if look.level.is_none() { " opacity=\"0.45\"" } else { "" };
    let body = if look.warning {
        format!(
            "<mask id=\"cut\" maskUnits=\"userSpaceOnUse\" x=\"0\" y=\"0\" width=\"16\" height=\"16\">\
<polyline points=\"{full}\" fill=\"#fff\"/><polyline points=\"{cut}\" fill=\"#000\"/></mask>\
<g fill=\"#bebebe\"{opacity} mask=\"url(#cut)\">{battery}</g>\
<path d=\"{BADGE_PATH}\" fill=\"#f57900\" class=\"warning\"/>",
            full = Rect { x0: 0.0, y0: 0.0, x1: 16.0, y1: 16.0, r: 0.0 }.svg_points(),
            cut = CUT.svg_points(),
        )
    } else {
        format!("<g fill=\"#bebebe\"{opacity}>{battery}</g>")
    };
    format!("<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"16\" height=\"16\" viewBox=\"0 0 16 16\">{body}</svg>\n")
}

/// What covers a point: 0 nothing, 1 the battery, 2 the warning badge.
fn sample(look: Look, x: f32, y: f32) -> u8 {
    if look.warning {
        if BADGE.contains(x, y) && !BANG.contains(x, y) && !DOT.contains(x, y) {
            return 2;
        }
        if CUT.contains(x, y) {
            return 0;
        }
    }
    let body = (OUTER.contains(x, y) || NUB.contains(x, y)) && !INNER.contains(x, y);
    let fill = look.level.and_then(fill_rect).is_some_and(|r| r.contains(x, y));
    u8::from(body || fill)
}

const FG: [u8; 3] = [0xe8, 0xe8, 0xe8];
const AMBER: [u8; 3] = [0xf0, 0x9a, 0x2a];

/// The icon as ARGB32 pixels (network byte order), 4x4 supersampled.
pub fn pixmap(look: Look, size: i32) -> ksni::Icon {
    const SS: i32 = 4;
    let alpha = if look.level.is_none() { 0.45 } else { 1.0 };
    let mut data = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let (mut battery, mut warn) = (0, 0);
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = ((px * SS + sx) as f32 + 0.5) / (size * SS) as f32 * 16.0;
                    let y = ((py * SS + sy) as f32 + 0.5) / (size * SS) as f32 * 16.0;
                    match sample(look, x, y) {
                        1 => battery += 1,
                        2 => warn += 1,
                        _ => {}
                    }
                }
            }
            let n = (SS * SS) as f32;
            let (rgb, cover) = if warn > 0 { (AMBER, (warn + battery) as f32 / n) } else { (FG, battery as f32 / n * alpha) };
            data.extend_from_slice(&[(cover * 255.0).round() as u8, rgb[0], rgb[1], rgb[2]]);
        }
    }
    ksni::Icon { width: size, height: size, data }
}

pub fn pixmaps(look: Look) -> Vec<ksni::Icon> {
    [16, 22, 24, 32, 48, 64].iter().map(|s| pixmap(look, *s)).collect()
}

fn icon_dir() -> PathBuf {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| crate::home().join(".local/share"));
    data.join("icons/hicolor")
}

/// Write the symbolic icons into the user's icon theme (only when they changed) and
/// refresh its cache, so panels can find them by name. Returns whether anything changed.
pub fn install() -> std::io::Result<bool> {
    let root = icon_dir();
    let dir = root.join("scalable/status");
    std::fs::create_dir_all(&dir)?;
    let mut changed = false;
    for look in Look::all() {
        let path = dir.join(format!("{}.svg", look.name()));
        let svg = svg(look);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(svg.as_str()) {
            std::fs::write(&path, svg)?;
            changed = true;
        }
    }
    if changed {
        refresh_icon_cache(&root);
    }
    Ok(changed)
}

/// A stale icon-theme.cache would hide new icons: rebuild it if one exists.
fn refresh_icon_cache(root: &Path) {
    if root.join("icon-theme.cache").exists() {
        let ok = std::process::Command::new("gtk-update-icon-cache")
            .args(["-q", "-f", "-t"])
            .arg(root)
            .status()
            .is_ok_and(|s| s.success());
        if !ok {
            // No tool: drop the cache so the theme directory is scanned instead.
            let _ = std::fs::remove_file(root.join("icon-theme.cache"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding() {
        assert_eq!(Look::new(Some(6.0), true).level, Some(10));
        assert_eq!(Look::new(Some(4.9), true).level, Some(0));
        assert_eq!(Look::new(Some(100.0), false).name(), "power-applet-100-symbolic");
        assert_eq!(Look::new(None, true).name(), "power-applet-none-warning-symbolic");
        assert_eq!(Look::all().count(), 24);
    }

    #[test]
    fn badge_is_amber_and_clear_of_the_battery() {
        let look = Look { level: Some(100), warning: true };
        assert_eq!(sample(look, 9.0, 12.5), 2);
        assert_eq!(sample(look, 12.0, 14.0), 0, "the dot of the !");
        assert_eq!(sample(look, 7.5, 12.0), 0, "the gap around the badge");
        assert_eq!(sample(look, 3.0, 8.0), 1);
        assert_eq!(sample(Look { warning: false, ..look }, 7.5, 6.0), 1);
        assert_eq!(sample(Look { level: Some(50), warning: false }, 7.5, 6.0), 0, "drained from the top");
    }

    /// Writes all icons as SVG to `ICON_DUMP_DIR`, to look at them.
    #[test]
    #[ignore]
    fn dump_svgs() {
        let dir = std::env::var("ICON_DUMP_DIR").unwrap();
        for look in Look::all() {
            std::fs::write(format!("{dir}/{}.svg", look.name()), svg(look)).unwrap();
        }
    }

    /// Writes the pixmaps as raw ARGB to `ICON_DUMP_DIR` (`convert -size 64x64 -depth 8 rgba:…`
    /// after swapping channels).
    #[test]
    #[ignore]
    fn dump_pixmaps() {
        let dir = std::env::var("ICON_DUMP_DIR").unwrap();
        for look in [Look { level: Some(100), warning: false }, Look { level: Some(100), warning: true }, Look { level: Some(10), warning: true }] {
            for size in [22, 64] {
                let icon = pixmap(look, size);
                let rgba: Vec<u8> = icon.data.chunks(4).flat_map(|p| [p[1], p[2], p[3], p[0]]).collect();
                std::fs::write(format!("{dir}/{}-{size}.rgba", look.name()), rgba).unwrap();
            }
        }
    }
}
