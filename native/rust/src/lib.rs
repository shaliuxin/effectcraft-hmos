//! 鸿蒙专属 FFI 边界。动态图形 / 视觉特效合成的全部逻辑保持在安全 Rust 上游（effectcraft 各 crate）。
//!
//! 与 FilmCraft 同栈，关键差异：
//!   1) **App 入口**：`EffectcraftApp::new(effectcraft_host::session())`。`effectcraft_host::session()`
//!      已接好媒体解码 / 导入 / 表达式 / 脚本 / 导出，直接可用（不像 FilmCraft 需注入自定义 Services）。
//!   2) **GPU 注入**：EffectCraft 从 `frame.wgpu_render_state()` 读取 RenderState 再建自己的
//!      GPU 合成器（235 条 compute 管线）。适配层把已绑定 surface/device 的 `egui_wgpu::RenderState`
//!      经 `eframe::Frame::with_render_state(rs)` 交给 Frame，`logic`/`ui`（内部 `init_gpu`）取用它，
//!      与 UI 共用同一 device —— 预览在 GPU 上合成；取不到就回退 CPU 合成，UI 不受影响。
//!   3) **文件对话框**：EffectCraft 主路径是 `Hooks::file_dialog_async`（非阻塞，另线程回调 `done`），
//!      命令通道仍是 `app.session.execute(id, params)`；文件导入走 `file.import`（`{"paths":[…]}`）。
//!   4) **IME**：`EffectcraftApp::install_ime_reset_hooks(&ctx)`（关联函数，非自由函数）。
#![allow(clippy::needless_lifetimes)]
mod appearance;
mod bridge;
mod display_color;
mod file_open;
mod form_factor;
mod input;
mod locale;
mod render;

use eframe::App as _; // raw_input_hook / logic / ui / on_exit 都是 trait 方法
use effectcraft_ui_egui::{DialogDone, EffectcraftApp, FileDialog, Hooks};
use serde::Deserialize;
use std::{
    sync::{
        OnceLock,
        mpsc,
        atomic::{AtomicI8, Ordering},
    },
    time::{Duration, Instant},
};

/// 把 `log` crate 的记录转到 hilog：wgpu-hal 的 EGL 诊断（平台选择 / surface 创建 / swap 失败）
/// 默认没有 logger 就会被丢掉，设备上完全看不见。
#[cfg(target_env = "ohos")]
mod glog {
    use std::ffi::{CString, c_char};

    unsafe extern "C" {
        fn wc_hilog(level: i32, message: *const c_char);
    }

    struct Hilog;

    impl log::Log for Hilog {
        fn enabled(&self, _: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            let level = match record.level() {
                log::Level::Error => 2,
                log::Level::Warn => 1,
                _ => 0,
            };
            let line = format!("[{}] {}: {}", record.level(), record.target(), record.args());
            if let Ok(text) = CString::new(line) {
                // SAFETY: 有效的 NUL 结尾 C 字符串；wc_hilog 只读该指针。
                unsafe { wc_hilog(level, text.as_ptr()) };
            }
        }

        fn flush(&self) {}
    }

    static LOGGER: Hilog = Hilog;

