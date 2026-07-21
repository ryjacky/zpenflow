//! Pen-button binding model (OTD-inspired, design.md §6.6).
//!
//! A `Binding` is "what to do when this pen button changes state". The
//! predecessor hardcoded `Ctrl/Shift/E` in `pen_injector.py`; the rewrite
//! makes that the **default profile** so the GUI can later expose binding
//! customization (Wave 4) without touching the engine hot path.
//!
//! Why these specific shapes:
//!   - `KeyTap` for buttons like "press 'E' to toggle eraser" — Krita
//!     consumes a single `keydown/keyup` pair.
//!   - `KeyHold` / `KeyChord` for modifier-style buttons (Ctrl held while
//!     drawing). Predecessor §2.3 #4 explains why we send keyboard events
//!     for modifier buttons rather than mouse clicks: SendInput mouse
//!     clicks get filtered by Windows Ink under concurrent pen contact.
//!   - `EraserToggle` flips the WinRT `Inverted` bit on subsequent pen
//!     samples; design §6.6 calls this out as cleaner than tapping `E`.

use windows::Win32::UI::Input::KeyboardAndMouse::{VIRTUAL_KEY, VK_CONTROL, VK_E, VK_SHIFT};

/// Which physical mouse button a `Binding::MouseButton` synthesises.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MouseButtonKind {
    Left,
    Right,
    Middle,
}

/// When a `Binding::MouseButton` fires, mirroring the two pen-button modes
/// Wacom's driver exposes.
///
/// The distinction is not cosmetic — the two modes travel through different
/// input paths, and only one of them is part of the Windows pen model.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ClickMode {
    /// Fire from the button alone while the pen hovers, without touching the
    /// surface.
    ///
    /// Windows has no native equivalent: MSDN scopes
    /// `POINTER_FLAG_SECONDBUTTON` to a pen "in contact with the digitizer
    /// surface with the pen barrel button pressed", and describes the barrel
    /// as a modifier for the tip's action, not an independent button. So this
    /// mode is necessarily a synthetic `SendInput` mouse click — the same
    /// mechanism Wacom's own driver uses for it.
    ///
    /// Consequence worth knowing before choosing it: Chromium classifies a
    /// mouse message that arrives within 500 ms of a pen `WM_POINTER` message
    /// *at the current cursor position* as a pen-synthesised duplicate, tags
    /// it `EF_FROM_TOUCH`, and never forwards it to the renderer. Because we
    /// stream pen samples continuously and warp the cursor to the pen tip
    /// before the click, both conditions hold, and the click is dropped over
    /// web content. It still works on browser chrome and in non-Chromium
    /// apps. `ClickAndTap` has none of these problems.
    #[default]
    HoverClick,
    /// Fire when the button is held **and** the pen tip touches down — the
    /// Wacom "Click & Tap" convention.
    ///
    /// Driven through the VMulti HID barrel bit rather than `SendInput`, so
    /// Windows produces a genuine pen pointer event at the pen's own
    /// coordinate and no synthetic mouse message exists to be filtered.
    ///
    /// Only expressible for [`MouseButtonKind::Right`]: the HID digitizer
    /// contract has exactly one pen-button usage (Barrel, 0x44), and Windows
    /// documents that additional usages are not delivered to applications in
    /// `WM_POINTER` messages.
    ClickAndTap,
}

#[derive(Clone, Debug)]
pub enum Binding {
    /// No-op. Useful as a "disabled" slot in a profile.
    None,
    /// Send `keydown(vk); keyup(vk)` once per button press.
    KeyTap(VIRTUAL_KEY),
    /// Hold one or more keys for the lifetime of the press. On press, each
    /// key is sent down in order; on release, each is sent up in reverse
    /// order (so a `Ctrl+Shift` hold releases Shift before Ctrl, matching
    /// the natural "release inner modifier first" expectation). A
    /// single-key hold is just a one-element `Vec`.
    KeyHold(Vec<VIRTUAL_KEY>),
    /// Send all keys down, then all up, in order. Useful for `Ctrl+Z` style.
    KeyChord(Vec<VIRTUAL_KEY>),
    /// Hold a mouse button while the pen button is pressed: down on press,
    /// up on release. The Wacom convention of mapping a barrel button to
    /// right-click for the context menu.
    ///
    /// `mode` decides *when* the click fires and, as a direct consequence,
    /// which input path carries it — see [`ClickMode`]. HANDOFF §2.3 #4
    /// cautions that synthetic mouse-button presses get filtered by Windows
    /// Ink **during ongoing pen contact**, which is one more reason
    /// [`ClickMode::ClickAndTap`] routes through the native barrel instead;
    /// for in-stroke modifiers prefer `KeyHold`.
    MouseButton {
        button: MouseButtonKind,
        mode: ClickMode,
    },
    /// Flip the `PEN_FLAG_INVERTED` bit on subsequent pen samples until
    /// pressed again. Krita Windows Ink mode reads the bit as "this is the
    /// eraser end of the pen".
    EraserToggle,
}

