// SPDX-License-Identifier: GPL-3.0-or-later

//! How long the UI thread is kept busy at a time (NFR-PERF-10: nothing over 16 ms), and when the first
//! frame is up (NFR-PERF-01).
//!
//! A stretch starts when winit hands the window an event, or when another thread's action is run, and
//! ends when the event loop next gets round to a closure posted at its start. That takes in Slint's
//! handling of the event, the redraw it caused and whatever was queued behind it: all the time the
//! window could not have answered. A stretch over the budget is logged at Debug, and the longest
//! is logged at the end.

use std::cell::{Cell, RefCell};
use std::time::{Duration, Instant};

use slint::winit_030::{EventResult, WinitWindowAccessor, winit::event::WindowEvent};

/// NFR-PERF-10: one frame at 60 Hz.
pub const BUDGET: Duration = Duration::from_millis(16);

#[derive(Default)]
struct Clock {
    /// What started the stretch the UI thread is in, and when.
    busy: Cell<Option<(Instant, &'static str)>>,
    longest: Cell<Option<(Duration, &'static str)>>,
    over: Cell<u32>,
    /// Runs once, after the first frame is drawn.
    on_first_frame: RefCell<Option<Box<dyn FnOnce()>>>,
}

thread_local! {
    static CLOCK: Clock = Clock::default();
}

/// Times what the window is handed, and calls `on_first_frame` once its first frame is drawn. Only
/// one hook per window: this replaces any other.
pub fn watch(window: &slint::Window, on_first_frame: Box<dyn FnOnce()>) {
    CLOCK.with(|clock| *clock.on_first_frame.borrow_mut() = Some(on_first_frame));
    window.on_winit_window_event(|_, event| {
        let what = name(event);
        begin(what);
        if matches!(event, WindowEvent::RedrawRequested)
            && CLOCK.with(|clock| clock.on_first_frame.borrow().is_some())
        {
            // After the event, so after Slint has drawn the frame.
            let _ = slint::invoke_from_event_loop(|| {
                if let Some(first) = CLOCK.with(|clock| clock.on_first_frame.borrow_mut().take()) {
                    first();
                }
            });
        }
        EventResult::Propagate
    });
}

/// Runs another thread's action as a stretch of its own.
pub fn time<T>(what: &'static str, action: impl FnOnce() -> T) -> T {
    let start = Instant::now();
    let result = action();
    record(what, start.elapsed());
    result
}

/// The longest stretch so far, what started it, and how many went over [`BUDGET`].
#[must_use]
pub fn longest() -> (Option<(Duration, &'static str)>, u32) {
    CLOCK.with(|clock| (clock.longest.get(), clock.over.get()))
}

fn begin(what: &'static str) {
    let started = CLOCK.with(|clock| {
        if clock.busy.get().is_some() {
            return false;
        }
        clock.busy.set(Some((Instant::now(), what)));
        true
    });
    if started && slint::invoke_from_event_loop(end).is_err() {
        // Only as the app ends.
        CLOCK.with(|clock| clock.busy.set(None));
    }
}

fn end() {
    if let Some((start, what)) = CLOCK.with(|clock| clock.busy.take()) {
        record(what, start.elapsed());
    }
}

fn record(what: &'static str, took: Duration) {
    CLOCK.with(|clock| {
        if clock
            .longest
            .get()
            .is_none_or(|(longest, _)| took > longest)
        {
            clock.longest.set(Some((took, what)));
        }
        if took > BUDGET {
            clock.over.set(clock.over.get().saturating_add(1));
            tracing::debug!(
                what,
                ms = took.as_secs_f64() * 1000.0,
                "the UI thread was busy for longer than a frame"
            );
        }
    });
}

/// A short, fixed name for an event: the events themselves carry data not worth logging.
fn name(event: &WindowEvent) -> &'static str {
    match event {
        WindowEvent::RedrawRequested => "redraw",
        WindowEvent::Resized(_) => "resize",
        WindowEvent::Moved(_) => "move",
        WindowEvent::CursorMoved { .. } => "cursor",
        WindowEvent::CursorEntered { .. } | WindowEvent::CursorLeft { .. } => "cursor in or out",
        WindowEvent::MouseInput { .. } => "mouse button",
        WindowEvent::MouseWheel { .. } => "wheel",
        WindowEvent::KeyboardInput { .. } => "key",
        WindowEvent::ModifiersChanged(_) => "modifiers",
        WindowEvent::Focused(_) => "focus",
        WindowEvent::CloseRequested => "close",
        WindowEvent::ScaleFactorChanged { .. } => "scale",
        WindowEvent::ThemeChanged(_) => "theme",
        WindowEvent::Occluded(_) => "occluded",
        _ => "window event",
    }
}
