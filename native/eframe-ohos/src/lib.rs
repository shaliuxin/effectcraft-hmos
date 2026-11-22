//! OHOS-only stand-in for the `eframe` crate.
//!
//! `effectcraft-ui-egui` (upstream, untouched) is written against the real `eframe` fork, whose
//! touchpoints here are:
//!   * `eframe::wgpu`         — a plain re-export of the `wgpu` crate
//!   * `eframe::egui_wgpu`    — a plain re-export of the `egui-wgpu` crate
//!   * `eframe::Frame`        — `new()` / `_new_kittest()` / `wgpu_render_state()` /
//!                              `winit_window()` / `set_ui_language()`
//!   * `impl eframe::App for EffectcraftApp` — the trait's `raw_input_hook` / `logic` / `ui` / `on_exit`
//!
//! The real eframe pulls in `winit` (no OpenHarmony backend, `compile_error!`), so we replace it,
//! only for this OHOS build, with this shim that has no windowing dependency. The egui / wgpu
//! objects are owned by the adapter (native/rust), exactly as in FilmCraft.
//!
//! `winit_window()` returns `None` on OHOS: there is no OS `winit::window::Window` (the surface is
//! an OHOS `NativeWindow` held by the adapter). `effectcraft-ui-egui`'s `pointer_wrap` numeric-scrub
//! across monitor edges early-returns when it is `None` — that convenience degrades gracefully and
//! the rest of the app is unaffected.
#![allow(dead_code)]

pub use egui_wgpu;
pub use wgpu;

use egui::{Context, RawInput, Ui};
use std::sync::Arc;

/// Physical screen position in device pixels (mirrors `winit::dpi::PhysicalPosition<i32>`).
#[derive(Clone, Copy, Debug, Default)]
pub struct PhysicalPosition {
    pub x: i32,
    pub y: i32,
}

/// Physical screen size in device pixels (mirrors `winit::dpi::PhysicalSize<u32>`).
#[derive(Clone, Copy, Debug, Default)]
pub struct PhysicalSize {
    pub width: u32,
    pub height: u32,
}

/// A monitor handle stand-in (mirrors `winit::window::MonitorHandle`), never actually present.
#[derive(Clone, Copy, Debug, Default)]
pub struct Monitor;

impl Monitor {
    pub fn position(&self) -> PhysicalPosition {
        PhysicalPosition::default()
    }
    pub fn size(&self) -> PhysicalSize {
        PhysicalSize::default()
    }
}

/// A window handle stand-in (mirrors `winit::window::Window`), never actually present on OHOS.
/// It exists only so the upstream `frame.winit_window()` call type-checks; it always reports
/// "nothing usable", which makes `pointer_wrap` take its early-return path.
#[derive(Clone, Debug, Default)]
pub struct Window {
    // empty: there is no OS window behind it
}

impl Window {
    pub fn inner_position(&self) -> Result<PhysicalPosition, String> {
        Ok(PhysicalPosition::default())
    }
    pub fn available_monitors(&self) -> std::vec::IntoIter<Monitor> {
        Vec::<Monitor>::new().into_iter()
    }
    pub fn current_monitor(&self) -> Option<Monitor> {
        None
    }
    pub fn set_cursor_position(&self, _position: PhysicalPosition) -> Result<(), String> {
        Err("no OS window on OpenHarmony".to_string())
    }
}

/// Minimal `eframe::Frame`. The adapter fills `render_state` after it binds the wgpu device so the
/// app's GPU compositor (`init_gpu` / `finish_gpu_build`) can build pipelines on the same device the
/// UI is painted with. `set_ui_language` is a no-op: the adapter already drives the interface
/// language through the app's own hooks, and a hidden-context `Frame` cannot touch `ctx` anyway.
#[derive(Clone, Default)]
pub struct Frame {
    render_state: Option<egui_wgpu::RenderState>,
    winit_window: Option<Arc<Window>>,
}

impl Frame {
    pub fn new() -> Self {
        Self {
            render_state: None,
            winit_window: None,
        }
    }

    /// Build a frame that already carries the GPU `RenderState` (used by the OHOS adapter once the
    /// device/surface are bound).
    pub fn with_render_state(rs: egui_wgpu::RenderState) -> Self {
        Self {
            render_state: Some(rs),
            winit_window: None,
        }
    }

    /// Create a new empty [Frame] for testing apps (mirrors the real eframe hidden constructor that
    /// `effectcraft-ui-egui::bench` and the kittest harness call unconditionally).
    pub fn _new_kittest() -> Self {
        Self::new()
    }

    pub fn set_ui_language(&self, _lang: Option<&str>) {}

    /// The GPU render state, when the adapter has attached one (None: app composites on the CPU).
    pub fn wgpu_render_state(&self) -> Option<&egui_wgpu::RenderState> {
        self.render_state.as_ref()
    }

    /// There is no OS `winit` window on OHOS, so this is always `None`.
    pub fn winit_window(&self) -> Option<&Arc<Window>> {
        self.winit_window.as_ref()
    }
}

/// Minimal `eframe::App` trait with the four methods `EffectcraftApp` implements. Every method has
/// a default body so the trait can be implemented partially, matching upstream's usage.
pub trait App {
    fn raw_input_hook(&mut self, _ctx: &Context, _raw_input: &mut RawInput) {}
    fn logic(&mut self, _ctx: &Context, _frame: &mut Frame) {}
    fn ui(&mut self, _ui: &mut Ui, _frame: &mut Frame) {}
    fn on_exit(&mut self) {}
}