    pub fn install() {
        if log::set_logger(&LOGGER).is_ok() {
            log::set_max_level(log::LevelFilter::Info);
        }
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Config {
    files: String,
    #[serde(default)]
    documents: String,
    #[serde(default = "default_scale")]
    scale: f32,
    #[serde(default)]
    language: String,
    /// 系统 locale（`zh-Hans-CN` 这类 BCP-47 标签），供「跟随系统」解析。
    #[serde(default, rename = "systemLocale")]
    system_locale: String,
    #[serde(default, rename = "displayP3")]
    display_p3: bool,
    #[serde(default, rename = "systemTheme")]
    system_theme: String,
    #[serde(default)]
    fullscreen: bool,
    #[serde(default, rename = "deviceType")]
    device_type: String,
    #[serde(default, rename = "widthVp")]
    width_vp: f32,
}
fn default_scale() -> f32 {
    1.
}

#[allow(dead_code)]
enum Message {
    Attach(usize, u32, u32),
    Resize(u32, u32),
    Detach,
    Frame,
    Active(bool),
    Input(input::Input),
    /// 素材 / 工程文件：`(路径名, 字节)`。文件已由 ArkTS 落到沙箱 `.inbox`，这里只把沙箱路径交给上游。
    Open(String, Vec<u8>),
    Drop(String, Vec<u8>, f32, f32),
    DropFile(String, std::fs::File, f32, f32),
    Close,
    Wake(Duration),
    /// 命令通道（自动化 / 控制端）：转成 `app.session.execute(id, params)`。
    Command(String, serde_json::Value),
}

static SEND: OnceLock<mpsc::Sender<Message>> = OnceLock::new();
fn send(message: Message) {
    if let Some(tx) = SEND.get() {
        let _ = tx.send(message);
    }
}

/// 把 ArkTS 上报的系统主题透传给 appearance 模块。
static SYSTEM_THEME_TOKEN: AtomicI8 = AtomicI8::new(0);

/// # Safety
/// `ptr`/`len` 必须在调用期间指向可读内存；调用方保证。
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> Option<&'a [u8]> {
    if len > isize::MAX as usize || (ptr.is_null() && len != 0) {
        return None;
    }
    if len == 0 {
        return Some(&[]);
    }
    Some(unsafe { std::slice::from_raw_parts(ptr, len) })
}

fn copy_bytes(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(data.len())
        .map_err(|e| format!("Not enough memory to open file: {e}"))?;
    bytes.extend_from_slice(data);
    Ok(bytes)
}

fn write_temp_file(dir: &str, name: &str, bytes: &[u8]) -> std::io::Result<String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let safe: String = name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let path = std::path::Path::new(dir).join(format!("{stamp}_{safe}"));
    std::fs::write(&path, bytes)?;
    Ok(path.to_string_lossy().into_owned())
}

// ===================== FFI 导出（C++ 边界调用） =====================

/// # Safety
/// 指针 / 长度对必须指向可读内存直到本调用返回；notify 回调在进程生命周期内保持有效。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_init(ptr: *const u8, len: usize, notify: bridge::Notify) -> bool {
    let Some(data) = (unsafe { bytes(ptr, len) }) else {
        return false;
    };
    let Ok(config) = serde_json::from_slice::<Config>(data) else {
        return false;
    };
    if !config.scale.is_finite() || config.scale <= 0. || config.scale > 8. {
        return false;
    }
    bridge::init(notify);
    #[cfg(target_env = "ohos")]
    glog::install();
    // EffectCraft 的 config/自动恢复/模型/字体按 EFFECTCRAFT_CONFIG_DIR（回退 HOME/.config）定位，
    // 指向沙箱 files 使其落在有写权限的空间。必须在 effectcraft_host::session() 之前设（进程级 OnceLock）。
    if !config.files.is_empty() {
        std::env::set_var("EFFECTCRAFT_CONFIG_DIR", &config.files);
    }
    locale::install(&config.files, &config.system_locale, &config.language);
    if !config.system_theme.is_empty() {
        let dark = config.system_theme.eq_ignore_ascii_case("dark");
        SYSTEM_THEME_TOKEN.store(if dark { 1 } else { -1 }, Ordering::Release);
        appearance::set_dark(dark);
    }
    let (tx, rx) = mpsc::channel();
    if SEND.set(tx).is_err() {
        return true;
    }
    std::thread::Builder::new()
        .name("effectcraft-native".into())
        .spawn(move || {
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker(config, rx))).is_err() {
                bridge::notify(0, "error", "Native host initialization failed", &[]);
            }
        })
        .is_ok()
}

/// # Safety
/// 指针 / 长度对必须可读直到返回。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_input(ptr: *const u8, len: usize) {
    let Some(data) = (unsafe { bytes(ptr, len) }) else {
        return;
    };
    let Ok(value) = serde_json::from_slice(data) else {
        return;
    };
    send(Message::Input(value));
}

