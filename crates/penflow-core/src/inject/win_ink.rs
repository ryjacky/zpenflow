//! Unified pen + touch injection via the WinRT `InputInjector` (Windows Ink).
//!
//! Why one injector for both: the WinRT
//! `Windows.UI.Input.Preview.Injection.InputInjector` class is **agile**
//! (its methods are thread-safe and can be called from any thread without
//! marshaling). By contrast Win32 `InjectTouchInput` has hard
//! thread-affinity (the same thread that called `InitializeTouchInjection`
//! must call `InjectTouchInput`); driving it from tokio's worker pool
//! returns `E_INVALIDARG` because the worker thread shifts between awaits.
//!
//! HANDOFF.md §2.3 #3 noted that the predecessor's Python build moved
//! touch injection to Win32 because of misleading `winsdk` Python signatures
//! for the WinRT touch API (`InitializeTouchInjection(mode)` single-arg vs.
//! C# two-arg, `InjectTouchInput` iterable vs. single). Those signature
//! issues are Python-bindings-only — `windows-rs` generates types directly
//! from the WinRT IDL, so we can use the WinRT API uniformly here.
//!
//! Design choice: hold ONE `InputInjector` and call both
//! `InitializePenInjection` and `InitializeTouchInjection` on it. The
//! injector then accepts both `InjectPenInput` and `InjectTouchInput`
//! across its lifetime.

use std::collections::{HashMap, HashSet};

use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::UI::Input::Preview::Injection::{
    InjectedInputPenButtons, InjectedInputPenInfo, InjectedInputPenParameters,
    InjectedInputPoint, InjectedInputPointerInfo, InjectedInputPointerOptions,
    InjectedInputTouchInfo, InjectedInputTouchParameters, InjectedInputVisualizationMode,
    InputInjector as WinRtInputInjector,
};
use windows_collections::IIterable;

use crate::error::{EngineError, EngineResult};

use super::{PenSample, TouchPoint};

/// Unified pen + touch injector backed by `Windows.UI.Input.Preview.Injection.InputInjector`.
pub struct InputInjector {
    injector: WinRtInputInjector,
    /// Whether `InitializePenInjection` succeeded (gates the pen-side
    /// teardown in `Drop`).
    pen_initialized: bool,
    /// Whether `InitializeTouchInjection` succeeded.
    touch_initialized: bool,

    // --- pen flip-then-flush state (HANDOFF §1.5) ---
    last_pen_eraser: bool,
    last_pen_in_range: bool,

    // --- touch state machine ---
    /// Last submitted snapshot keyed by contact id; used to fabricate
    /// `New | PointerDown` for arrivals and `PointerUp` at the previous
    /// position for departures.
    last_touch_pos: HashMap<u32, (i32, i32)>,
}

// SAFETY: WinRT InputInjector is agile; the only non-trivial state is the
// HashMap and the bookkeeping bools, all of which are Send. We only ever
// own this struct on one thread at a time.
unsafe impl Send for InputInjector {}

impl InputInjector {
    /// Build the injector and initialise both pen and touch subsystems.
    /// Sets `PER_MONITOR_AWARE_V2` process-wide so injected pixel coordinates
    /// are physical pixels, not DIPs (gate-2 finding §4.4b).
    pub fn new() -> EngineResult<Self> {
        // Process-wide; idempotent. Returns an error if already set, which
        // we ignore — that case means the host (Tauri) already configured
        // it.
        let _ = unsafe {
            SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
        };

        let injector = WinRtInputInjector::TryCreate().map_err(EngineError::from)?;

        injector
            .InitializePenInjection(InjectedInputVisualizationMode::Default)
            .map_err(EngineError::from)?;
        injector
            .InitializeTouchInjection(InjectedInputVisualizationMode::Default)
            .map_err(EngineError::from)?;

        Ok(Self {
            injector,
            pen_initialized: true,
            touch_initialized: true,
            last_pen_eraser: false,
            last_pen_in_range: false,
            last_touch_pos: HashMap::new(),
        })
    }

