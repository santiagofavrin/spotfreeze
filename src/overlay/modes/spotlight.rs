//! Spotlight LAYER: the frozen screen is darkened except a clear circle around
//! the cursor. Pure state machine — pixel work moved to
//! [`crate::overlay::composite::compose_frame`], which the controller feeds
//! with this layer's [`SpotlightMode::cursor`]/[`SpotlightMode::radius`] via
//! [`crate::overlay::modes::ModeStack::render_state`].
//!
//! The spotlight key `S` cycles the shape (Circle → Diamond → Star → RoundedRect → Rectangle)
//! and then turns the layer off on the last shape — see
//! [`is_last_shape`](SpotlightMode::is_last_shape) and
//! [`cycle_shape`](SpotlightMode::cycle_shape).

use super::ModeEffect;
use crate::geometry::{Point, Rect, SpotlightShape};

/// Smallest selectable spotlight radius (physical px).
const MIN_RADIUS: u32 = 10;
/// Largest selectable spotlight radius (physical px).
const MAX_RADIUS: u32 = 1000;
/// Win32 `WHEEL_DELTA`: one wheel notch.
const WHEEL_DELTA: i64 = 120;
/// The wheel step is proportional to the CURRENT radius:
/// `radius / RADIUS_STEP_DIVISOR` px per notch (minimum 1 px), so the resize
/// curve is tighter when the spotlight is small and broader when it is
/// large. At the default radius (150 px) the step is 10 px — the historical
/// fixed step, so the out-of-box feel is unchanged.
const RADIUS_STEP_DIVISOR: u32 = 15;

/// Wheel step in px per notch at `radius` (minimum 1 px).
fn radius_step(radius: u32) -> i64 {
    (radius / RADIUS_STEP_DIVISOR).max(1) as i64
}

/// Axis-aligned bounding box of the spotlight circle in monitor-local pixels.
/// `+1` on each axis: the circle `dx^2 + dy^2 <= r^2` reaches `cx + r`
/// inclusive. Unclipped — dirty regions may extend past the monitor edge;
/// the controller clips them to the window.
fn circle_bbox(center: Point, radius: u32) -> Rect {
    let r = radius as i32;
    Rect::new(center.x - r, center.y - r, radius * 2 + 1, radius * 2 + 1)
}

/// Smallest rect covering both `a` and `b`; an empty rect contributes nothing.
///
/// Empty/right/bottom math is inlined from pub fields (per the `geometry`
/// contract: empty = either axis is 0) so this module stays independent of
/// the `Rect` helper methods.
fn rect_union(a: Rect, b: Rect) -> Rect {
    if a.width == 0 || a.height == 0 {
        return b;
    }
    if b.width == 0 || b.height == 0 {
        return a;
    }
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width as i32).max(b.x + b.width as i32);
    let bottom = (a.y + a.height as i32).max(b.y + b.height as i32);
    Rect::new(x, y, (right - x) as u32, (bottom - y) as u32)
}

/// Repaint effect for a circle that moved/resized from `old` to `new`, each a
/// `(monitor, bbox)` pair. Cross-monitor moves repaint both monitors.
fn circle_repaint(old: (usize, Rect), new: (usize, Rect)) -> ModeEffect {
    let mut effect = ModeEffect::none();
    if old.0 == new.0 {
        effect.repaint.push((new.0, Some(rect_union(old.1, new.1))));
    } else {
        effect.repaint.push((old.0, Some(old.1)));
        effect.repaint.push((new.0, Some(new.1)));
    }
    effect
}

/// Spotlight layer state: cursor position + circle radius.
///
/// Wheel events resize the circle. The layer applies EVERY wheel event it is
/// offered — the [`super::ModeStack`] routes only PLAIN wheel events (no
/// modifiers held) here, so resizing needs no modifier and modifier chords
/// (e.g. the zoom-modifier wheel) never reach the layer.
///
/// Wheel deltas arrive in RAW Win32 units (one notch = [`WHEEL_DELTA`] = 120).
/// Smooth-scroll hardware (precision touchpads, high-resolution wheels) sends
/// sub-notch deltas (|delta| < 120) that pure per-event truncation would
/// silently drop, so they are banked in `wheel_accum`: every event consumes
/// only the delta its whole-pixel step accounts for and keeps the truncation
/// remainder for the next event (Bresenham-style), making resize responsive
/// at any scroll granularity without drift.
pub struct SpotlightMode {
    cursor: Point,
    cursor_monitor: usize,
    radius: u32,
    shape: SpotlightShape,
    /// Unconsumed wheel delta, banked in `delta × step` units at the current
    /// radius's step (`WHEEL_DELTA` units = 1 px). |value| stays below
    /// `WHEEL_DELTA` after every applied resize, i.e. the remainder is
    /// always worth less than 1 px.
    wheel_accum: i64,
}

