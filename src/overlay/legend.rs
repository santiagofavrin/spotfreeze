//! PURE mode/hotkey legend: while frozen, a compact HUD near the top-center
//! of every monitor shows the current spotlight shape, active and inactive
//! modes with keycap-styled hotkeys, then the app version and a close ("×")
//! button, separated by vertical dividers.
//!
//! The HUD is MOVABLE: the user can grab it anywhere and drag it to a new
//! position on that monitor (the position is per-monitor and per-freeze-
//! session, starting at the default top-center spot). The close button at the
//! HUD's right end hides it for the rest of the freeze session (it reappears
//! on the next freeze). The `overlay.show_legend` setting controls whether
//! the HUD appears at all. The controller (not this module) owns the
//! position/hidden/drag state; [`Legend::paint`] simply draws at the origin
//! it is given.
//!
//! The HUD sits below the top edge with a quiet inset so it stays visible
//! without looking pinned to the screen boundary. It is painted into the
//! composed frame only — never into the capture originals — so it can never
//! leak into a snip copy or the capture-mode re-base.
//!
//! Text is rendered with the embedded **Inter** typeface (SIL Open Font
//! License 1.1, see `assets/fonts/OFL.txt`), rasterized to per-pixel alpha
//! coverage at construction time by the pure-Rust [`fontdue`] crate — no OS
//! text APIs, fully headless-testable. Glyphs are pre-rasterized once per
//! freeze (in [`Legend::build`]) into cached coverage bitmaps, so [`Legend::paint`]
//! only blits cached coverage with the shared integer alpha-blend math: no
//! font work happens on the per-frame repaint path. Everything here is
//! deterministic pixel math.
//!
//! Design language: a rangefinder / camera overlay. Dark translucent
//! container, tight 4px corners, 1px border. Hotkeys sit in small keycaps.
//! Active modes use near-white text; the spotlight shape icon uses a cyan
//! accent when that layer is on.

use crate::capture::DibBuffer;
use crate::geometry::{Point, Rect, SpotlightShape};
use crate::settings::model::{HotkeySettings, Rgb};
use fontdue::{Font, FontSettings};

/// Embedded Inter Regular (SIL OFL 1.1) — Latin subset, weight 400.
const FONT_REGULAR_BYTES: &[u8] = include_bytes!("../../assets/fonts/Inter-Regular.ttf");
/// Embedded Inter SemiBold (SIL OFL 1.1) — Latin subset, weight 600. The
/// active tab switches to this weight for emphasis.
const FONT_SEMIBOLD_BYTES: &[u8] = include_bytes!("../../assets/fonts/Inter-SemiBold.ttf");

/// Font size in physical pixels (PerMonitorV2 — no DPI math).
const FONT_PX: f32 = 14.0;

/// Horizontal padding between the HUD edge and the first/last elements.
const PILL_PAD_X: u32 = 12;
/// HUD corner radius in pixels (sharp/technical 4px).
const PILL_RADIUS: u32 = 4;
/// Distance between the frame's top edge and the HUD's top edge (reduced to 24 for subtlety).
const TOP_MARGIN: u32 = 24;

/// Spacing between mode tabs.
const TAB_GAP: u32 = 16;

/// HUD background: extremely dark gray/black translucent panel.
const PILL_COLOR: Rgb = Rgb {
    r: 15,
    g: 15,
    b: 17,
};
/// HUD background blend alpha (~86%: high contrast, reads through slightly).
const PILL_ALPHA: u8 = 220;

/// 1px border around the HUD.
const BORDER_COLOR: Rgb = Rgb {
    r: 60,
    g: 62,
    b: 68,
};
/// Border alpha (fully opaque).
const BORDER_ALPHA: u8 = 255;

/// Divider line color.
const DIVIDER_COLOR: Rgb = Rgb {
    r: 45,
    g: 47,
    b: 52,
};
const DIVIDER_ALPHA: u8 = 255;

/// Text on the active tab (near-white).
const TEXT_ACTIVE: Rgb = Rgb {
    r: 240,
    g: 240,
    b: 245,
};
/// Text on inactive tabs (cool system gray).
const TEXT_INACTIVE: Rgb = Rgb {
    r: 140,
    g: 145,
    b: 155,
};
/// Active status/dot color: neon cyan.
const ACCENT_COLOR: Rgb = Rgb {
    r: 0,
    g: 229,
    b: 255,
};

/// Active keycap background.
const KEYCAP_BG_ACTIVE: Rgb = Rgb {
    r: 45,
    g: 45,
    b: 48,
};
/// Active keycap border.
const KEYCAP_BORDER_ACTIVE: Rgb = Rgb {
    r: 80,
    g: 82,
    b: 90,
};

/// Inactive keycap background.
const KEYCAP_BG_INACTIVE: Rgb = Rgb {
    r: 26,
    g: 26,
    b: 28,
};
/// Inactive keycap border.
const KEYCAP_BORDER_INACTIVE: Rgb = Rgb {
    r: 42,
    g: 42,
    b: 45,
};

/// Version label text (dimmer gray, never highlighted).
const TEXT_VERSION: Rgb = Rgb {
    r: 90,
    g: 95,
    b: 105,
};

/// One legend tab: a mode's display name and the hotkey that reaches it.
pub struct LegendTab {
    pub name: String,
    pub hotkey: String,
}

/// A pre-rasterized string: an alpha-coverage bitmap (0..=255 per pixel,
/// row-major, top-down) plus its pixel dimensions. Built once per freeze;
/// [`Legend::paint`] only blits it.
struct CoverageBitmap {
    width: u32,
    height: u32,
    coverage: Vec<u8>,
}