/// # Safety
/// 同 `wc_init`。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_reply(
    id: u64,
    ok: bool,
    text: *const u8,
    text_len: usize,
    data: *const u8,
    len: usize,
) {
    let Some(text) = (unsafe { bytes(text, text_len) }) else {
        bridge::reply(id, Err("Invalid reply text buffer".into()));
        return;
    };
    let Some(data) = (unsafe { bytes(data, len) }) else {
        bridge::reply(id, Err("Invalid reply data buffer".into()));
        return;
    };
    let text = String::from_utf8_lossy(text).into_owned();
    bridge::reply(
        id,
        if ok {
            copy_bytes(data).map(|data| (text, data))
        } else {
            Err(text)
        },
    );
}

/// # Safety
/// 指针 / 长度对必须可读直到返回。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_open(text: *const u8, n: usize, data: *const u8, len: usize) {
    if let (Some(text), Some(data)) = (unsafe { bytes(text, n) }, unsafe { bytes(data, len) }) {
        match copy_bytes(data) {
            Ok(data) => send(Message::Open(String::from_utf8_lossy(text).into_owned(), data)),
            Err(error) => bridge::notify(0, "error", &error, &[]),
        }
    }
}

/// # Safety
/// 指针 / 长度对必须可读直到返回。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_drop(
    text: *const u8,
    n: usize,
    data: *const u8,
    len: usize,
    x: f32,
    y: f32,
) {
    if !x.is_finite() || !y.is_finite() {
        return;
    }
    if let (Some(text), Some(data)) = (unsafe { bytes(text, n) }, unsafe { bytes(data, len) }) {
        send(Message::Drop(
            String::from_utf8_lossy(text).into_owned(),
            data.to_vec(),
            x,
            y,
        ));
    }
}

/// # Safety
/// `text`/`n` 必须可读且 `fd` 打开直到返回。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_drop_file(text: *const u8, n: usize, fd: i32, x: f32, y: f32) {
    if !x.is_finite() || !y.is_finite() {
        return;
    }
    let Some(text) = (unsafe { bytes(text, n) }) else {
        return;
    };
    match unsafe { file_open::duplicate(fd) } {
        Ok(file) => send(Message::DropFile(
            String::from_utf8_lossy(text).into_owned(),
            file,
            x,
            y,
        )),
        Err(error) => bridge::notify(0, "error", &error, &[]),
    }
}

/// # Safety
/// `ptr`/`len` 为自由文本命令标识。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_command(ptr: *const u8, len: usize) {
    let Some(data) = (unsafe { bytes(ptr, len) }) else {
        return;
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(data) else {
        return;
    };
    let Some(id) = v.get("id").and_then(|v| v.as_str()) else {
        return;
    };
    send(Message::Command(
        id.into(),
        v.get("params").cloned().unwrap_or_default(),
    ));
}