impl SpotlightMode {
    /// `default_radius` in physical pixels (settings: `spotlight.default_radius`).
    ///
    /// The radius is clamped to `10..=1000` px, the same range wheel resizing
    /// is clamped to, so a rogue settings value cannot break the invariant.
    pub fn new(default_radius: u32, shape: SpotlightShape) -> Self {
        Self {
            cursor: Point::default(),
            cursor_monitor: 0,
            radius: default_radius.clamp(MIN_RADIUS, MAX_RADIUS),
            shape,
            wheel_accum: 0,
        }
    }

    /// Current circle radius in physical pixels (wheel-adjusted).
    pub fn radius(&self) -> u32 {
        self.radius
    }

    /// Cursor position the circle is centered on (monitor-local px).
    pub fn cursor(&self) -> Point {
        self.cursor
    }

    /// Monitor the cursor (and therefore the hole) is on.
    pub fn cursor_monitor(&self) -> usize {
        self.cursor_monitor
    }

    /// Spotlight shape (circle, diamond, star, rounded_rect, or rectangle).
    pub fn shape(&self) -> SpotlightShape {
        self.shape
    }

    /// `true` when the current shape is the LAST entry in
    /// [`SpotlightShape::ALL`] (i.e. `Rectangle`). The stack uses this to
    /// decide whether the spotlight key should advance the shape or turn the
    /// layer off.
    pub fn is_last_shape(&self) -> bool {
        self.shape as usize == SpotlightShape::ALL.len() - 1
    }

    /// Cycle to the next shape in `SpotlightShape::ALL`, wrapping around.
    /// The stack guards the wrap with [`is_last_shape`](Self::is_last_shape)
    /// so the spotlight key turns the layer off instead of wrapping back to
    /// Circle.
    /// Returns a `ModeEffect` that repaints the current hole region (the
    /// bounding box is the same for all shapes, so only the old region needs
    /// repainting — the new shape draws into the same bbox).
    pub fn cycle_shape(&mut self) -> ModeEffect {
        let current = self.shape as usize;
        let next = (current + 1) % SpotlightShape::ALL.len();
        self.shape = SpotlightShape::ALL[next];
        // Repaint the current hole region — the bbox is the same for all shapes.
        ModeEffect::repaint(
            self.cursor_monitor,
            Some(circle_bbox(self.cursor, self.radius)),
        )
    }

    /// Tracks the cursor; requests a repaint of the hole's old + new regions.
    pub fn on_mouse_move(&mut self, monitor: usize, at: Point) -> ModeEffect {
        if monitor == self.cursor_monitor && at == self.cursor {
            return ModeEffect::none();
        }
        let old = (self.cursor_monitor, circle_bbox(self.cursor, self.radius));
        self.cursor = at;
        self.cursor_monitor = monitor;
        circle_repaint(old, (monitor, circle_bbox(at, self.radius)))
    }