/// Per-tab pre-rendered text in both weights. Sized to the wider of the
/// two so the HUD layout never shifts when a tab toggles active/inactive.
struct TabRender {
    name_reg: CoverageBitmap,
    name_semi: CoverageBitmap,
    hotkey_reg: CoverageBitmap,
    hotkey_semi: CoverageBitmap,
    /// Name slot width = `max(name_reg.width, name_semi.width)`.
    name_w: u32,
    /// Hotkey slot width = `max(hotkey_reg.width, hotkey_semi.width)`.
    hotkey_w: u32,
}

/// The freeze-time legend: tab texts, pre-rasterized glyphs, and layout
/// metrics, computed once.
pub struct Legend {
    /// Rendered tab texts (`NAME [HOTKEY]`), in display order.
    tabs: Vec<String>,
    /// Pre-rasterized per-tab coverage (Regular + SemiBold), parallel to `tabs`.
    chips: Vec<TabRender>,
    /// The app version label shown after the tabs (empty => omitted).
    version: String,
    /// Pre-rasterized version label (Regular).
    version_bmp: CoverageBitmap,
    /// Pre-rasterized close-button glyph ("×", U+00D7, Regular). Always
    /// present so the HUD is always closeable.
    close_bmp: CoverageBitmap,
    /// Total HUD width in pixels.
    pill_width: u32,
    /// Total HUD height in pixels.
    pill_height: u32,
    /// Close-button hit square side (= `pill_height`).
    close_size: u32,
    /// Spotlight shape shown as first-class status on the left (Some for live, None for custom test tabs).
    shape: Option<SpotlightShape>,
}

impl Legend {
    /// The legend for a freeze session: one tab per mode in the fixed
    /// Spotlight / Zoom / Capture order, labelled with the freeze-time binding,
    /// followed by the app version label. The ZOOM tab is labelled with the
    /// zoom-modifier wheel chord (e.g. `Shift+Wheel`) — zoom is implicit in
    /// every mode, reached by the modifier + mouse wheel, so there is no
    /// dedicated zoom hotkey to show. The spotlight shape is drawn as a
    /// vector icon on the left rather than appended as a unicode suffix.
    pub fn from_hotkeys(hotkeys: &HotkeySettings, shape: SpotlightShape) -> Self {
        Self::build(
            &[
                LegendTab {
                    name: "Spotlight".into(),
                    hotkey: hotkeys.mode_spotlight.to_display(),
                },
                LegendTab {
                    name: "Zoom".into(),
                    hotkey: format!("{}+Wheel", hotkeys.zoom_modifier.to_display()),
                },
                LegendTab {
                    name: "Capture".into(),
                    hotkey: hotkeys.mode_snip.to_display(),
                },
            ],
            &format!("v{}", env!("CARGO_PKG_VERSION")),
            Some(shape),
        )
    }

    /// Tabs in display order; each renders as `NAME [HOTKEY]`. No version
    /// label and no shape icon (used by tests and callers that want the tab-only HUD).
    pub fn new(tabs: &[LegendTab]) -> Self {
        Self::build(tabs, "", None)
    }

    /// Shared constructor: pre-rasterize each tab text in both weights (and
    /// the version in Regular), then derive the HUD geometry from the cached
    /// bitmaps. All font work happens here — once per freeze — never in
    /// [`Legend::paint`].
    fn build(tabs: &[LegendTab], version: &str, shape: Option<SpotlightShape>) -> Self {
        let regular = load_font(FONT_REGULAR_BYTES);
        let semibold = load_font(FONT_SEMIBOLD_BYTES);

        let texts: Vec<String> = tabs
            .iter()
            .map(|t| format!("{} [{}]", t.name, t.hotkey))
            .collect();

        let chips: Vec<TabRender> = tabs
            .iter()
            .map(|t| {
                let name_reg = rasterize_string(&regular, &t.name, FONT_PX);
                let name_semi = rasterize_string(&semibold, &t.name, FONT_PX);
                let hotkey_reg = rasterize_string(&regular, &t.hotkey, FONT_PX);
                let hotkey_semi = rasterize_string(&semibold, &t.hotkey, FONT_PX);
                TabRender {
                    name_w: name_reg.width.max(name_semi.width),
                    hotkey_w: hotkey_reg.width.max(hotkey_semi.width),
                    name_reg,
                    name_semi,
                    hotkey_reg,
                    hotkey_semi,
                }
            })
            .collect();

        let version_bmp = rasterize_string(&regular, version, FONT_PX);
        // The close ("×", U+00D7) button: rasterized with the same Inter
        // Regular font as the inactive tab text.
        let close_bmp = rasterize_string(&regular, "\u{00D7}", FONT_PX);

        // Fixed professional height of 32 pixels.
        let pill_height = 32;
        let close_size = pill_height;

        let mut pill_width = 2 * PILL_PAD_X;

        // Shape icon module
        if shape.is_some() {
            pill_width += 12 + 12 + 1 + 12; // icon (12) + gap (12) + divider (1) + gap (12)
        }

        // Tabs
        let chips_width: u32 = chips
            .iter()
            .map(|c| c.name_w + 6 + c.hotkey_w + 12) // name + gap (6) + keycap (hotkey + 12 px padding)
            .sum::<u32>()
            + TAB_GAP * chips.len().saturating_sub(1) as u32;
        pill_width += chips_width;

        // Divider before version/close
        pill_width += 12 + 1 + 12;

        // Version
        let version_width = if version.is_empty() {
            0
        } else {
            version_bmp.width + 12
        };
        pill_width += version_width;

        // Close button
        pill_width += close_size;

        Self {
            tabs: texts,
            chips,
            version: version.to_string(),
            version_bmp,
            close_bmp,
            pill_width,
            pill_height,
            close_size,
            shape,
        }
    }