/// # Safety
/// `window` 必须是存活的 OHNativeWindow，且已获取一个原生引用；该引用所有权转移给 worker 及其 wgpu surface。
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wc_attach(window: usize, w: u32, h: u32) {
    if window != 0 {
        send(Message::Attach(window, w, h));
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn wc_resize(w: u32, h: u32) {
    send(Message::Resize(w, h));
}

#[unsafe(no_mangle)]
pub extern "C" fn wc_detach() {
    bridge::cancel();
    send(Message::Detach);
}

#[unsafe(no_mangle)]
pub extern "C" fn wc_frame() {
    send(Message::Frame);
}

#[unsafe(no_mangle)]
pub extern "C" fn wc_active(active: bool) {
    send(Message::Active(active));
}

#[unsafe(no_mangle)]
pub extern "C" fn wc_close() {
    send(Message::Close);
}

// ===================== 仅 OHOS 宿主逻辑 =====================

#[cfg(target_env = "ohos")]
unsafe extern "C" {
    fn wc_request_vsync();
}

#[cfg(target_env = "ohos")]
fn worker(config: Config, rx: mpsc::Receiver<Message>) {
    bridge::notify(0, "stage", "Rust worker entered", &[]);
    let ctx = egui::Context::default();
    let form = form_factor::Form::new(&config.device_type, config.width_vp);
    bridge::notify(
        0,
        "diagnostic",
        &format!(
            "form factor {:?}, {:.0} vp, {:?}",
            form.device_type, form.width_vp, form.width_class
        ),
        &[],
    );

    // effectcraft_host::session() 已接好媒体/导入/表达式/脚本/导出；EffectcraftApp 已把宿主缝直接
    // 暴露为 new(Session) / raw_input_hook / logic / ui / on_exit（impl eframe::App），hooks 字段可直写。
    let session = effectcraft_host::session();
    let mut app = EffectcraftApp::new(session);
    EffectcraftApp::install_ime_reset_hooks(&ctx);
    app.integrated_titlebar = false; // 鸿蒙走 ArkTS 原生标题栏，不用上游自绘标题栏
    app.hooks = hooks(&config.files, &config.system_locale);
    bridge::notify(0, "stage", "editor ready", &[]);

    let mut render = match render::Render::new(config.display_p3) {
        Ok(render) => render,
        Err(error) => {
            bridge::notify(0, "error", &error, &[]);
            return;
        }
    };
    let mut active = true;
    let mut visible = false;
    let mut deadline: Option<Instant> = None;
    let started = Instant::now();
    let mut events: Vec<egui::Event> = Vec::new();
    let mut input_state = input::State::default();
    let mut close = false;
    let mut exiting = false;
    // 软键盘是否已由宿主弹起：避免每帧重复通知，仅在 egui 文本焦点变化时切换。
    let mut ime_shown = false;
    // 每帧复用的最小 eframe::Frame（shim）；绑定 surface 后携带 GPU RenderState 交给上游 init_gpu。
    let mut frame = eframe::Frame::new();

    ctx.set_request_repaint_callback(|info| send(Message::Wake(info.delay)));
    bridge::notify(0, "ready", "Native host initialized", &[]);

    loop {
        if exiting {
            break;
        }
        let wait = deadline.map(|d| d.saturating_duration_since(Instant::now()));
        let message = match wait {
            Some(wait) => match rx.recv_timeout(wait) {
                Ok(m) => m,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    deadline = None;
                    if active && visible && render.attached() {
                        unsafe { wc_request_vsync() };
                    }
                    continue;
                }
                Err(_) => break,
            },
            None => match rx.recv() {
                Ok(m) => m,
                Err(_) => break,
            },
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || -> Result<(), String> {
                match message {
                    Message::Attach(ptr, w, h) => {
                        visible = w > 0 && h > 0;
                        render.bind(render::Window(ptr), w, h)?;
                        // GPU 注入：把已绑定 surface/device 的 RenderState 交给 Frame，
                        // 上游 init_gpu 从 frame.wgpu_render_state() 取用，在同一个 device 上建合成器。
                        if let Some(rs) = render.egui_render_state() {
                            frame = eframe::Frame::with_render_state(rs);
                        }
                        bridge::resume();
                        deadline = Some(Instant::now());
                        ime_shown = false;
                        bridge::notify(0, "ready", "Native egui / wgpu GLES + GPU compositor", &[]);
                    }
                    Message::Resize(w, h) => {
                        visible = w > 0 && h > 0;
                        render.resize(w, h)?;
                        deadline = Some(Instant::now());
                    }
                    Message::Detach => {
                        visible = false;
                        deadline = None;
                        render.detach();
                        input_state.cancel(&mut events);
                        ime_shown = false;
                        bridge::notify(0, "ime", "hide", &[]);
                    }
                    Message::Active(value) => {
                        active = value;
                        if active {
                            bridge::resume();
                            deadline = Some(Instant::now());
                        } else {
                            deadline = None;
                            input_state.cancel(&mut events);
                        }
                    }
                    Message::Input(value) => {
                        if value.kind == "system_theme" {
                            let dark = value.text.eq_ignore_ascii_case("dark");
                            appearance::set_dark(dark);
                            deadline = Some(Instant::now());
                            return Ok(());
                        }
                        input_state.push(value, config.scale, &mut events);
                        deadline = Some(Instant::now());
                    }
                    Message::Open(name, data) => {
                        if let Ok(path) = write_temp_file(&config.files, &name, &data) {
                            open_into_app(&mut app, &path);
                        }
                        deadline = Some(Instant::now());
                    }
                    Message::Drop(name, data, _x, _y) => {
                        if let Ok(path) = write_temp_file(&config.files, &name, &data) {
                            open_into_app(&mut app, &path);
                        }
                        deadline = Some(Instant::now());
                    }
                    Message::DropFile(name, file, _x, _y) => {
                        match file_open::read(file) {
                            Ok(data) => {
                                if let Ok(path) = write_temp_file(&config.files, &name, &data) {
                                    open_into_app(&mut app, &path);
                                }
                            }
                            Err(e) => bridge::notify(0, "error", &e, &[]),
                        }
                        deadline = Some(Instant::now());
                    }
                    Message::Close => {
                        bridge::resume();
                        close = true;
                        deadline = Some(Instant::now());
                    }
                    Message::Wake(delay) => {
                        if active && visible {
                            if let Some(next) = Instant::now().checked_add(delay) {
                                deadline = Some(deadline.map_or(next, |d| d.min(next)));
                            }
                        }
                    }
                    Message::Command(id, params) => {
                        match app.session.execute(&id, params) {
                            Ok(v) => {
                                let preview = v.to_string();
                                let preview = preview.chars().take(120).collect::<String>();
                                bridge::notify(0, "command", &format!("{id}: {preview}"), &[]);
                            }
                            Err(e) => bridge::notify(0, "command_err", &format!("{id}: {e}"), &[]),
                        }
                        deadline = Some(Instant::now());
                    }
                    Message::Frame => {
                        if !active || !visible {
                            return Ok(());
                        }
                        if !render.attached() {
                            return Ok(());
                        }
                        let size = egui::vec2(
                            render.width as f32 / config.scale,
                            render.height as f32 / config.scale,
                        );
                        let mut raw = egui::RawInput {
                            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                            time: Some(started.elapsed().as_secs_f64()),
                            events: std::mem::take(&mut events),
                            system_theme: appearance::current(),
                            ..Default::default()
                        };
                        let viewport = raw.viewports.entry(egui::ViewportId::ROOT).or_default();
                        viewport.native_pixels_per_point = Some(config.scale);
                        viewport.inner_rect = raw.screen_rect;
                        viewport.focused = Some(true);
                        let validating_close = close;
                        if close {
                            viewport.events.push(egui::ViewportEvent::Close);
                            close = false;
                        }
                        // 宿主缝：raw_input_hook（trait）→ logic/ui（trait，经携带 RenderState 的 Frame）。
                        app.raw_input_hook(&ctx, &mut raw);
                        let output = editor_frame(&ctx, &mut app, raw, &mut frame);
                        for command in &output.platform_output.commands {
                            if let egui::OutputCommand::OpenUrl(url) = command {
                                bridge::notify(0, "url", &url.url, &[]);
                            }
                        }
                        // 软键盘随文本焦点自动弹起 / 收起。
                        let ime_wanted = output.platform_output.ime.is_some();
                        if ime_wanted != ime_shown {
                            ime_shown = ime_wanted;
                            bridge::notify(0, "ime", if ime_wanted { "show" } else { "hide" }, &[]);
                        }
                        let mut commands = Vec::new();
                        let mut repaint_delay: Option<Duration> = None;
                        if let Some(v) = output.viewport_output.get(&egui::ViewportId::ROOT) {
                            commands.extend(v.commands.clone());
                            repaint_delay = Some(v.repaint_delay);
                        }
                        // 关窗门控：本帧向 egui 推了 ViewportEvent::Close 后，若上游报 CancelClose 说明有
                        // 未保存工程、已弹出「保存更改？」对话框，必须继续正常绘制而不能退出；用户选择后
                        // 上游发 ViewportCommand::Close，下一帧重新注入 Close 事件再验一次，无 CancelClose 才放行。
                        match close_action(validating_close, &commands) {
                            CloseAction::Confirm => {
                                app.on_exit();
                                bridge::notify(0, "close", "", &[]);
                                exiting = true;
                                return Ok(());
                            }
                            CloseAction::Request => {
                                close = true;
                                deadline = Some(Instant::now());
                            }
                            CloseAction::Cancel | CloseAction::None => {}
                        }
                        let painted = render.paint(&ctx, output)?;
                        if first_frame_marker(painted) {
                            bridge::notify(0, "first_frame", "Native UI presented", &[]);
                        }
                        if painted && active && visible && render.attached() {
                            deadline = match repaint_delay {
                                Some(delay) if delay.is_zero() => Some(Instant::now()),
                                Some(delay) if delay < Duration::from_secs(3600) => {
                                    Some(Instant::now() + delay)
                                }
                                _ => None,
                            };
                        }
                    }
                }
                Ok(())
            },
        ));
        let _ = result;
    }

    // 正常退出路径（Detach / 断连）也要 flush session。
    if !exiting {
        let _ = app.on_exit();
    }
}

