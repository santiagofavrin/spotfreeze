//! Native AppKit settings editor.
//!
//! The window is NON-modal: `open()` creates it and returns immediately, and
//! the outcome travels back through the `on_done` callback exactly once —
//! `Some(settings)` after a successful Save, `None` on Cancel or the close
//! button. (The previous design used `runModalForWindow`, whose loop can end
//! before the user acts, leaving Save/Cancel calling `stopModal` into a dead
//! loop while the window lingered; delivering through the window's own close
//! cycle has no such failure mode.)
//!
//! Hotkey rows are rebound BY PRESSING the new combination: each row pairs a
//! read-only binding field with **Set** (arms a key capture backed by a local
//! `NSEvent` monitor — the next non-modifier key chord becomes the binding,
//! bare `Esc` cancels) and **Default** (restores the row's factory binding).
//! The zoom-modifier row captures the same way, except that a modifier-only
//! chord ends the capture once every modifier has been released.
//!
//! The validation code in this module deliberately has no AppKit dependency.
//! This keeps malformed fields and conflicting bindings testable on every
//! platform; the window below is only an editor for those fields.

use crate::geometry::SpotlightShape;
use crate::hotkeys::gesture::{HotkeyGesture, Modifiers, is_modifier_vk};
use crate::hotkeys::keymap;
use crate::settings::model::{AppSettings, Rgb};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, ProtocolObject, Sel};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSApplication, NSButton, NSButtonType, NSControlTextEditingDelegate, NSEvent,
    NSEventMask, NSEventModifierFlags, NSEventType, NSFont, NSTextField, NSTextFieldDelegate,
    NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask,
};
use objc2_foundation::{
    NSNotification, NSObjectNSThreadPerformAdditions, NSObjectProtocol, NSPoint, NSRect, NSSize,
    NSString,
};
use std::cell::{Cell, RefCell};
use std::ptr::NonNull;

const RADIUS_MIN: u32 = 10;
const RADIUS_MAX: u32 = 2000;
const ZOOM_STEP_MAX: f32 = 4.0;
const ZOOM_MAX_LIMIT: f32 = 64.0;

/// Win32 `VK_ESCAPE` — the cancel key of an armed capture.
const VK_ESCAPE: u32 = 0x1B;

const WINDOW_W: f64 = 520.0;
const WINDOW_H: f64 = 640.0;
const LABEL_X: f64 = 28.0;
const LABEL_W: f64 = 195.0;
const FIELD_X: f64 = 228.0;
const BINDING_FIELD_W: f64 = 118.0;
const PLAIN_FIELD_W: f64 = 246.0;
const SET_X: f64 = 354.0;
const SET_W: f64 = 46.0;
const DEFAULT_X: f64 = 406.0;
const DEFAULT_W: f64 = 68.0;
const ROW_H: f64 = 20.0;
const ROW_PITCH: f64 = 23.0;
const HEADING_H: f64 = 20.0;
const TOP_Y: f64 = 585.0;

/// Binding rows (Set/Default buttons): the six full gestures plus the
/// modifier-only zoom chord.
const BINDING_ROW_COUNT: usize = 7;
const ZOOM_MODIFIER_ROW: usize = 6;
const FIELD_COUNT: usize = 15;
const ROW_LABELS: [&str; FIELD_COUNT] = [
    "Freeze toggle",
    "Spotlight",
    "Capture",
    "Copy / enter capture",
    "Copy / unfreeze",
    "Reset zoom",
    "Zoom modifier",
    "Default radius",
    "Step factor",
    "Minimum zoom",
    "Maximum zoom",
    "Dim opacity",
    "Capture opacity",
    "Overlay color",
    "Capture color",
];
/// Section headings and the row index each heading sits above.
const SECTION_HEADS: [(usize, &str); 5] = [
    (0, "Hotkeys"),
    (7, "Spotlight"),
    (8, "Zoom"),
    (11, "Overlay"),
    (14, "General"),
];

#[derive(Clone, Debug)]
struct Fields {
    gestures: [String; 6],
    zoom_modifier: String,
    radius: String,
    step_factor: String,
    zoom_min: String,
    zoom_max: String,
    dim_opacity: String,
    snip_dim_opacity: String,
    color: String,
    snip_color: String,
    auto_start: bool,
    show_legend: bool,
    /// Not edited by the window: carried through so a save preserves it.
    shape: SpotlightShape,
}