    /// Resizes the radius by the raw wheel delta.
    ///
    /// `delta` is in RAW Win32 wheel units: `120` (wheel up) shrinks the
    /// radius by one step, proportionally (`60` = half a step), clamped to
    /// `10..=1000`. The step scales with the CURRENT radius — see
    /// [`radius_step`] — so the resize curve is tighter when the spotlight
    /// is small and broader when it is large (1 px per notch at the 10 px
    /// minimum, 10 px at the default 150 px radius, 66 px at the 1000 px
    /// maximum). Sub-notch deltas from smooth-scroll hardware are NOT
    /// dropped: they accumulate in `wheel_accum` and each event consumes
    /// only the delta its whole-pixel step accounts for, so a stream of
    /// tiny deltas (e.g. precision-touchpad `+6` ticks) still resizes once
    /// the banked delta reaches a whole pixel. The wheel's cursor position
    /// is tracked too, so a dirty region covers both the old and the new
    /// circle.
    pub fn on_wheel(&mut self, monitor: usize, at: Point, delta: i32) -> ModeEffect {
        // i64 math: delta * step fits easily. Bank the delta in delta×step
        // units at the CURRENT radius's step, then convert the bank to whole
        // pixels (truncating toward zero); the truncation remainder stays
        // banked for the next event. The step only changes when whole pixels
        // are applied, so a banked remainder is always worth less than 1 px
        // — the accumulator can never grow unbounded or drift.
        self.wheel_accum += delta as i64 * radius_step(self.radius);
        // Positive wheel deltas mean wheel up, which makes the spotlight
        // smaller. Negative deltas make it larger.
        let px = (self.wheel_accum / WHEEL_DELTA) as i32;
        let step = -px;
        if px != 0 {
            self.wheel_accum -= px as i64 * WHEEL_DELTA;
        }
        let new_radius =
            (self.radius as i32 + step).clamp(MIN_RADIUS as i32, MAX_RADIUS as i32) as u32;
        if new_radius == self.radius && monitor == self.cursor_monitor && at == self.cursor {
            return ModeEffect::none();
        }
        let old = (self.cursor_monitor, circle_bbox(self.cursor, self.radius));
        self.cursor = at;
        self.cursor_monitor = monitor;
        self.radius = new_radius;
        circle_repaint(old, (monitor, circle_bbox(at, new_radius)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- construction / state ------------------------------------------

    #[test]
    fn new_clamps_default_radius() {
        assert_eq!(
            SpotlightMode::new(5, SpotlightShape::Circle).radius(),
            MIN_RADIUS
        );
        assert_eq!(
            SpotlightMode::new(5000, SpotlightShape::Circle).radius(),
            MAX_RADIUS
        );
        assert_eq!(
            SpotlightMode::new(100, SpotlightShape::Circle).radius(),
            100
        );
    }

    #[test]
    fn new_starts_cursor_at_origin_monitor_zero() {
        let m = SpotlightMode::new(100, SpotlightShape::Circle);
        assert_eq!(m.cursor(), Point::new(0, 0));
        assert_eq!(m.cursor_monitor(), 0);
    }

    // ---- shape cycling ------------------------------------------------------

    #[test]
    fn is_last_shape_true_for_rectangle() {
        assert!(SpotlightMode::new(100, SpotlightShape::Rectangle).is_last_shape());
        assert!(!SpotlightMode::new(100, SpotlightShape::RoundedRect).is_last_shape());
        assert!(!SpotlightMode::new(100, SpotlightShape::Star).is_last_shape());
        assert!(!SpotlightMode::new(100, SpotlightShape::Circle).is_last_shape());
        assert!(!SpotlightMode::new(100, SpotlightShape::Diamond).is_last_shape());
    }

    #[test]
    fn cycle_shape_advances_and_wraps() {
        let mut m = SpotlightMode::new(100, SpotlightShape::Circle);
        assert_eq!(m.shape(), SpotlightShape::Circle);
        m.cycle_shape();
        assert_eq!(m.shape(), SpotlightShape::Diamond);
        m.cycle_shape();
        assert_eq!(m.shape(), SpotlightShape::Star);
        m.cycle_shape();
        assert_eq!(m.shape(), SpotlightShape::RoundedRect);
        m.cycle_shape();
        assert_eq!(m.shape(), SpotlightShape::Rectangle);
        m.cycle_shape();
        assert_eq!(m.shape(), SpotlightShape::Circle, "wraps around");
    }

    #[test]
    fn cycle_shape_preserves_radius_and_cursor() {
        let mut m = SpotlightMode::new(50, SpotlightShape::Circle);
        m.on_mouse_move(1, Point::new(100, 200));
        m.on_wheel(1, Point::new(100, 200), 120); // radius 47 (step 3 at r=50)
        assert_eq!(m.radius(), 47);
        assert_eq!(m.cursor(), Point::new(100, 200));
        assert_eq!(m.cursor_monitor(), 1);

        m.cycle_shape();
        assert_eq!(m.shape(), SpotlightShape::Diamond);
        assert_eq!(m.radius(), 47, "radius preserved");
        assert_eq!(m.cursor(), Point::new(100, 200), "cursor preserved");
        assert_eq!(m.cursor_monitor(), 1, "monitor preserved");
    }

    // ---- mouse move ------------------------------------------------------

    #[test]
    fn mouse_move_same_position_is_noop() {
        let mut m = SpotlightMode::new(50, SpotlightShape::Circle);
        assert_eq!(m.on_mouse_move(0, Point::new(0, 0)), ModeEffect::none());
    }

    #[test]
    fn mouse_move_dirty_is_union_of_old_and_new_circle() {
        let mut m = SpotlightMode::new(50, SpotlightShape::Circle);
        // First move from the default (0,0) to (100,100): union of both
        // radius-50 circle bboxes = [-50,-50 .. 151,151).
        let e = m.on_mouse_move(0, Point::new(100, 100));
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(-50, -50, 201, 201)))],);
        // Second move to (110,100): union of circles at (100,100) and (110,100).
        let e = m.on_mouse_move(0, Point::new(110, 100));
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(50, 50, 111, 101)))],);
    }

    #[test]
    fn mouse_move_to_other_monitor_repaints_both() {
        let mut m = SpotlightMode::new(20, SpotlightShape::Circle);
        m.on_mouse_move(0, Point::new(100, 100));
        let e = m.on_mouse_move(1, Point::new(30, 40));
        assert_eq!(
            e.repaint,
            vec![
                (0, Some(Rect::new(80, 80, 41, 41))),
                (1, Some(Rect::new(10, 20, 41, 41))),
            ],
        );
        assert_eq!(m.cursor_monitor(), 1);
        assert_eq!(m.cursor(), Point::new(30, 40));
    }

    // ---- wheel -----------------------------------------------------------

    #[test]
    fn wheel_step_is_tighter_when_small_and_broader_when_big() {
        // The resize curve: the per-notch step scales with the current
        // radius — fine control for small spotlights, coarse for big ones.
        assert_eq!(radius_step(MIN_RADIUS), 1, "tightest: 1 px per notch");
        assert_eq!(radius_step(30), 2);
        assert_eq!(radius_step(100), 6);
        assert_eq!(
            radius_step(150),
            10,
            "default radius keeps the historical 10 px step"
        );
        assert_eq!(radius_step(500), 33);
        assert_eq!(radius_step(MAX_RADIUS), 66, "broadest at the maximum");
    }

    #[test]
    fn wheel_one_notch_at_the_default_radius_is_10px() {
        // r=160 and r=150 share the 10 px step band, so the round trip is
        // exact: one notch (120 raw) = 10 px, matching the historical fixed
        // step at the default radius.
        let mut m = SpotlightMode::new(160, SpotlightShape::Circle);
        // Union of the r=160 and r=150 circle bboxes at (0,0).
        let e = m.on_wheel(0, Point::new(0, 0), 120);
        assert_eq!(m.radius(), 150);
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(-160, -160, 321, 321)))]);
        let e = m.on_wheel(0, Point::new(0, 0), -120);
        assert_eq!(m.radius(), 160);
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(-160, -160, 321, 321)))]);
    }

    #[test]
    fn wheel_multi_notch_and_fine_delta_scale_proportionally() {
        let mut m = SpotlightMode::new(100, SpotlightShape::Circle);
        // Step at r=100 is 6 px per notch: 240 raw = two notches = -12.
        m.on_wheel(0, Point::new(0, 0), 240);
        assert_eq!(m.radius(), 88);
        // Step at r=88 is 5: +60 raw = half a notch = -2 (remainder banked).
        m.on_wheel(0, Point::new(0, 0), 60);
        assert_eq!(m.radius(), 86);
        // -60 at r=86 (still step 5) cancels the +60 exactly.
        m.on_wheel(0, Point::new(0, 0), -60);
        assert_eq!(m.radius(), 88);
    }

    #[test]
    fn wheel_sub_notch_deltas_still_resize() {
        // D2 regression: precision touchpads send sub-notch deltas (|delta| <
        // 120). At r=100 (step 6) four +60 events MUST change the radius
        // (-3 px each: 60 raw = half a notch).
        let mut m = SpotlightMode::new(100, SpotlightShape::Circle);
        for _ in 0..4 {
            m.on_wheel(0, Point::new(0, 0), 60);
        }
        assert_eq!(m.radius(), 88, "four +60 deltas = half a notch each");
        // And downwards: the step follows the CURRENT radius (5 at r=88,
        // then 6 again from r=90), so the way back covers different ground —
        // half-notch remainders bank and are never dropped.
        for _ in 0..4 {
            m.on_wheel(0, Point::new(0, 0), -60);
        }
        assert_eq!(m.radius(), 99);
    }

    #[test]
    fn wheel_tiny_deltas_accumulate_to_whole_pixels() {
        // D2 regression: very fine deltas below one pixel per event must NOT
        // be dropped — they bank until a whole pixel exists. At r=100 the
        // step is 6, so +6 raw = 0.3 px.
        let mut m = SpotlightMode::new(100, SpotlightShape::Circle);
        m.on_wheel(0, Point::new(0, 0), 6);
        assert_eq!(m.radius(), 100, "first +6 banks 0.3 px: no change yet");
        m.on_wheel(0, Point::new(0, 0), 6);
        assert_eq!(m.radius(), 100, "two +6 events = 0.6 px: still banked");
        m.on_wheel(0, Point::new(0, 0), 6);
        m.on_wheel(0, Point::new(0, 0), 6);
        assert_eq!(m.radius(), 99, "four +6 events = 1.2 px: one whole pixel");
        // Twenty +6 events total = 120 raw = one notch = -6 px at step 6.
        for _ in 0..16 {
            m.on_wheel(0, Point::new(0, 0), 6);
        }
        assert_eq!(m.radius(), 94);
    }

    #[test]
    fn wheel_remainder_carries_across_events_without_drift() {
        // +130 twice at r=100 (step 6): 260 raw × 6 = 13 px; Bresenham
        // banking yields exactly 13 px split 6 + 7 (the truncation remainder
        // is never lost).
        let mut m = SpotlightMode::new(100, SpotlightShape::Circle);
        m.on_wheel(0, Point::new(0, 0), 130);
        assert_eq!(m.radius(), 94);
        m.on_wheel(0, Point::new(0, 0), 130);
        assert_eq!(m.radius(), 87);
        // A full notch immediately after, at r=87 (step 5), yields exactly
        // -5 (no residue distortion).
        m.on_wheel(0, Point::new(0, 0), 120);
        assert_eq!(m.radius(), 82);
        // Direction reversal within the same step band is symmetric: ±60 at
        // r=100 (step 6; r=97 stays in the band) cancel exactly.
        let mut m = SpotlightMode::new(100, SpotlightShape::Circle);
        m.on_wheel(0, Point::new(0, 0), 60);
        m.on_wheel(0, Point::new(0, 0), -60);
        assert_eq!(m.radius(), 100);
    }

    #[test]
    fn wheel_clamps_at_min_and_max() {
        let mut m = SpotlightMode::new(MIN_RADIUS, SpotlightShape::Circle);
        let e = m.on_wheel(0, Point::new(0, 0), 120);
        assert_eq!(m.radius(), MIN_RADIUS);
        assert_eq!(e, ModeEffect::none()); // clamped: nothing changed

        let mut m = SpotlightMode::new(MAX_RADIUS, SpotlightShape::Circle);
        let e = m.on_wheel(0, Point::new(0, 0), -120);
        assert_eq!(m.radius(), MAX_RADIUS);
        assert_eq!(e, ModeEffect::none());

        // A huge delta lands exactly on the clamp, not past it.
        let mut m = SpotlightMode::new(100, SpotlightShape::Circle);
        m.on_wheel(0, Point::new(0, 0), -120 * 1000);
        assert_eq!(m.radius(), MAX_RADIUS);
        m.on_wheel(0, Point::new(0, 0), 120 * 1000);
        assert_eq!(m.radius(), MIN_RADIUS);
    }

    #[test]
    fn wheel_tracks_cursor_and_covers_both_circles() {
        let mut m = SpotlightMode::new(50, SpotlightShape::Circle);
        m.on_mouse_move(0, Point::new(200, 200));
        // Wheel at a different position: cursor follows the wheel event.
        // Step at r=50 is 3, so one notch shrinks to r=47.
        let e = m.on_wheel(0, Point::new(100, 100), 120);
        // Old: circle r=50 at (200,200); new: r=47 at (100,100).
        // Union: x/y from the new bbox (53,53), right/bottom from the old (251,251).
        assert_eq!(e.repaint, vec![(0, Some(Rect::new(53, 53, 198, 198)))],);
    }
}