#[cfg(target_env = "ohos")]
#[allow(dead_code)]
fn first_frame_marker(painted: bool) -> bool {
    use std::sync::atomic::{AtomicBool, Ordering};
    static FIRST: AtomicBool = AtomicBool::new(false);
    if painted && !FIRST.swap(true, Ordering::AcqRel) {
        true
    } else {
        false
    }
}

#[cfg(target_env = "ohos")]
fn editor_frame(
    ctx: &egui::Context,
    app: &mut EffectcraftApp,
    raw: egui::RawInput,
    frame: &mut eframe::Frame,
) -> egui::FullOutput {
    // 与上游 eframe 宿主一致：每帧的逻辑拍（`logic`，含逐帧 tick / GPU 合成器初始化 / 时间线推进）先于 UI 拍。
    // `run_ui` 会跑 logic-pass（pass 0）+ 完整渲染 pass（pass 1）：pass 0 跑 logic，各 pass 跑 ui。
    ctx.run_ui(raw, |ui| {
        if ui.ctx().current_pass_index() == 0 {
            app.logic(ctx, frame);
        }
        app.ui(ui, frame);
    })
}

#[cfg(target_env = "ohos")]
#[derive(Debug, PartialEq, Eq)]
enum CloseAction {
    None,
    Request,
    Cancel,
    Confirm,
}