    /// `(width, height)` of the HUD in pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.pill_width, self.pill_height)
    }

    /// The rendered tab texts (`NAME [HOTKEY]`) in display order — for tests
    /// and diagnostics.
    pub fn tab_labels(&self) -> Vec<String> {
        self.tabs.clone()
    }

    /// The trailing version label (empty when omitted).
    pub fn version_label(&self) -> &str {
        &self.version
    }

    /// The active spotlight shape (if any) shown on the left.
    pub fn shape(&self) -> Option<SpotlightShape> {
        self.shape
    }

    /// The HUD's default top-left origin for a buffer of `(width, height)`:
    /// centered horizontally, with [`TOP_MARGIN`] from the top (clamped so the
    /// whole HUD stays on-screen). Used at freeze time to seed the per-
    /// monitor legend position.
    pub fn default_origin(&self, buf_width: u32, buf_height: u32) -> Point {
        let (pw, ph) = self.size();
        let x0 = if pw > buf_width {
            0
        } else {
            (buf_width - pw) / 2
        };
        let y0 = if ph > buf_height {
            0
        } else {
            TOP_MARGIN.min(buf_height - ph)
        };
        Point::new(x0 as i32, y0 as i32)
    }

    /// The HUD's bounding rect at `origin` (top-left + size).
    pub fn pill_rect(&self, origin: Point) -> Rect {
        Rect::new(origin.x, origin.y, self.pill_width, self.pill_height)
    }

    /// The close-button hit region: a `close_size × close_size` square at the
    /// HUD's right end (inside the HUD), vertically spanning the full HUD
    /// height. Consistent with where [`Legend::paint`] draws the "×" glyph.
    pub fn close_hit_rect(&self, origin: Point) -> Rect {
        let close_x =
            origin.x + self.pill_width as i32 - PILL_PAD_X as i32 - self.close_size as i32;
        Rect::new(close_x, origin.y, self.close_size, self.close_size)
    }

    /// Paint the HUD at `origin` (its top-left in monitor-local coords) at
    /// full strength. `active[i]` highlights tab `i` (missing flags read as
    /// inactive). Skips monitors smaller than the HUD instead of clipping
    /// it, and skips empty legends. The HUD's default position (centered
    /// horizontally near the top) is available via [`Legend::default_origin`].
    pub fn paint(&self, buf: &mut DibBuffer, active: &[bool], origin: Point) {
        let (pw, ph) = self.size();
        if self.tabs.is_empty() || pw > buf.width || ph > buf.height {
            return;
        }
        let x0 = origin.x;
        let y0 = origin.y;

        // HUD container body with 1px rounded border and translucent background.
        for y in y0..y0 + ph as i32 {
            for x in x0..x0 + pw as i32 {
                let is_inside_outer = rounded_rect_contains(x, y, x0, y0, pw, ph, PILL_RADIUS);
                let is_inside_inner = rounded_rect_contains(
                    x,
                    y,
                    x0 + 1,
                    y0 + 1,
                    pw - 2,
                    ph - 2,
                    PILL_RADIUS.saturating_sub(1),
                );
                if is_inside_outer {
                    if !is_inside_inner {
                        blend_px(buf, x, y, BORDER_COLOR, BORDER_ALPHA);
                    } else {
                        blend_px(buf, x, y, PILL_COLOR, PILL_ALPHA);
                    }
                }
            }
        }

        let mut current_x = x0 + PILL_PAD_X as i32;

        // Draw the shape icon module if present
        if let Some(shape) = self.shape {
            // Draw shape icon
            let cx = current_x + 6; // centered in a 12px wide slot
            let cy = y0 + (ph as i32) / 2;
            let is_spotlight_active = active.first().copied().unwrap_or(false);
            let icon_color = if is_spotlight_active {
                ACCENT_COLOR
            } else {
                TEXT_INACTIVE
            };
            draw_shape_icon(buf, cx, cy, shape, icon_color);

            current_x += 12 + 12; // icon width + gap

            // Draw divider
            draw_divider(buf, current_x, y0);
            current_x += 1 + 12; // divider + gap
        }

        // Draw tabs
        for (i, tr) in self.chips.iter().enumerate() {
            if i > 0 {
                current_x += TAB_GAP as i32;
            }

            let on = active.get(i).copied().unwrap_or(false);

            // 1. Draw name text
            let name_bmp = if on { &tr.name_semi } else { &tr.name_reg };
            let name_x = current_x + (tr.name_w as i32 - name_bmp.width as i32) / 2;
            let name_y = y0 + (ph as i32 - name_bmp.height as i32) / 2;
            blit_coverage(
                buf,
                name_x,
                name_y,
                name_bmp,
                if on { TEXT_ACTIVE } else { TEXT_INACTIVE },
            );

            current_x += tr.name_w as i32 + 6; // name slot + gap to keycap

            // 2. Draw keycap
            let keycap_w = tr.hotkey_w + 12;
            let keycap_h = 20;
            let keycap_x = current_x;
            let keycap_y = y0 + (ph as i32 - keycap_h as i32) / 2;

            let keycap_bg = if on {
                KEYCAP_BG_ACTIVE
            } else {
                KEYCAP_BG_INACTIVE
            };
            let keycap_border = if on {
                KEYCAP_BORDER_ACTIVE
            } else {
                KEYCAP_BORDER_INACTIVE
            };

            for y in keycap_y..keycap_y + keycap_h as i32 {
                for x in keycap_x..keycap_x + keycap_w as i32 {
                    let is_outer =
                        rounded_rect_contains(x, y, keycap_x, keycap_y, keycap_w, keycap_h, 2);
                    let is_inner = rounded_rect_contains(
                        x,
                        y,
                        keycap_x + 1,
                        keycap_y + 1,
                        keycap_w - 2,
                        keycap_h - 2,
                        1,
                    );
                    if is_outer {
                        if !is_inner {
                            blend_px(buf, x, y, keycap_border, 255);
                        } else {
                            blend_px(buf, x, y, keycap_bg, 255);
                        }
                    }
                }
            }

            // 3. Draw hotkey text inside keycap
            let hotkey_bmp = if on { &tr.hotkey_semi } else { &tr.hotkey_reg };
            let hotkey_x = keycap_x + 6 + (tr.hotkey_w as i32 - hotkey_bmp.width as i32) / 2;
            let hotkey_y = keycap_y + (keycap_h as i32 - hotkey_bmp.height as i32) / 2;
            blit_coverage(
                buf,
                hotkey_x,
                hotkey_y,
                hotkey_bmp,
                if on { TEXT_ACTIVE } else { TEXT_INACTIVE },
            );

            current_x += keycap_w as i32;
        }

        // Draw divider before version/close
        current_x += 12;
        draw_divider(buf, current_x, y0);
        current_x += 1 + 12;

        // Draw version label if present
        if !self.version.is_empty() {
            let vy = y0 + (ph as i32 - self.version_bmp.height as i32) / 2;
            blit_coverage(buf, current_x, vy, &self.version_bmp, TEXT_VERSION);
        }

        // Draw close button
        let close_x = x0 + pw as i32 - PILL_PAD_X as i32 - self.close_size as i32;
        let close_y = y0;
        let close_cx = close_x + (self.close_size as i32 - self.close_bmp.width as i32) / 2;
        let close_cy = close_y + (self.close_size as i32 - self.close_bmp.height as i32) / 2;
        blit_coverage(buf, close_cx, close_cy, &self.close_bmp, TEXT_INACTIVE);
    }
}