    /// Inject one pen sample. Implements the eraser flip-then-flush
    /// protocol from HANDOFF §1.5: when the eraser bit changes mid-stroke,
    /// emit one synthetic out-of-range frame at the previous coordinates
    /// (with the OLD eraser state) so the driver sees a clean `Inverted`
    /// transition.
    pub fn inject_pen(&mut self, sample: &PenSample) -> EngineResult<()> {
        if sample.eraser != self.last_pen_eraser && self.last_pen_in_range {
            let mut flush = *sample;
            flush.in_range = false;
            flush.in_contact = false;
            flush.pressure = 0.0;
            self.write_pen(&flush, self.last_pen_eraser)?;
        }
        self.write_pen(sample, sample.eraser)?;
        self.last_pen_eraser = sample.eraser;
        self.last_pen_in_range = sample.in_range;
        Ok(())
    }

    fn write_pen(&self, sample: &PenSample, eraser: bool) -> EngineResult<()> {
        let info = InjectedInputPenInfo::new().map_err(EngineError::from)?;

        let mut opts = InjectedInputPointerOptions::None;
        if sample.in_range {
            opts = opts | InjectedInputPointerOptions::InRange;
        }
        if sample.in_contact {
            opts = opts
                | InjectedInputPointerOptions::InContact
                | InjectedInputPointerOptions::Update;
        }
        if !sample.in_range && !sample.in_contact {
            opts = opts | InjectedInputPointerOptions::PointerUp;
        }

        let pointer = InjectedInputPointerInfo {
            PointerId: 1,
            PointerOptions: opts,
            PixelLocation: InjectedInputPoint {
                PositionX: sample.x,
                PositionY: sample.y,
            },
            TimeOffsetInMilliseconds: 0,
            PerformanceCount: 0,
        };
        info.SetPointerInfo(pointer).map_err(EngineError::from)?;

        let buttons = if eraser {
            InjectedInputPenButtons::Inverted
        } else {
            InjectedInputPenButtons::None
        };
        info.SetPenButtons(buttons).map_err(EngineError::from)?;

        let params = InjectedInputPenParameters::Pressure
            | InjectedInputPenParameters::TiltX
            | InjectedInputPenParameters::TiltY;
        info.SetPenParameters(params).map_err(EngineError::from)?;
        info.SetPressure(sample.pressure as f64)
            .map_err(EngineError::from)?;
        info.SetTiltX(sample.tilt_x_deg).map_err(EngineError::from)?;
        info.SetTiltY(sample.tilt_y_deg).map_err(EngineError::from)?;

        self.injector
            .InjectPenInput(&info)
            .map_err(EngineError::from)?;
        Ok(())
    }

    /// Inject a multi-touch snapshot. The struct compares against the
    /// previous snapshot to compute the right `InjectedInputPointerOptions`
    /// for each contact:
    ///
    /// - **New id** (not in previous): `New | InRange | InContact | PointerDown`
    /// - **Persistent id** (in both): `InRange | InContact | Update`
    /// - **Lost id** (in previous, missing now): `PointerUp` at the last
    ///   known position
    ///
    /// The first contact in the snapshot also gets `Primary`.
    ///
    /// `state` field on `TouchPoint` is **ignored** — the diff overrides
    /// whatever the caller passed.
    pub fn inject_touch(&mut self, snapshot: &[TouchPoint]) -> EngineResult<()> {
        // No contacts and no previous contacts → nothing to inject.
        if snapshot.is_empty() && self.last_touch_pos.is_empty() {
            return Ok(());
        }

        let mut infos: Vec<InjectedInputTouchInfo> = Vec::new();
        let mut current_ids: HashSet<u32> = HashSet::with_capacity(snapshot.len());

        // 1. Down / Update for every contact in the new snapshot.
        for (i, tp) in snapshot.iter().enumerate() {
            current_ids.insert(tp.id);
            let was_down = self.last_touch_pos.contains_key(&tp.id);
            let mut opts = InjectedInputPointerOptions::InRange
                | InjectedInputPointerOptions::InContact;
            if was_down {
                opts = opts | InjectedInputPointerOptions::Update;
            } else {
                opts = opts
                    | InjectedInputPointerOptions::New
                    | InjectedInputPointerOptions::PointerDown;
            }
            if i == 0 {
                opts = opts | InjectedInputPointerOptions::Primary;
            }
            infos.push(make_touch_info(tp.id, tp.x, tp.y, opts)?);
        }

        // 2. Up for ids that disappeared this frame, using the last known
        //    position.
        let mut lost: Vec<(u32, i32, i32)> = Vec::new();
        for (id, pos) in self.last_touch_pos.iter() {
            if !current_ids.contains(id) {
                lost.push((*id, pos.0, pos.1));
            }
        }
        for (id, x, y) in lost {
            infos.push(make_touch_info(
                id,
                x,
                y,
                InjectedInputPointerOptions::PointerUp,
            )?);
        }

        if infos.is_empty() {
            return Ok(());
        }

        // 3. Wrap as IIterable<InjectedInputTouchInfo> and inject.
        //    `T::Default` for an interface/class type is `Option<T>`, so we
        //    map each value into `Some` for the `From<Vec<T::Default>>`
        //    impl.
        let optional: Vec<Option<InjectedInputTouchInfo>> =
            infos.into_iter().map(Some).collect();
        let iterable: IIterable<InjectedInputTouchInfo> = optional.into();
        self.injector
            .InjectTouchInput(&iterable)
            .map_err(EngineError::from)?;

        // 4. Update last_touch_pos to mirror the new snapshot.
        self.last_touch_pos.clear();
        for tp in snapshot {
            self.last_touch_pos.insert(tp.id, (tp.x, tp.y));
        }
        Ok(())
    }
}