/// EffectCraft 自管未保存工程：`panels::unsaved::on_close_requested` 检测 `close_requested()` 后，
/// 脏工程弹「保存更改？」并发 `ViewportCommand::CancelClose`；用户选 Save / Don't Save 后发
/// `ViewportCommand::Close`。宿主只看这两个命令（与 FilmCraft / DeckCraft 同一套机制）。
#[cfg(target_env = "ohos")]
fn close_action(validating: bool, commands: &[egui::ViewportCommand]) -> CloseAction {
    if commands.contains(&egui::ViewportCommand::CancelClose) {
        CloseAction::Cancel
    } else if validating {
        CloseAction::Confirm
    } else if commands.contains(&egui::ViewportCommand::Close) {
        CloseAction::Request
    } else {
        CloseAction::None
    }
}

/// 宿主 hook（OHOS）：文件对话框走 ArkTS 系统选择器（非阻塞），系统主题走 appearance 模块。
#[cfg(target_env = "ohos")]
fn hooks(files: &str, _system_locale: &str) -> Hooks {
    let mut h = Hooks::default();
    // 系统主题（跟随系统）：appearance 模块已知当前深浅，返回 Some(Theme) / None。
    h.system_theme = Some(Box::new(|_ctx: &egui::Context| appearance::current()));
    // 命令主路径：非阻塞文件对话框。另开线程与 ArkTS 交互，完成后回调 done（done 为 FnOnce + Send）。
    h.file_dialog_async = Some(Box::new(|dialog: FileDialog, done: DialogDone| {
        let _ = std::thread::Builder::new().name("ec-file-dialog".into()).spawn(move || {
            done(request_file_dialog(dialog));
        });
    }));
    // 阻塞回退（Settings 等少数直接调用点）。
    h.pick_files = Some(Box::new(|exts: &[&str]| {
        let req = serde_json::json!({ "exts": exts.to_vec(), "many": true }).to_string();
        pick_paths("openfiles", &req).unwrap_or_default()
    }));
    h.pick_open_project = Some(Box::new(|| pick_one("openproject", "")));
    h.pick_save = Some(Box::new(|name: &str| pick_one("savefile", &serde_json::json!({ "default": name }).to_string())));
    h.pick_save_file = Some(Box::new(|default: &str, ext: &str| {
        pick_one("savefile", &serde_json::json!({ "default": default, "ext": ext }).to_string())
    }));
    h.pick_folder = Some(Box::new(|| pick_one("pickfolder", "")));
    // 音频输出（cpal / OHOS 音频）首版不接：预览静音，编辑 / 合成不受影响。
    h.audio_device = None;
    h.audio_devices = None;
    // 设备丢失：告知宿主（随后上游发 ViewportCommand::Close 走关窗路径）。
    h.device_lost = Some(Box::new(|saved: Option<Result<String, String>>| {
        let text = match saved {
            None => "graphics device lost".to_string(),
            Some(Ok(p)) => format!("graphics device lost (auto-saved {p})"),
            Some(Err(e)) => format!("graphics device lost ({e})"),
        };
        bridge::notify(0, "device_lost", &text, &[]);
    }));
    h.before_gpu_build = None;
    let _ = files;
    h
}