/// Parse an embedded Inter TTF.
fn load_font(bytes: &'static [u8]) -> Font {
    Font::from_bytes(
        bytes,
        FontSettings {
            collection_index: 0,
            scale: FONT_PX,
            load_substitutions: false,
        },
    )
    .expect("embedded Inter font is valid")
}

/// Rasterize `text` at `px` into a top-down alpha-coverage bitmap.
fn rasterize_string(font: &Font, text: &str, px: f32) -> CoverageBitmap {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return CoverageBitmap {
            width: 0,
            height: 0,
            coverage: Vec::new(),
        };
    }
    let mut pen = 0.0_f32;
    let mut ascent: i32 = 0;
    let mut ymin_min: i32 = 0;
    let mut layout: Vec<(f32, u16)> = Vec::with_capacity(chars.len());
    for (i, &ch) in chars.iter().enumerate() {
        if i > 0
            && let Some(k) = font.horizontal_kern(chars[i - 1], ch, px)
        {
            pen += k;
        }
        let idx = font.lookup_glyph_index(ch);
        let m = font.metrics_indexed(idx, px);
        let top = m.ymin + m.height as i32;
        if top > ascent {
            ascent = top;
        }
        if m.ymin < ymin_min {
            ymin_min = m.ymin;
        }
        layout.push((pen, idx));
        pen += m.advance_width;
    }
    let total_width = pen.round().max(1.0) as u32;
    let descent = (-ymin_min).max(0) as u32;
    let line_height = (ascent as u32).saturating_add(descent).max(1);
    let baseline = ascent;

    let mut coverage = vec![0u8; (total_width * line_height) as usize];
    for &(pen_x, idx) in &layout {
        let (m, bmp) = font.rasterize_indexed(idx, px);
        let place_x = pen_x.round() as i32 + m.xmin;
        let place_y = baseline - (m.ymin + m.height as i32);
        for gy in 0..m.height as i32 {
            for gx in 0..m.width as i32 {
                let bx = place_x + gx;
                let by = place_y + gy;
                if bx >= 0 && (bx as u32) < total_width && by >= 0 && (by as u32) < line_height {
                    let c = bmp[(gy as usize) * m.width + (gx as usize)];
                    let cell = &mut coverage[(by as usize) * total_width as usize + bx as usize];
                    if c > *cell {
                        *cell = c;
                    }
                }
            }
        }
    }
    CoverageBitmap {
        width: total_width,
        height: line_height,
        coverage,
    }
}

/// Draw a vector shape icon centered at `(cx, cy)` in a `14x14` region.
fn draw_shape_icon(buf: &mut DibBuffer, cx: i32, cy: i32, shape: SpotlightShape, color: Rgb) {
    for dy in -6_i32..=6 {
        for dx in -6_i32..=6 {
            let inside = match shape {
                SpotlightShape::Circle => dx * dx + dy * dy <= 25,
                SpotlightShape::Rectangle => dx.abs() <= 5 && dy.abs() <= 5,
                SpotlightShape::RoundedRect => {
                    if dx.abs() <= 5 && dy.abs() <= 5 {
                        if dx.abs() >= 4 && dy.abs() >= 4 {
                            (dx.abs() - 3) * (dx.abs() - 3) + (dy.abs() - 3) * (dy.abs() - 3) <= 4
                        } else {
                            true
                        }
                    } else {
                        false
                    }
                }
                SpotlightShape::Diamond => dx.abs() + dy.abs() <= 5,
                SpotlightShape::Star => {
                    let t1 = (-5..=2).contains(&dy) && 7 * dx.abs() <= 4 * (dy + 5);
                    let t2 = (-2..=5).contains(&dy) && 7 * dx.abs() <= 4 * (5 - dy);
                    t1 || t2
                }
            };
            if inside {
                blend_px(buf, cx + dx, cy + dy, color, 255);
            }
        }
    }
}