fn fields_from_settings(s: &AppSettings) -> Fields {
    Fields {
        gestures: [
            s.hotkeys.freeze_toggle.to_display(),
            s.hotkeys.mode_spotlight.to_display(),
            s.hotkeys.mode_snip.to_display(),
            s.hotkeys.snip_copy.to_display(),
            s.hotkeys.cancel.to_display(),
            s.hotkeys.reset_zoom.to_display(),
        ],
        zoom_modifier: s.hotkeys.zoom_modifier.to_display(),
        radius: s.spotlight.default_radius.to_string(),
        step_factor: s.zoom.step_factor.to_string(),
        zoom_min: s.zoom.min.to_string(),
        zoom_max: s.zoom.max.to_string(),
        dim_opacity: s.overlay.dim_opacity.to_string(),
        snip_dim_opacity: s.overlay.snip_dim_opacity.to_string(),
        color: s.overlay.color.to_hex(),
        snip_color: s.overlay.snip_color.to_hex(),
        auto_start: s.auto_start,
        show_legend: s.overlay.show_legend,
        shape: s.spotlight.shape,
    }
}

/// The factory binding text of one binding row (`0..=6`).
fn default_binding_text(row: usize) -> String {
    let d = crate::settings::model::HotkeySettings::default();
    match row {
        0 => d.freeze_toggle.to_display(),
        1 => d.mode_spotlight.to_display(),
        2 => d.mode_snip.to_display(),
        3 => d.snip_copy.to_display(),
        4 => d.cancel.to_display(),
        5 => d.reset_zoom.to_display(),
        _ => d.zoom_modifier.to_display(),
    }
}

/// Parse and validate all values edited by the window.
///
/// This is intentionally a primitive-string boundary: it is also the
/// contract tested by the headless unit tests below.
fn validate_fields(f: &Fields) -> Result<AppSettings, Vec<String>> {
    let mut errors = Vec::new();
    let mut gestures = Vec::with_capacity(6);
    for (index, text) in f.gestures.iter().enumerate() {
        match HotkeyGesture::parse(text) {
            Ok(g) if g.is_registerable() => gestures.push(g),
            Ok(_) => errors.push(format!("Hotkey {} is not registerable", index + 1)),
            Err(e) => errors.push(format!("Hotkey {}: {e}", index + 1)),
        }
    }
    for i in 0..gestures.len() {
        for j in (i + 1)..gestures.len() {
            if gestures[i] == gestures[j] {
                errors.push(format!(
                    "Hotkey {} conflicts with hotkey {} ({})",
                    i + 1,
                    j + 1,
                    gestures[i].to_display()
                ));
            }
        }
    }
    let zoom_modifier = match Modifiers::parse(&f.zoom_modifier) {
        Ok(m) if !m.is_empty() => Some(m),
        Ok(_) => {
            errors.push("Zoom modifier must contain at least one modifier".into());
            None
        }
        Err(e) => {
            errors.push(format!("Zoom modifier: {e}"));
            None
        }
    };

    let radius = parse_u32(
        "Spotlight radius",
        &f.radius,
        RADIUS_MIN,
        RADIUS_MAX,
        &mut errors,
    );
    let step = parse_f32(
        "Zoom step factor",
        &f.step_factor,
        1.0,
        true,
        ZOOM_STEP_MAX,
        &mut errors,
    );
    let min = parse_f32(
        "Zoom minimum",
        &f.zoom_min,
        1.0,
        false,
        ZOOM_MAX_LIMIT,
        &mut errors,
    );
    let max = parse_f32(
        "Zoom maximum",
        &f.zoom_max,
        1.0,
        false,
        ZOOM_MAX_LIMIT,
        &mut errors,
    );
    if let (Some(min), Some(max)) = (min, max)
        && min >= max
    {
        errors.push("Zoom minimum must be smaller than zoom maximum".into());
    }
    let dim = parse_u8("Overlay opacity", &f.dim_opacity, &mut errors);
    let snip_dim = parse_u8("Capture opacity", &f.snip_dim_opacity, &mut errors);
    let color = parse_color("Overlay color", &f.color, &mut errors);
    let snip_color = parse_color("Capture color", &f.snip_color, &mut errors);

    if !errors.is_empty() || gestures.len() != 6 || zoom_modifier.is_none() {
        return Err(errors);
    }
    let h = &mut AppSettings::default().hotkeys;
    [
        &mut h.freeze_toggle,
        &mut h.mode_spotlight,
        &mut h.mode_snip,
        &mut h.snip_copy,
        &mut h.cancel,
        &mut h.reset_zoom,
    ]
    .into_iter()
    .zip(gestures)
    .for_each(|(slot, value)| *slot = value);
    h.zoom_modifier = zoom_modifier.unwrap();
    Ok(AppSettings {
        hotkeys: h.clone(),
        spotlight: crate::settings::model::SpotlightSettings {
            default_radius: radius.unwrap(),
            shape: f.shape,
        },
        zoom: crate::settings::model::ZoomSettings {
            step_factor: step.unwrap(),
            min: min.unwrap(),
            max: max.unwrap(),
        },
        overlay: crate::settings::model::OverlaySettings {
            dim_opacity: dim.unwrap(),
            color: color.unwrap(),
            snip_dim_opacity: snip_dim.unwrap(),
            snip_color: snip_color.unwrap(),
            show_legend: f.show_legend,
        },
        auto_start: f.auto_start,
    })
}