/// 按 EffectCraft 的 `FileDialog` 与 ArkTS 交互，回包为路径数组（空 = 取消）。
#[cfg(target_env = "ohos")]
fn request_file_dialog(dialog: FileDialog) -> Result<Vec<String>, String> {
    match dialog {
        FileDialog::Open { exts, many } => {
            let req = serde_json::json!({ "exts": exts, "many": many }).to_string();
            pick_paths("openfiles", &req)
        }
        FileDialog::OpenProject => pick_paths("openproject", ""),
        FileDialog::Folder => pick_paths("pickfolder", ""),
        FileDialog::Save { default, ext, .. } => {
            let req = serde_json::json!({ "default": default, "ext": ext }).to_string();
            pick_paths("savefile", &req)
        }
    }
}

/// 阻塞等待 ArkTS 回包，返回路径数组。取消（空 name）返回空数组。
#[cfg(target_env = "ohos")]
fn pick_paths(kind: &str, req: &str) -> Result<Vec<String>, String> {
    match bridge::request(kind, req, &[]) {
        Ok((name, _bytes)) => {
            let name = name.trim();
            if name.is_empty() {
                return Ok(Vec::new());
            }
            if let Ok(arr) = serde_json::from_str::<Vec<String>>(name) {
                return Ok(arr);
            }
            Ok(vec![name.to_string()])
        }
        Err(e) => {
            if e.to_ascii_lowercase().contains("cancel") {
                Ok(Vec::new())
            } else {
                log::warn!("[ec] pick {kind} failed: {e}");
                Err(e)
            }
        }
    }
}

/// 单选 / 单路径：取路径数组的第一个。
#[cfg(target_env = "ohos")]
fn pick_one(kind: &str, req: &str) -> Option<String> {
    pick_paths(kind, req).ok().and_then(|v| v.into_iter().next())
}

/// 把落沙箱的文件路径交给上游 `file.import`。失败只记日志、不中断帧循环。
#[cfg(target_env = "ohos")]
fn open_into_app(app: &mut EffectcraftApp, path: &str) {
    match app.session.execute("file.import", serde_json::json!({ "paths": [path] })) {
        Ok(v) => {
            let s = v.to_string();
            let s = s.chars().take(120).collect::<String>();
            log::info!("[ec] import {path}: {s}");
        }
        Err(e) => {
            log::warn!("[ec] import failed for {path}: {e}");
            bridge::notify(0, "error", &format!("import: {e}"), &[]);
        }
    }
}

#[cfg(not(target_env = "ohos"))]
fn worker(_config: Config, _rx: mpsc::Receiver<Message>) {
    bridge::notify(0, "error", "Native host requires an OHOS target", &[]);
}