impl Binding {
    /// True when this binding is served by the pen's native HID barrel bit
    /// rather than by a synthetic `SendInput` click.
    ///
    /// The injector uses this for both halves of the same decision: assert
    /// the barrel bit on the outgoing pen report while the button is held,
    /// and suppress the `SendInput` click that would otherwise double-fire
    /// on top of it (the regression fixed in #42, from the other direction).
    pub fn drives_native_barrel(&self) -> bool {
        matches!(
            self,
            Binding::MouseButton {
                button: MouseButtonKind::Right,
                mode: ClickMode::ClickAndTap,
            }
        )
    }
}

/// Bindings for one pen's three buttons + the contact threshold.
///
/// Slot mapping on the MovinkPad Pro 14 (HANDOFF §2.1):
///   - `barrel_1` ↔ `MotionEvent.BUTTON_STYLUS_PRIMARY`
///   - `barrel_2` ↔ `MotionEvent.BUTTON_STYLUS_SECONDARY`
///   - `tertiary` ↔ `MotionEvent.BUTTON_TERTIARY` (the third stylus button —
///     does NOT chord like Wacom Pro Pen 3, despite the docs)
#[derive(Clone, Debug)]
pub struct PenButtonProfile {
    pub barrel_1: Binding,
    pub barrel_2: Binding,
    pub tertiary: Binding,
    /// Pressure must exceed this fraction [0, 1] before we treat the pen as
    /// "in contact" (HANDOFF §1.5 — OTD pattern). Default 0 means any
    /// non-zero pressure registers, matching the predecessor's behaviour.
    pub tip_threshold: f32,
}

impl Default for PenButtonProfile {
    /// Predecessor-compatible default: barrel-1 holds Ctrl, barrel-2 holds
    /// Shift, tertiary taps 'E' (Krita's eraser-toggle shortcut). The
    /// design originally proposed `EraserToggle` (flip the WinRT
    /// `Inverted` bit) as cleaner state, but in practice users find the
    /// behaviour confusing — the pen "becomes" an eraser silently with
    /// no visible UI change in some apps. A 'E' tap goes through Krita's
    /// own tool-switch UI which matches what users expect from the
    /// tertiary button on physical Wacom tablets.
    fn default() -> Self {
        Self {
            barrel_1: Binding::KeyHold(vec![VK_CONTROL]),
            barrel_2: Binding::KeyHold(vec![VK_SHIFT]),
            tertiary: Binding::KeyTap(VK_E),
            tip_threshold: 0.0,
        }
    }
}

impl PenButtonProfile {
    /// Returns a profile that matches the predecessor's exact behaviour
    /// including the `E`-tap for the third button. Provided for users who
    /// want bit-for-bit compatibility while migrating from the old build.
    pub fn predecessor_compat() -> Self {
        Self {
            barrel_1: Binding::KeyHold(vec![VK_CONTROL]),
            barrel_2: Binding::KeyHold(vec![VK_SHIFT]),
            tertiary: Binding::KeyTap(VK_E),
            tip_threshold: 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_profile_matches_design_recommendation() {
        let p = PenButtonProfile::default();
        match &p.barrel_1 {
            Binding::KeyHold(keys) => assert_eq!(keys.as_slice(), &[VK_CONTROL]),
            other => panic!("expected KeyHold([VK_CONTROL]), got {other:?}"),
        }
        match &p.barrel_2 {
            Binding::KeyHold(keys) => assert_eq!(keys.as_slice(), &[VK_SHIFT]),
            other => panic!("expected KeyHold([VK_SHIFT]), got {other:?}"),
        }
        assert!(matches!(p.tertiary, Binding::KeyTap(VK_E)));
        assert_eq!(p.tip_threshold, 0.0);
    }

    #[test]
    fn predecessor_compat_uses_e_tap() {
        let p = PenButtonProfile::predecessor_compat();
        assert!(matches!(p.tertiary, Binding::KeyTap(VK_E)));
    }

    #[test]
    fn only_right_click_and_tap_drives_the_native_barrel() {
        // The HID descriptor has exactly one pen-button usage, so this is
        // the only combination that can be served without SendInput.
        assert!(Binding::MouseButton {
            button: MouseButtonKind::Right,
            mode: ClickMode::ClickAndTap,
        }
        .drives_native_barrel());

        for b in [
            Binding::MouseButton {
                button: MouseButtonKind::Right,
                mode: ClickMode::HoverClick,
            },
            Binding::MouseButton {
                button: MouseButtonKind::Left,
                mode: ClickMode::ClickAndTap,
            },
            Binding::MouseButton {
                button: MouseButtonKind::Middle,
                mode: ClickMode::ClickAndTap,
            },
            Binding::KeyTap(VK_E),
            Binding::None,
        ] {
            assert!(!b.drives_native_barrel(), "unexpected native: {b:?}");
        }
    }

    #[test]
    fn click_mode_defaults_to_hover_click() {
        // Settings written before this field existed must keep behaving the
        // way they did — HoverClick is the pre-existing semantics.
        assert_eq!(ClickMode::default(), ClickMode::HoverClick);
    }
}