fn parse_u32(label: &str, text: &str, min: u32, max: u32, errors: &mut Vec<String>) -> Option<u32> {
    match text.trim().parse::<u32>() {
        Ok(v) if (min..=max).contains(&v) => Some(v),
        Ok(_) => {
            errors.push(format!("{label} must be between {min} and {max}"));
            None
        }
        Err(_) => {
            errors.push(format!("{label} must be a whole number"));
            None
        }
    }
}

fn parse_u8(label: &str, text: &str, errors: &mut Vec<String>) -> Option<u8> {
    match text.trim().parse::<u8>() {
        Ok(v) => Some(v),
        Err(_) => {
            errors.push(format!("{label} must be between 0 and 255"));
            None
        }
    }
}

fn parse_f32(
    label: &str,
    text: &str,
    min: f32,
    exclusive: bool,
    max: f32,
    errors: &mut Vec<String>,
) -> Option<f32> {
    match text.trim().parse::<f32>() {
        Ok(v) if v.is_finite() && (if exclusive { v > min } else { v >= min }) && v <= max => {
            Some(v)
        }
        _ => {
            errors.push(format!("{label} must be finite and in the allowed range"));
            None
        }
    }
}

fn parse_color(label: &str, text: &str, errors: &mut Vec<String>) -> Option<Rgb> {
    match Rgb::parse_hex(text.trim()) {
        Ok(c) => Some(c),
        Err(e) => {
            errors.push(format!("{label}: {e}"));
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Pure capture-decision logic (no AppKit) — unit-tested at the bottom.
// ---------------------------------------------------------------------------

/// What one key event means for an armed gesture-row capture.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GestureCaptureDecision {
    /// A modifier going down, or an unmapped key: keep waiting.
    KeepCapturing,
    /// Bare `Esc`: abort, keep the previous binding.
    Cancel,
    /// A non-modifier key went down: adopt this chord as the new binding.
    Bind(HotkeyGesture),
}

/// Decide an armed gesture-row capture from one key-down event.
///
/// * Bare modifier key-downs never decide anything — the capture stays armed
///   so chords (`Cmd+Shift+P`, …) build up in `modifiers`.
/// * `Esc` with NO modifiers cancels; `Esc` WITH modifiers is a legitimate
///   binding.
/// * Anything else binds `modifiers + key` verbatim.
fn decide_gesture_capture(vk: u32, modifiers: Modifiers) -> GestureCaptureDecision {
    if is_modifier_vk(vk) {
        return GestureCaptureDecision::KeepCapturing;
    }
    if vk == VK_ESCAPE && modifiers.is_empty() {
        return GestureCaptureDecision::Cancel;
    }
    GestureCaptureDecision::Bind(HotkeyGesture::new(modifiers, vk))
}

/// Decide an armed zoom-modifier capture from one modifier-state change.
///
/// The capture accumulates every modifier seen while armed (`seen`) and binds
/// the moment the keyboard returns to a fully unmodified state — but only if
/// at least one modifier was actually pressed. Returns `None` to keep
/// capturing.
fn decide_modifier_capture(seen: Modifiers, current: Modifiers) -> Option<Modifiers> {
    if current.is_empty() && !seen.is_empty() {
        Some(seen)
    } else {
        None
    }
}

/// The final text an armed capture leaves in its row's field.
#[derive(Clone, Debug, PartialEq)]
enum CaptureOutcome {
    /// Restore the pre-capture binding (bare `Esc`, or abandoned).
    Cancel,
    Gesture(HotkeyGesture),
    Modifiers(Modifiers),
}

/// The live capture of one binding row, owned by the thread-local
/// [`CAPTURE`] slot. At most one capture is armed at a time.
struct Capture {
    /// The row's field index (`0..=6`).
    row: usize,
    /// The binding text shown before the capture armed — restored on cancel.
    previous: String,
    /// The local event monitor; removed exactly once when the capture ends.
    monitor: Retained<AnyObject>,
    /// Set once the monitor reached a decision; the field text is finalized
    /// by the deferred `captureFinished:`.
    decided: Option<CaptureOutcome>,
    /// Every modifier seen while armed (zoom-modifier capture only).
    seen: Modifiers,
}

/// The completion callback: `Some(settings)` after a successful Save,
/// `None` on Cancel or the close button.
type OnDone = Box<dyn FnOnce(Option<AppSettings>)>;

thread_local! {
    /// The open settings window, if any. Keeps the target (and through its
    /// ivars the window) alive from `open()` until the close-cycle teardown.
    static SESSION: RefCell<Option<Retained<SettingsTarget>>> = const { RefCell::new(None) };
    /// The armed binding capture, if any.
    static CAPTURE: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

struct WindowIvars {
    window: Retained<NSWindow>,
    /// All [`FIELD_COUNT`] value fields, in row order.
    fields: Vec<Retained<NSTextField>>,
    auto_start: Retained<NSButton>,
    show_legend: Retained<NSButton>,
    on_done: RefCell<Option<OnDone>>,
    /// The Save result, consumed exactly once by `teardown:`.
    result: RefCell<Option<AppSettings>>,
    /// Not edited by the window: carried through so a save preserves it.
    shape: SpotlightShape,
    /// Guards the one-shot `teardown:`.
    torn_down: Cell<bool>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpotFreezeSettingsTarget"]
    #[ivars = WindowIvars]
    struct SettingsTarget;

    impl SettingsTarget {
        #[unsafe(method(save:))]
        fn save(&self, _sender: &AnyObject) {
            end_capture();
            let values: Vec<String> = self
                .ivars()
                .fields
                .iter()
                .map(|f| f.stringValue().to_string())
                .collect();
            let f = Fields {
                gestures: values[..6].to_vec().try_into().unwrap(),
                zoom_modifier: values[6].clone(),
                radius: values[7].clone(),
                step_factor: values[8].clone(),
                zoom_min: values[9].clone(),
                zoom_max: values[10].clone(),
                dim_opacity: values[11].clone(),
                snip_dim_opacity: values[12].clone(),
                color: values[13].clone(),
                snip_color: values[14].clone(),
                auto_start: {
                    let state: isize = unsafe { msg_send![&*self.ivars().auto_start, state] };
                    state == 1
                },
                show_legend: {
                    let state: isize = unsafe { msg_send![&*self.ivars().show_legend, state] };
                    state == 1
                },
                shape: self.ivars().shape,
            };
            match validate_fields(&f) {
                Ok(settings) => {
                    *self.ivars().result.borrow_mut() = Some(settings);
                    self.close_window();
                }
                Err(errors) => {
                    let alert = NSAlert::new(self.mtm());
                    alert.setMessageText(&NSString::from_str("Check your settings"));
                    alert.setInformativeText(&NSString::from_str(&errors[0]));
                    let _ = alert.addButtonWithTitle(&NSString::from_str("OK"));
                    alert.window().setLevel(1001);
                    alert.runModal();
                }
            }
        }

        #[unsafe(method(cancel:))]
        fn cancel(&self, _sender: &AnyObject) {
            end_capture();
            self.close_window();
        }

        /// "Set" button of a binding row: arm the capture for row = tag.
        #[unsafe(method(setBinding:))]
        fn set_binding(&self, sender: &AnyObject) {
            let row = button_tag(sender) as usize;
            end_capture();
            arm_capture(row);
        }

        /// "Default" button of a binding row: restore row = tag's factory
        /// binding.
        #[unsafe(method(defaultBinding:))]
        fn default_binding(&self, sender: &AnyObject) {
            let row = button_tag(sender) as usize;
            end_capture();
            self.ivars().fields[row]
                .setStringValue(&NSString::from_str(&default_binding_text(row)));
        }

        /// Deferred end of a capture whose monitor reached a decision:
        /// remove the monitor and finalize the field text.
        #[unsafe(method(captureFinished:))]
        fn capture_finished(&self, _sender: &AnyObject) {
            let Some(capture) = CAPTURE.with(|c| c.borrow_mut().take()) else {
                return;
            };
            // SAFETY: the token came from addLocalMonitorForEventsMatchingMask
            // and is removed exactly once — the CAPTURE slot hands it to
            // exactly one of capture_finished / end_capture.
            unsafe { NSEvent::removeMonitor(&capture.monitor) };
            let text = match capture.decided {
                Some(CaptureOutcome::Gesture(gesture)) => gesture.to_display(),
                Some(CaptureOutcome::Modifiers(modifiers)) => modifiers.to_display(),
                Some(CaptureOutcome::Cancel) | None => capture.previous,
            };
            self.ivars().fields[capture.row]
                .setStringValue(&NSString::from_str(&text));
        }

        /// Deferred single-shot teardown after the window closed: deliver the
        /// outcome, then drop the session (releasing target and window).
        #[unsafe(method(teardown:))]
        fn teardown(&self, _sender: &AnyObject) {
            if self.ivars().torn_down.replace(true) {
                return;
            }
            let outcome = self.ivars().result.borrow_mut().take();
            let on_done = self.ivars().on_done.borrow_mut().take();
            SESSION.with(|s| {
                *s.borrow_mut() = None;
            });
            if let Some(on_done) = on_done {
                on_done(outcome);
            }
        }
    }

    unsafe impl NSObjectProtocol for SettingsTarget {}

    unsafe impl NSWindowDelegate for SettingsTarget {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            end_capture();
            // Deferred so the session is never dropped from inside the close
            // cycle itself.
            // SAFETY: `teardown:` takes no argument; AppKit callbacks and
            // local monitors are main-thread only, and this enqueues onto
            // the main run loop.
            unsafe {
                self.performSelectorOnMainThread_withObject_waitUntilDone(
                    sel!(teardown:),
                    None,
                    false,
                );
            }
        }

        /// Losing key status (app switch, another window) disarms an armed
        /// capture so the keyboard is never swallowed for the whole app.
        #[unsafe(method(windowDidResignKey:))]
        fn window_did_resign_key(&self, _notification: &NSNotification) {
            end_capture();
        }
    }

    unsafe impl NSControlTextEditingDelegate for SettingsTarget {
        /// Focusing a value field abandons an armed capture (parity with the
        /// Windows shell's EN_SETFOCUS disarm), so typing into it is never
        /// swallowed as a potential binding.
        #[unsafe(method(controlTextDidBeginEditing:))]
        fn control_text_did_begin_editing(&self, _notification: &NSNotification) {
            end_capture();
        }
    }

    unsafe impl NSTextFieldDelegate for SettingsTarget {}
);

impl SettingsTarget {
    fn new(mtm: MainThreadMarker, ivars: WindowIvars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        unsafe { msg_send![super(this), init] }
    }

    fn close_window(&self) {
        // close() runs windowWillClose, which schedules teardown.
        self.ivars().window.close();
    }
}

fn button_tag(sender: &AnyObject) -> isize {
    // SAFETY: every sender of setBinding:/defaultBinding: is one of our
    // NSButtons, which respond to `tag`.
    unsafe { msg_send![sender, tag] }
}

/// Disarm an armed capture without adopting a binding: remove the monitor
/// and restore the row's previous text. No-op when nothing is armed.
fn end_capture() {
    let Some(capture) = CAPTURE.with(|c| c.borrow_mut().take()) else {
        return;
    };
    // SAFETY: see capture_finished.
    unsafe { NSEvent::removeMonitor(&capture.monitor) };
    if let Some(target) = SESSION.with(|s| s.borrow().clone()) {
        target.ivars().fields[capture.row].setStringValue(&NSString::from_str(&capture.previous));
    }
}

/// Arm the capture for binding row `row` (`0..=6`). On monitor-install
/// failure the row simply keeps its previous binding.
fn arm_capture(row: usize) {
    let Some(target) = SESSION.with(|s| s.borrow().clone()) else {
        return;
    };
    let previous = target.ivars().fields[row].stringValue().to_string();
    let Some(monitor) = install_capture_monitor() else {
        return;
    };
    CAPTURE.with(|c| {
        *c.borrow_mut() = Some(Capture {
            row,
            previous,
            monitor,
            decided: None,
            seen: Modifiers::NONE,
        });
    });
    target.ivars().fields[row].setStringValue(&NSString::from_str("Press keys…"));
}

/// Install the local monitor of an armed capture. While armed it swallows
/// every key event destined for this app (see [`handle_capture_event`]).
fn install_capture_monitor() -> Option<Retained<AnyObject>> {
    let block = RcBlock::new(move |event: NonNull<NSEvent>| -> *mut NSEvent {
        // SAFETY: the event is valid for the duration of the callback.
        let event = unsafe { event.as_ref() };
        handle_capture_event(event)
    });
    let mask = NSEventMask::KeyDown | NSEventMask::FlagsChanged;
    // SAFETY: the block matches the documented local-monitor signature and
    // is retained by AppKit until removeMonitor ends the capture. The
    // nullable return reflects a failed monitor installation.
    unsafe { NSEvent::addLocalMonitorForEventsMatchingMask_handler(mask, &block) }
}

/// One monitored key event: pass it through, or swallow it (`null`) while a
/// capture is armed. When the event decides the capture, the field update
/// and monitor removal are deferred to `captureFinished:` — a monitor must
/// not remove itself from inside its own callback.
fn handle_capture_event(event: &NSEvent) -> *mut NSEvent {
    let event_type = event.r#type();
    if event_type != NSEventType::KeyDown && event_type != NSEventType::FlagsChanged {
        return NonNull::from(event).as_ptr();
    }
    let modifiers = modifiers_from(event.modifierFlags());
    let mut swallow = false;
    let mut decided = false;
    let mut target: Option<Retained<SettingsTarget>> = None;
    CAPTURE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let Some(capture) = slot.as_mut() else {
            return;
        };
        target = SESSION.with(|s| s.borrow().clone());
        if capture.decided.is_some() {
            return; // decided; pass through until the deferred removal
        }
        swallow = true;
        let vk = keymap::cg_keycode_to_vk(event.keyCode());
        let outcome = if capture.row == ZOOM_MODIFIER_ROW {
            match event_type {
                // A modifier-only binding has no non-modifier key: any
                // key-down other than the cancel Esc is ignored.
                NSEventType::KeyDown if vk == Some(VK_ESCAPE) && modifiers.is_empty() => {
                    Some(CaptureOutcome::Cancel)
                }
                NSEventType::FlagsChanged => {
                    capture.seen = capture.seen | modifiers;
                    decide_modifier_capture(capture.seen, modifiers).map(CaptureOutcome::Modifiers)
                }
                _ => None,
            }
        } else {
            match (event_type, vk) {
                (NSEventType::KeyDown, Some(vk)) => match decide_gesture_capture(vk, modifiers) {
                    GestureCaptureDecision::KeepCapturing => None,
                    GestureCaptureDecision::Cancel => Some(CaptureOutcome::Cancel),
                    GestureCaptureDecision::Bind(gesture) => Some(CaptureOutcome::Gesture(gesture)),
                },
                _ => None,
            }
        };
        if let Some(outcome) = outcome {
            capture.decided = Some(outcome);
            decided = true;
        }
    });
    if decided && let Some(target) = target {
        // SAFETY: standard NSObject API; enqueued to run after this event.
        unsafe {
            target.performSelectorOnMainThread_withObject_waitUntilDone(
                sel!(captureFinished:),
                None,
                false,
            );
        }
    }
    if swallow {
        std::ptr::null_mut()
    } else {
        NonNull::from(event).as_ptr()
    }
}