impl Drop for InputInjector {
    fn drop(&mut self) {
        if self.touch_initialized {
            let _ = self.injector.UninitializeTouchInjection();
        }
        if self.pen_initialized {
            let _ = self.injector.UninitializePenInjection();
        }
    }
}

fn make_touch_info(
    id: u32,
    x: i32,
    y: i32,
    options: InjectedInputPointerOptions,
) -> EngineResult<InjectedInputTouchInfo> {
    let info = InjectedInputTouchInfo::new().map_err(EngineError::from)?;
    let pointer = InjectedInputPointerInfo {
        PointerId: id,
        PointerOptions: options,
        PixelLocation: InjectedInputPoint {
            PositionX: x,
            PositionY: y,
        },
        TimeOffsetInMilliseconds: 0,
        PerformanceCount: 0,
    };
    info.SetPointerInfo(pointer).map_err(EngineError::from)?;
    // No pressure / orientation provided — leave TouchParameters at None.
    info.SetTouchParameters(InjectedInputTouchParameters::None)
        .map_err(EngineError::from)?;
    Ok(info)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::TouchState;

    /// Smoke: build the unified injector. Initialises COM apartment if the
    /// harness hasn't.
    #[test]
    fn create_injector() {
        let _ = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
            .ok()
        };
        match InputInjector::new() {
            Ok(_) => {}
            Err(EngineError::Win32(e)) => {
                eprintln!("[skip] InputInjector unavailable in this context: {e:?}");
            }
            Err(other) => panic!("unexpected error: {other:?}"),
        }
    }

    /// Inject a single hover sample at (1, 1) and a one-finger touch at
    /// (1, 1). Cursor blip is the same as the gate-3 probe, harmless.
    #[test]
    fn pen_and_touch_smoke() {
        let _ = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            )
            .ok()
        };
        let mut inj = match InputInjector::new() {
            Ok(i) => i,
            Err(EngineError::Win32(e)) => {
                eprintln!("[skip] InputInjector unavailable: {e:?}");
                return;
            }
            Err(other) => panic!("unexpected: {other:?}"),
        };

        let pen = PenSample {
            x: 1,
            y: 1,
            pressure: 0.0,
            tilt_x_deg: 0,
            tilt_y_deg: 0,
            in_range: true,
            in_contact: false,
            eraser: false,
            captured_at: None,
        };
        inj.inject_pen(&pen).expect("pen hover");

        let touch_down = vec![TouchPoint {
            id: 1,
            x: 1,
            y: 1,
            state: TouchState::Update,
        }];
        inj.inject_touch(&touch_down).expect("touch down");
        // Lift it.
        inj.inject_touch(&[]).expect("touch up");
    }
}