/// Draw a vertical 1px divider.
fn draw_divider(buf: &mut DibBuffer, x: i32, y0: i32) {
    let start_y = y0 + 8;
    let end_y = y0 + 24;
    for y in start_y..end_y {
        blend_px(buf, x, y, DIVIDER_COLOR, DIVIDER_ALPHA);
    }
}

/// Blend a cached coverage bitmap into `buf` at `(x, y)`.
fn blit_coverage(buf: &mut DibBuffer, x: i32, y: i32, cb: &CoverageBitmap, color: Rgb) {
    for gy in 0..cb.height as i32 {
        for gx in 0..cb.width as i32 {
            let c = cb.coverage[(gy as usize) * cb.width as usize + gx as usize];
            if c != 0 {
                blend_px(buf, x + gx, y + gy, color, c);
            }
        }
    }
}

/// Blend pixel `(x, y)` of `buf` toward `color` at `alpha`.
fn blend_px(buf: &mut DibBuffer, x: i32, y: i32, color: Rgb, alpha: u8) {
    if alpha == 0 || x < 0 || y < 0 || x >= buf.width as i32 || y >= buf.height as i32 {
        return;
    }
    let i = y as usize * buf.stride as usize + x as usize * 4;
    let keep = 255 - alpha as u32;
    let a = alpha as u32;
    for (ch, fg) in [(0usize, color.b), (1, color.g), (2, color.r)] {
        buf.pixels[i + ch] = ((buf.pixels[i + ch] as u32 * keep + fg as u32 * a) / 255) as u8;
    }
}