/// NSEvent modifier flags → the cross-platform [`Modifiers`] set (⌘ maps to
/// the `WIN` slot, matching the gesture syntax shared by every platform).
fn modifiers_from(flags: NSEventModifierFlags) -> Modifiers {
    let mut out = Modifiers::NONE;
    if flags.contains(NSEventModifierFlags::Shift) {
        out = out | Modifiers::SHIFT;
    }
    if flags.contains(NSEventModifierFlags::Control) {
        out = out | Modifiers::CTRL;
    }
    if flags.contains(NSEventModifierFlags::Option) {
        out = out | Modifiers::ALT;
    }
    if flags.contains(NSEventModifierFlags::Command) {
        out = out | Modifiers::WIN;
    }
    out
}

fn field(
    mtm: MainThreadMarker,
    frame: NSRect,
    value: &str,
    editable: bool,
) -> Retained<NSTextField> {
    let f = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
    f.setStringValue(&NSString::from_str(value));
    f.setEditable(editable);
    if !editable {
        f.setSelectable(false);
    }
    f
}

fn label(mtm: MainThreadMarker, frame: NSRect, text: &str) -> Retained<NSTextField> {
    let l = NSTextField::labelWithString(&NSString::from_str(text), mtm);
    l.setFrame(frame);
    l.setFont(Some(&NSFont::systemFontOfSize(13.0)));
    l
}

/// One push button; `tag` identifies the binding row to the action.
fn push_button(
    mtm: MainThreadMarker,
    frame: NSRect,
    title: &str,
    action: Sel,
    tag: isize,
) -> Retained<NSButton> {
    let b = NSButton::initWithFrame(NSButton::alloc(mtm), frame);
    b.setTitle(&NSString::from_str(title));
    b.setTag(tag);
    // SAFETY: the selector is one of this class's own action methods.
    unsafe { b.setAction(Some(action)) };
    b
}

fn checkbox(mtm: MainThreadMarker, y: f64, title: &str, checked: bool) -> Retained<NSButton> {
    let b = NSButton::initWithFrame(
        NSButton::alloc(mtm),
        NSRect {
            origin: NSPoint { x: LABEL_X, y },
            size: NSSize {
                width: 300.0,
                height: 24.0,
            },
        },
    );
    b.setTitle(&NSString::from_str(title));
    b.setButtonType(NSButtonType::Switch);
    b.setState(checked as isize);
    b
}

fn field_value(values: &Fields, row: usize) -> String {
    match row {
        0..=5 => values.gestures[row].clone(),
        6 => values.zoom_modifier.clone(),
        7 => values.radius.clone(),
        8 => values.step_factor.clone(),
        9 => values.zoom_min.clone(),
        10 => values.zoom_max.clone(),
        11 => values.dim_opacity.clone(),
        12 => values.snip_dim_opacity.clone(),
        13 => values.color.clone(),
        _ => values.snip_color.clone(),
    }
}