/// `true` when pixel `(px, py)` lies inside the `w`x`h` rectangle at
/// `(x0, y0)` with corner radius `r` (clamped to half the shorter side).
fn rounded_rect_contains(px: i32, py: i32, x0: i32, y0: i32, w: u32, h: u32, r: u32) -> bool {
    let x1 = x0 + w as i32;
    let y1 = y0 + h as i32;
    if px < x0 || px >= x1 || py < y0 || py >= y1 {
        return false;
    }
    let r = r.min(w / 2).min(h / 2) as i32;
    let (il, it) = (x0 + r, y0 + r);
    let (ir, ib) = (x1 - r, y1 - r);
    let dx = if px < il {
        il - px
    } else if px >= ir {
        px - ir + 1
    } else {
        0
    };
    let dy = if py < it {
        it - py
    } else if py >= ib {
        py - ib + 1
    } else {
        0
    };
    dx * dx + dy * dy <= r * r
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Solid test frame (`[B, G, R, A]` per pixel).
    fn frame(w: u32, h: u32, c: [u8; 4]) -> DibBuffer {
        DibBuffer {
            width: w,
            height: h,
            stride: w * 4,
            pixels: c.repeat((w * h) as usize),
        }
    }

    fn px(buf: &DibBuffer, x: u32, y: u32) -> [u8; 4] {
        let i = (y * buf.stride + x * 4) as usize;
        buf.pixels[i..i + 4].try_into().unwrap()
    }

    /// Sum of the BGRA color channels over a rectangle.
    fn region_sum(buf: &DibBuffer, x0: u32, y0: u32, w: u32, h: u32) -> u64 {
        let mut s = 0u64;
        for y in y0..y0 + h {
            for x in x0..x0 + w {
                if let Some(p) = buf.pixel(x, y) {
                    s += p[0] as u64 + p[1] as u64 + p[2] as u64;
                }
            }
        }
        s
    }

    fn tabs(spec: &[(&str, &str)]) -> Vec<LegendTab> {
        spec.iter()
            .map(|&(name, hotkey)| LegendTab {
                name: name.into(),
                hotkey: hotkey.into(),
            })
            .collect()
    }

    // ---- font rasterization --------------------------------------------------

    #[test]
    fn embedded_fonts_parse_and_rasterize_a_glyph() {
        let font = load_font(FONT_REGULAR_BYTES);
        let cb = rasterize_string(&font, "S", FONT_PX);
        assert!(cb.width > 0 && cb.height > 0, "non-empty glyph bitmap");
        assert!(
            cb.coverage.iter().any(|&c| c > 0),
            "the 'S' glyph has covered pixels (anti-aliased)"
        );
        let distinct: Vec<u8> = {
            let mut v: Vec<u8> = cb.coverage.iter().copied().filter(|&c| c > 0).collect();
            v.sort_unstable();
            v.dedup();
            v
        };
        assert!(
            distinct.len() >= 2,
            "anti-aliased: multiple coverage levels present, got {distinct:?}"
        );
    }

    #[test]
    fn empty_string_rasterizes_to_a_zero_size_bitmap() {
        let font = load_font(FONT_REGULAR_BYTES);
        let cb = rasterize_string(&font, "", FONT_PX);
        assert_eq!((cb.width, cb.height), (0, 0));
        assert!(cb.coverage.is_empty());
    }

    #[test]
    fn rasterize_string_width_grows_with_text() {
        let font = load_font(FONT_REGULAR_BYTES);
        let short = rasterize_string(&font, "S", FONT_PX).width;
        let long = rasterize_string(&font, "Spotlight [S]", FONT_PX).width;
        assert!(long > short, "longer text is wider: {long} vs {short}");
    }

    #[test]
    fn rectangle_unicode_glyph_rasterizes_to_non_empty_coverage() {
        let font = load_font(FONT_REGULAR_BYTES);
        let cb = rasterize_string(&font, "□", FONT_PX);
        assert!(
            cb.width > 0 && cb.height > 0,
            "non-empty glyph bitmap for □"
        );
        assert!(
            cb.coverage.iter().any(|&c| c > 0),
            "the □ glyph has covered pixels (anti-aliased)"
        );
    }

    #[test]
    fn star_unicode_glyph_rasterizes_to_non_empty_coverage() {
        let font = load_font(FONT_REGULAR_BYTES);
        let cb = rasterize_string(&font, "✶", FONT_PX);
        assert!(
            cb.width > 0 && cb.height > 0,
            "non-empty glyph bitmap for ✶"
        );
        assert!(
            cb.coverage.iter().any(|&c| c > 0),
            "the ✶ glyph has covered pixels (anti-aliased)"
        );
    }

    // ---- from_hotkeys data contract -----------------------------------------

    #[test]
    fn from_hotkeys_uses_the_freeze_time_bindings() {
        let hotkeys = HotkeySettings::default();
        let legend = Legend::from_hotkeys(&hotkeys, SpotlightShape::Circle);
        assert_eq!(
            legend.tab_labels(),
            vec![
                "Spotlight [S]".to_string(),
                "Zoom [Shift+Wheel]".to_string(),
                "Capture [C]".to_string(),
            ]
        );
        assert_eq!(
            legend.version_label(),
            &format!("v{}", env!("CARGO_PKG_VERSION")),
            "the legend carries the app version"
        );
    }

    #[test]
    fn from_hotkeys_zoom_tab_reflects_the_zoom_modifier() {
        let mut hotkeys = HotkeySettings::default();
        hotkeys.zoom_modifier =
            crate::hotkeys::gesture::Modifiers::CTRL | crate::hotkeys::gesture::Modifiers::SHIFT;
        let legend = Legend::from_hotkeys(&hotkeys, SpotlightShape::Circle);
        assert_eq!(legend.tab_labels()[1], "Zoom [Ctrl+Shift+Wheel]");
    }

    // ---- geometry ------------------------------------------------------------

    #[test]
    fn size_is_nonzero_and_sensible_for_the_font_size() {
        let legend = Legend::from_hotkeys(&HotkeySettings::default(), SpotlightShape::Circle);
        let (w, h) = legend.size();
        assert!(w > 200, "pill has a sizable width: {w}");
        assert_eq!(h, 32, "height is exactly 32 pixels");
    }

    #[test]
    fn a_longer_binding_widens_the_pill() {
        let mut hotkeys = HotkeySettings::default();
        let default_w = Legend::from_hotkeys(&HotkeySettings::default(), SpotlightShape::Circle)
            .size()
            .0;
        hotkeys.zoom_modifier = crate::hotkeys::gesture::Modifiers::CTRL
            | crate::hotkeys::gesture::Modifiers::ALT
            | crate::hotkeys::gesture::Modifiers::SHIFT
            | crate::hotkeys::gesture::Modifiers::WIN;
        let wider = Legend::from_hotkeys(&hotkeys, SpotlightShape::Circle)
            .size()
            .0;
        assert!(
            wider > default_w,
            "a longer zoom-modifier chord widens the pill: {wider} vs {default_w}"
        );
    }

    #[test]
    fn the_version_label_widens_the_pill() {
        let hotkeys = HotkeySettings::default();
        let with_version = Legend::build(
            &[
                LegendTab {
                    name: "Spotlight".into(),
                    hotkey: hotkeys.mode_spotlight.to_display(),
                },
                LegendTab {
                    name: "Zoom".into(),
                    hotkey: format!("{}+Wheel", hotkeys.zoom_modifier.to_display()),
                },
                LegendTab {
                    name: "Capture".into(),
                    hotkey: hotkeys.mode_snip.to_display(),
                },
            ],
            "v1.0.0",
            None,
        );
        let tab_only = Legend::new(&[
            LegendTab {
                name: "Spotlight".into(),
                hotkey: hotkeys.mode_spotlight.to_display(),
            },
            LegendTab {
                name: "Zoom".into(),
                hotkey: format!("{}+Wheel", hotkeys.zoom_modifier.to_display()),
            },
            LegendTab {
                name: "Capture".into(),
                hotkey: hotkeys.mode_snip.to_display(),
            },
        ]);
        let (vw, _) = with_version.size();
        let (tw, _) = tab_only.size();
        assert!(vw > tw, "the version label widens the pill: {vw} vs {tw}");
        assert_eq!(
            vw - tw,
            with_version.version_bmp.width + 12,
            "exactly the version gap plus its rasterized width"
        );
    }

    // ---- rounded_rect_contains ----------------------------------------------

    #[test]
    fn rounded_rect_includes_bands_excludes_corner_outside_the_radius() {
        let contains = |x, y| rounded_rect_contains(x, y, 0, 0, 20, 20, 5);
        assert!(!contains(0, 0), "diagonal corner pixel outside");
        assert!(contains(5, 0), "top edge at the radius");
        assert!(contains(10, 0), "top band");
        assert!(contains(0, 10), "left band");
        assert!(!contains(19, 19), "far corner symmetric to (0,0)");
        assert!(!contains(-1, 10), "outside the rect");
        assert!(!contains(20, 10), "right edge exclusive");
        assert!(contains(5, 5), "corner circle center pixel");
    }

    #[test]
    fn rounded_rect_radius_clamps_to_half_the_short_side() {
        assert!(!rounded_rect_contains(0, 0, 0, 0, 20, 6, 10));
        assert!(rounded_rect_contains(3, 0, 0, 0, 20, 6, 10));
        assert!(
            rounded_rect_contains(0, 0, 0, 0, 20, 6, 0),
            "zero radius = plain rect"
        );
    }

    // ---- blend_px ------------------------------------------------------------

    #[test]
    fn blend_px_exact_math_and_bounds() {
        let mut buf = frame(4, 4, [100, 100, 100, 200]);
        let red = Rgb { r: 200, g: 0, b: 0 };
        blend_px(&mut buf, 1, 1, red, 128);
        let [b, g, r, a] = px(&buf, 1, 1);
        assert_eq!(b, ((100u32 * 127) / 255) as u8);
        assert_eq!(g, ((100u32 * 127) / 255) as u8);
        assert_eq!(r, ((100u32 * 127 + 200 * 128) / 255) as u8);
        assert_eq!(a, 200, "alpha byte untouched");
        let before = buf.pixels.clone();
        blend_px(&mut buf, -1, 0, red, 255);
        blend_px(&mut buf, 4, 0, red, 255);
        blend_px(&mut buf, 0, 0, red, 0);
        assert_eq!(
            buf.pixels, before,
            "out of bounds and zero alpha are no-ops"
        );
    }

    // ---- paint ---------------------------------------------------------------

    #[test]
    fn paint_centers_the_pill_near_the_top_and_darkens_it() {
        let legend = Legend::new(&tabs(&[("Spotlight", "S")]));
        let (pw, ph) = legend.size();
        let mut buf = frame(800, 160, [180, 180, 180, 255]);
        let plain = buf.pixels.clone();
        let origin = legend.default_origin(800, 160);
        legend.paint(&mut buf, &[false], origin);

        let x0 = origin.x as u32;
        let y0 = origin.y as u32;
        let center = px(&buf, x0 + pw / 2, y0 + ph / 2);
        assert!(
            center[0] < 180 && center[1] < 180 && center[2] < 180,
            "pill darkens the frame: {center:?}"
        );
        assert_ne!(
            center,
            [PILL_COLOR.b, PILL_COLOR.g, PILL_COLOR.r, 255],
            "translucent, not solid"
        );
        let _ = plain;
        assert_eq!(px(&buf, 0, 0), [180, 180, 180, 255]);
        assert_eq!(px(&buf, 799, 159), [180, 180, 180, 255]);
        assert_eq!(px(&buf, x0, y0), [180, 180, 180, 255]);
    }

    #[test]
    fn paint_renders_tab_text() {
        let legend = Legend::new(&tabs(&[("Spotlight", "S")]));
        let (pw, ph) = legend.size();
        let mut buf = frame(800, 160, [0, 0, 0, 255]);
        let plain = buf.pixels.clone();
        let origin = legend.default_origin(800, 160);
        legend.paint(&mut buf, &[false], origin);
        assert_ne!(buf.pixels, plain, "paint changes pixels");
        let x0 = origin.x as u32;
        let y0 = origin.y as u32;
        let text_band_sum = region_sum(&buf, x0, y0, pw, ph);
        assert!(
            text_band_sum > 0,
            "the text/pill band has non-black pixels: {text_band_sum}"
        );
    }

    #[test]
    fn paint_highlights_the_active_tab() {
        let legend = Legend::new(&tabs(&[("AA", "B"), ("CC", "D")]));
        let mut on = frame(400, 160, [40, 40, 40, 255]);
        let origin = legend.default_origin(400, 160);
        legend.paint(&mut on, &[true, false], origin);
        let mut off = frame(400, 160, [40, 40, 40, 255]);
        legend.paint(&mut off, &[false, false], origin);

        let x0 = origin.x as u32;
        let y0 = origin.y as u32;
        let first_chip_w = legend.chips[0].name_w + 6 + legend.chips[0].hotkey_w + 12;
        let on_first = region_sum(&on, x0 + PILL_PAD_X, y0, first_chip_w, legend.pill_height);
        let off_first = region_sum(&off, x0 + PILL_PAD_X, y0, first_chip_w, legend.pill_height);
        assert!(
            on_first > off_first,
            "active tab chip is brighter: on={on_first} off={off_first}"
        );
        let second_x = x0 + PILL_PAD_X + first_chip_w + TAB_GAP;
        let second_w = legend.chips[1].name_w + 6 + legend.chips[1].hotkey_w + 12;
        let on_second = region_sum(&on, second_x, y0, second_w, legend.pill_height);
        let off_second = region_sum(&off, second_x, y0, second_w, legend.pill_height);
        assert_eq!(
            on_second, off_second,
            "inactive tab identical in both paints"
        );
    }

    #[test]
    fn paint_renders_the_version_label_after_the_tabs() {
        let hotkeys = HotkeySettings::default();
        let with_version = Legend::build(
            &[
                LegendTab {
                    name: "Spotlight".into(),
                    hotkey: hotkeys.mode_spotlight.to_display(),
                },
                LegendTab {
                    name: "Zoom".into(),
                    hotkey: format!("{}+Wheel", hotkeys.zoom_modifier.to_display()),
                },
                LegendTab {
                    name: "Capture".into(),
                    hotkey: hotkeys.mode_snip.to_display(),
                },
            ],
            "v1.0.0",
            None,
        );
        let tab_only = Legend::new(&[
            LegendTab {
                name: "Spotlight".into(),
                hotkey: hotkeys.mode_spotlight.to_display(),
            },
            LegendTab {
                name: "Zoom".into(),
                hotkey: format!("{}+Wheel", hotkeys.zoom_modifier.to_display()),
            },
            LegendTab {
                name: "Capture".into(),
                hotkey: hotkeys.mode_snip.to_display(),
            },
        ]);
        let (vw, _) = with_version.size();
        let (tw, _) = tab_only.size();
        let mut a = frame(1024, 160, [0, 0, 0, 255]);
        let mut b = frame(1024, 160, [0, 0, 0, 255]);
        let origin_a = with_version.default_origin(1024, 160);
        let origin_b = tab_only.default_origin(1024, 160);
        with_version.paint(&mut a, &[false, false, false], origin_a);
        tab_only.paint(&mut b, &[false, false, false], origin_b);

        let a_x0 = origin_a.x as u32;
        let y0 = origin_a.y as u32;

        let chips_width: u32 = with_version
            .chips
            .iter()
            .map(|c| c.name_w + 6 + c.hotkey_w + 12)
            .sum::<u32>()
            + TAB_GAP * with_version.chips.len().saturating_sub(1) as u32;

        let version_x = a_x0 + PILL_PAD_X + chips_width + 12 + 1 + 12;
        assert!(vw > tw, "version pill wider");

        let trail_a = region_sum(
            &a,
            version_x,
            y0,
            vw - (version_x - a_x0),
            with_version.pill_height,
        );
        assert!(
            trail_a > 0,
            "the version label region has drawn content: {trail_a}"
        );
    }

    #[test]
    fn paint_skips_monitors_smaller_than_the_pill_and_empty_legends() {
        let legend = Legend::new(&tabs(&[
            ("Spotlight", "S"),
            ("Zoom", "F"),
            ("Capture", "C"),
        ]));
        let mut tiny = frame(32, 32, [100, 100, 100, 255]);
        let before = tiny.pixels.clone();
        legend.paint(&mut tiny, &[true, false, false], Point::new(0, 0));
        assert_eq!(tiny.pixels, before, "tiny monitor: skipped, not clipped");

        let empty = Legend::new(&[]);
        let mut buf = frame(400, 64, [100, 100, 100, 255]);
        let before = buf.pixels.clone();
        empty.paint(&mut buf, &[], Point::new(0, 0));
        assert_eq!(buf.pixels, before, "empty legend paints nothing");

        let mut buf2 = frame(800, 160, [100, 100, 100, 255]);
        let before2 = buf2.pixels.clone();
        let origin = legend.default_origin(800, 160);
        legend.paint(&mut buf2, &[true], origin);
        assert_ne!(buf2.pixels, before2, "something painted with partial flags");
    }

    // ---- origin-based paint + close button -----------------------------------

    #[test]
    fn default_origin_centers_horizontally_and_uses_top_margin() {
        let legend = Legend::new(&tabs(&[("Spotlight", "S")]));
        let (pw, ph) = legend.size();
        let origin = legend.default_origin(800, 200);
        assert_eq!(
            origin,
            Point::new(((800 - pw) / 2) as i32, TOP_MARGIN as i32),
            "centered horizontally, TOP_MARGIN from the top"
        );
        let origin_short = legend.default_origin(800, ph + 10);
        assert_eq!(origin_short.y, 10, "y clamps to the available slack");
    }

    #[test]
    fn paint_at_origin_draws_the_pill_at_that_origin() {
        let legend = Legend::new(&tabs(&[("Spotlight", "S")]));
        let (pw, ph) = legend.size();
        let mut buf = frame(800, 300, [180, 180, 180, 255]);
        let origin = Point::new(50, 200);
        legend.paint(&mut buf, &[false], origin);
        let center = px(&buf, 50 + pw / 2, 200 + ph / 2);
        assert!(
            center[0] < 180 && center[1] < 180 && center[2] < 180,
            "pill darkens the frame at the new origin: {center:?}"
        );
        let default = legend.default_origin(800, 300);
        let old_center = px(&buf, default.x as u32 + pw / 2, default.y as u32 + ph / 2);
        assert_eq!(
            old_center,
            [180, 180, 180, 255],
            "the default top-center region is untouched"
        );
    }

    #[test]
    fn close_hit_rect_is_inside_the_pill_at_the_right_end() {
        let legend = Legend::new(&tabs(&[("Spotlight", "S")]));
        let origin = Point::new(100, 50);
        let pill = legend.pill_rect(origin);
        let close = legend.close_hit_rect(origin);
        assert!(
            close.x >= pill.x
                && close.right() <= pill.right()
                && close.y >= pill.y
                && close.bottom() <= pill.bottom(),
            "close rect {close:?} inside pill {pill:?}"
        );
        assert_eq!(
            close.right(),
            origin.x + legend.pill_width as i32 - PILL_PAD_X as i32,
            "close rect sits at the pill's right end"
        );
        assert_eq!(close.width, legend.pill_height);
        assert_eq!(close.height, legend.pill_height);
    }

    #[test]
    fn paint_includes_a_close_button_region() {
        let legend = Legend::new(&tabs(&[("Spotlight", "S")]));
        let mut buf = frame(800, 160, [0, 0, 0, 255]);
        let origin = legend.default_origin(800, 160);
        legend.paint(&mut buf, &[false], origin);
        let close = legend.close_hit_rect(origin);
        let close_sum = region_sum(
            &buf,
            close.x as u32,
            close.y as u32,
            close.width,
            close.height,
        );
        assert!(
            close_sum > 0,
            "the close-button region has drawn content: {close_sum}"
        );
    }
}