/// Open the settings editor. Non-blocking: the outcome is delivered exactly
/// once through `on_done` — `Some(settings)` after a successful Save, `None`
/// on Cancel or the close button. Re-opening while a window exists just
/// focuses it.
pub fn open(mtm: MainThreadMarker, current: &AppSettings, on_done: OnDone) {
    if let Some(existing) = SESSION.with(|s| s.borrow().clone()) {
        existing.ivars().window.makeKeyAndOrderFront(None);
        return;
    }
    let values = fields_from_settings(current);
    let rect = NSRect {
        origin: NSPoint { x: 0.0, y: 0.0 },
        size: NSSize {
            width: WINDOW_W,
            height: WINDOW_H,
        },
    };
    let style =
        NSWindowStyleMask::Titled | NSWindowStyleMask::Closable | NSWindowStyleMask::Miniaturizable;
    let window: Retained<NSWindow> = unsafe {
        msg_send![
            NSWindow::alloc(mtm),
            initWithContentRect: rect,
            styleMask: style,
            backing: objc2_app_kit::NSBackingStoreType::Buffered,
            defer: false
        ]
    };
    window.setTitle(&NSString::from_str("SpotFreeze Settings"));
    unsafe { window.setReleasedWhenClosed(false) };

    let effect = NSVisualEffectView::initWithFrame(NSVisualEffectView::alloc(mtm), rect);
    effect.setMaterial(NSVisualEffectMaterial::Sidebar);
    effect.setState(NSVisualEffectState::Active);
    window.setContentView(Some(&effect));

    let mut fields = Vec::with_capacity(FIELD_COUNT);
    let mut row_buttons: Vec<Retained<NSButton>> = Vec::new();
    let mut y = TOP_Y;
    let mut next_head = 0;
    for (row, row_label) in ROW_LABELS.iter().enumerate() {
        if next_head < SECTION_HEADS.len() && SECTION_HEADS[next_head].0 == row {
            let heading = label(
                mtm,
                NSRect {
                    origin: NSPoint { x: LABEL_X, y },
                    size: NSSize {
                        width: WINDOW_W - 2.0 * LABEL_X,
                        height: HEADING_H,
                    },
                },
                SECTION_HEADS[next_head].1,
            );
            heading.setFont(Some(&NSFont::boldSystemFontOfSize(13.0)));
            effect.addSubview(&heading);
            y -= HEADING_H;
            next_head += 1;
        }
        let field_width = if row < BINDING_ROW_COUNT {
            BINDING_FIELD_W
        } else {
            PLAIN_FIELD_W
        };
        let row_field = field(
            mtm,
            NSRect {
                origin: NSPoint { x: FIELD_X, y },
                size: NSSize {
                    width: field_width,
                    height: ROW_H,
                },
            },
            &field_value(&values, row),
            row >= BINDING_ROW_COUNT,
        );
        let row_heading = label(
            mtm,
            NSRect {
                origin: NSPoint { x: LABEL_X, y },
                size: NSSize {
                    width: LABEL_W,
                    height: ROW_H,
                },
            },
            row_label,
        );
        effect.addSubview(&row_heading);
        effect.addSubview(&row_field);
        fields.push(row_field);
        if row < BINDING_ROW_COUNT {
            let set = push_button(
                mtm,
                NSRect {
                    origin: NSPoint { x: SET_X, y },
                    size: NSSize {
                        width: SET_W,
                        height: ROW_H,
                    },
                },
                "Set",
                sel!(setBinding:),
                row as isize,
            );
            let default = push_button(
                mtm,
                NSRect {
                    origin: NSPoint { x: DEFAULT_X, y },
                    size: NSSize {
                        width: DEFAULT_W,
                        height: ROW_H,
                    },
                },
                "Default",
                sel!(defaultBinding:),
                row as isize,
            );
            effect.addSubview(&set);
            effect.addSubview(&default);
            row_buttons.push(set.clone());
            row_buttons.push(default.clone());
        }
        y -= ROW_PITCH;
    }

    let check = checkbox(mtm, 42.0, "Launch at login", values.auto_start);
    let show_legend = checkbox(mtm, 70.0, "Show mode legend", values.show_legend);
    effect.addSubview(&check);
    effect.addSubview(&show_legend);

    let save = push_button(
        mtm,
        NSRect {
            origin: NSPoint { x: 365.0, y: 14.0 },
            size: NSSize {
                width: 115.0,
                height: 28.0,
            },
        },
        "Save",
        sel!(save:),
        0,
    );
    save.setKeyEquivalent(&NSString::from_str("\r"));
    let cancel = push_button(
        mtm,
        NSRect {
            origin: NSPoint { x: 240.0, y: 14.0 },
            size: NSSize {
                width: 115.0,
                height: 28.0,
            },
        },
        "Cancel",
        sel!(cancel:),
        0,
    );
    cancel.setKeyEquivalent(&NSString::from_str("\u{1b}"));
    effect.addSubview(&save);
    effect.addSubview(&cancel);

    let target = SettingsTarget::new(
        mtm,
        WindowIvars {
            window: window.clone(),
            fields,
            auto_start: check,
            show_legend,
            on_done: RefCell::new(Some(on_done)),
            result: RefCell::new(None),
            shape: current.spotlight.shape,
            torn_down: Cell::new(false),
        },
    );
    // SAFETY: the target lives in the session for the window's whole
    // lifetime, outliving every control that references it.
    unsafe {
        for button in &row_buttons {
            button.setTarget(Some(&target));
        }
        for (row, value_field) in target.ivars().fields.iter().enumerate() {
            if row >= BINDING_ROW_COUNT {
                value_field.setDelegate(Some(ProtocolObject::from_ref(&*target)));
            }
        }
        save.setTarget(Some(&target));
        cancel.setTarget(Some(&target));
    }

    window.setDelegate(Some(ProtocolObject::from_ref(&*target)));
    window.center();
    NSApplication::sharedApplication(mtm).activate();
    window.makeKeyAndOrderFront(None);
    SESSION.with(|s| {
        *s.borrow_mut() = Some(target);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_display_round_trips() {
        let original = AppSettings::default();
        assert_eq!(
            validate_fields(&fields_from_settings(&original)).unwrap(),
            original
        );
    }

    #[test]
    fn rejects_bad_hotkey() {
        let mut f = fields_from_settings(&AppSettings::default());
        f.gestures[0] = "not-a-hotkey".into();
        assert!(validate_fields(&f).is_err());
    }

    #[test]
    fn rejects_conflicting_hotkeys() {
        let mut f = fields_from_settings(&AppSettings::default());
        f.gestures[1] = f.gestures[0].clone();
        assert!(validate_fields(&f).is_err());
    }

    #[test]
    fn rejects_invalid_zoom_and_opacity() {
        let mut f = fields_from_settings(&AppSettings::default());
        f.zoom_min = "4".into();
        f.zoom_max = "2".into();
        f.step_factor = "1".into();
        f.dim_opacity = "256".into();
        assert!(validate_fields(&f).is_err());
    }

    #[test]
    fn rejects_bad_colors() {
        let mut f = fields_from_settings(&AppSettings::default());
        f.color = "blue".into();
        f.snip_color = "#12345".into();
        assert!(validate_fields(&f).is_err());
    }

    #[test]
    fn show_legend_round_trips_when_toggled_off() {
        let mut f = fields_from_settings(&AppSettings::default());
        assert!(f.show_legend, "default is true");
        f.show_legend = false;
        let settings = validate_fields(&f).expect("must validate");
        assert!(
            !settings.overlay.show_legend,
            "toggled-off show_legend reaches the settings copy"
        );
    }

    #[test]
    fn gesture_capture_binds_plain_key() {
        assert_eq!(
            decide_gesture_capture('S' as u32, Modifiers::NONE),
            GestureCaptureDecision::Bind(HotkeyGesture::parse("S").unwrap())
        );
    }

    #[test]
    fn gesture_capture_binds_chord_including_escaped_modifier() {
        let chord = Modifiers::CTRL | Modifiers::WIN;
        assert_eq!(
            decide_gesture_capture(VK_ESCAPE, chord),
            GestureCaptureDecision::Bind(HotkeyGesture::new(chord, VK_ESCAPE))
        );
    }

    #[test]
    fn gesture_capture_bare_escape_cancels() {
        assert_eq!(
            decide_gesture_capture(VK_ESCAPE, Modifiers::NONE),
            GestureCaptureDecision::Cancel
        );
    }

    #[test]
    fn gesture_capture_ignores_modifier_keydowns() {
        assert_eq!(
            decide_gesture_capture(0xA0, Modifiers::SHIFT),
            GestureCaptureDecision::KeepCapturing
        );
    }

    #[test]
    fn modifier_capture_waits_for_full_release() {
        assert_eq!(
            decide_modifier_capture(Modifiers::NONE, Modifiers::NONE),
            None
        );
        assert_eq!(
            decide_modifier_capture(Modifiers::NONE, Modifiers::SHIFT),
            None
        );
        assert_eq!(
            decide_modifier_capture(Modifiers::SHIFT, Modifiers::SHIFT),
            None
        );
    }

    #[test]
    fn modifier_capture_binds_max_seen_on_release() {
        assert_eq!(
            decide_modifier_capture(Modifiers::CTRL | Modifiers::SHIFT, Modifiers::NONE),
            Some(Modifiers::CTRL | Modifiers::SHIFT)
        );
    }

    #[test]
    fn default_bindings_round_trip_through_validation() {
        for row in 0..BINDING_ROW_COUNT {
            let mut f = fields_from_settings(&AppSettings::default());
            let text = default_binding_text(row);
            if row == ZOOM_MODIFIER_ROW {
                f.zoom_modifier = text;
            } else {
                f.gestures[row] = text;
            }
            assert!(
                validate_fields(&f).is_ok(),
                "default binding of row {row} must validate"
            );
        }
    }

    #[test]
    fn escape_keycode_maps_to_vk_escape() {
        // CG keycode 53 is the Esc key; the capture logic relies on it
        // mapping to VK_ESCAPE.
        assert_eq!(keymap::cg_keycode_to_vk(53), Some(VK_ESCAPE));
    }
}
