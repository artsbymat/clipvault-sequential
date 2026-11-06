//! Session-only sequential clipboard. This binary is independent of clipvault's CLI/database.

use std::{
    collections::VecDeque,
    env, fs,
    io::{self, Cursor, Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use eframe::egui::{
    self, Align, Color32, CornerRadius, CursorIcon, Frame, Id, LayerId, Layout, Margin, Order,
    Pos2, Response, RichText, Sense, Stroke, TextureHandle, Vec2,
};
use evdev::{Device, EventSummary, KeyCode};

const APP_NAME: &str = "Clipvault Sequential";
const BG: Color32 = Color32::from_rgb(20, 22, 20);
const SURFACE: Color32 = Color32::from_rgb(29, 32, 29);
const SURFACE_RAISED: Color32 = Color32::from_rgb(37, 41, 36);
const ACTIVE_SURFACE: Color32 = Color32::from_rgb(40, 47, 39);
const BORDER: Color32 = Color32::from_rgb(55, 61, 53);
const ACCENT: Color32 = Color32::from_rgb(218, 174, 92);
const ACCENT_SOFT: Color32 = Color32::from_rgb(184, 148, 82);
const TEXT: Color32 = Color32::from_rgb(232, 229, 218);
const MUTED: Color32 = Color32::from_rgb(151, 154, 143);
const FAINT: Color32 = Color32::from_rgb(100, 105, 96);
const DANGER: Color32 = Color32::from_rgb(222, 119, 110);

fn main() {
    configure_allocator();
    if let Err(error) = run() {
        eprintln!("{APP_NAME}: {error}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "linux")]
fn configure_allocator() {
    unsafe extern "C" {
        fn mallopt(param: std::ffi::c_int, value: std::ffi::c_int) -> std::ffi::c_int;
    }
    // M_ARENA_MAX = -8: Limit glibc memory arenas to 2, preventing multi-threaded heap fragmentation
    const M_ARENA_MAX: std::ffi::c_int = -8;
    // M_TRIM_THRESHOLD = -1: Aggressively release pages back to OS when free memory exceeds 128KB
    const M_TRIM_THRESHOLD: std::ffi::c_int = -1;
    unsafe {
        mallopt(M_ARENA_MAX, 2);
        mallopt(M_TRIM_THRESHOLD, 128 * 1024);
    }
}

#[cfg(not(target_os = "linux"))]
fn configure_allocator() {}

fn trim_memory() {
    #[cfg(target_os = "linux")]
    {
        unsafe extern "C" {
            fn malloc_trim(pad: usize) -> std::ffi::c_int;
        }
        unsafe {
            malloc_trim(0);
        }
    }
}

fn spawn_thread<F, T>(name: &'static str, f: F) -> io::Result<thread::JoinHandle<T>>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    thread::Builder::new()
        .name(name.to_string())
        .stack_size(256 * 1024)
        .spawn(f)
}

fn probe_image_dimensions(data: &[u8]) -> Option<(u32, u32)> {
    image::ImageReader::new(Cursor::new(data))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = env::args_os().collect();
    if args.get(1).is_some_and(|arg| arg == "--capture") {
        return capture_worker(Path::new(args.get(2).ok_or("missing capture socket")?));
    }
    ensure_command("wl-paste")?;
    ensure_command("wl-copy")?;

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(APP_NAME)
            .with_inner_size([440.0, 620.0])
            .with_min_inner_size([360.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        APP_NAME,
        options,
        Box::new(|cc| Ok(Box::new(SequentialApp::new(cc)?))),
    )?;
    Ok(())
}

fn ensure_command(name: &str) -> io::Result<()> {
    let found = env::var_os("PATH")
        .is_some_and(|paths| env::split_paths(&paths).any(|path| path.join(name).is_file()));
    found.then_some(()).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            format!("`{name}` was not found; install wl-clipboard first"),
        )
    })
}

fn capture_worker(socket: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if matches!(env::var("CLIPBOARD_STATE").as_deref(), Ok("nil" | "clear")) {
        return Ok(());
    }
    let mut data = Vec::new();
    io::stdin().read_to_end(&mut data)?;
    if data.is_empty() || std::str::from_utf8(&data).is_ok_and(|text| text.trim().is_empty()) {
        return Ok(());
    }
    UnixStream::connect(socket)?.write_all(&data)?;
    Ok(())
}

struct ClipboardItem {
    data: Arc<[u8]>,
    preview: Preview,
    texture: Option<TextureHandle>,
    detected_colors: Vec<DetectedColor>,
    is_pure_color: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct DetectedColor {
    color: Color32,
    raw: String,
    hex: String,
    rgb_str: String,
    hsl_str: String,
}

enum Preview {
    Text(String),
    Image { width: u32, height: u32 },
    Binary(usize),
}

impl ClipboardItem {
    fn new(data: Arc<[u8]>) -> Self {
        let (preview, detected_colors, is_pure_color) = if let Ok(text) = std::str::from_utf8(&data) {
            let (colors, is_pure) = extract_colors(text);
            let compact = text.split_whitespace().collect::<Vec<_>>().join(" ");
            let mut chars = compact.chars();
            let mut short: String = chars.by_ref().take(260).collect();
            if chars.next().is_some() {
                short.push('…');
            }
            (Preview::Text(short), colors, is_pure)
        } else if let Some((width, height)) = probe_image_dimensions(&data) {
            (
                Preview::Image { width, height },
                Vec::new(),
                false,
            )
        } else {
            (Preview::Binary(data.len()), Vec::new(), false)
        };
        Self {
            data,
            preview,
            texture: None,
            detected_colors,
            is_pure_color,
        }
    }

    fn kind(&self) -> &'static str {
        if self.is_pure_color {
            "COLOR"
        } else {
            match self.preview {
                Preview::Text(_) => "TEXT",
                Preview::Image { .. } => "IMAGE",
                Preview::Binary(_) => "DATA",
            }
        }
    }
}

struct SequentialApp {
    state: Arc<Mutex<QueueState>>,
    watcher: Child,
    socket_path: PathBuf,
    last_trim: SystemTime,
}

#[derive(Default)]
struct QueueState {
    items: VecDeque<ClipboardItem>,
    suppress_next: Option<Arc<[u8]>>,
    paste_in_flight: bool,
    paused: bool,
    input_error: Option<String>,
    last_error: Option<String>,
}

#[derive(Clone, Copy)]
enum MoveAction {
    Up(usize),
    Down(usize),
    Delete(usize),
    Reorder { from: usize, to: usize },
}

#[derive(Clone, Copy)]
enum ArrowIcon {
    Up,
    Down,
    Both,
}

#[derive(Clone, Copy)]
enum CaptureIcon {
    Pause,
    Resume,
}

impl SequentialApp {
    fn new(cc: &eframe::CreationContext<'_>) -> io::Result<Self> {
        configure_style(&cc.egui_ctx);
        let state = Arc::new(Mutex::new(QueueState::default()));
        let socket_path = unique_socket_path();
        let listener = UnixListener::bind(&socket_path)?;
        spawn_clipboard_listener(listener, state.clone(), cc.egui_ctx.clone());
        spawn_keyboard_listeners(state.clone(), cc.egui_ctx.clone());

        let watcher = Command::new("wl-paste")
            .arg("--watch")
            .arg(env::current_exe()?)
            .arg("--capture")
            .arg(&socket_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        Ok(Self {
            state,
            watcher,
            socket_path,
            last_trim: SystemTime::now(),
        })
    }

    fn item_ui(
        ui: &mut egui::Ui,
        item: &mut ClipboardItem,
        index: usize,
        total: usize,
        controls_enabled: bool,
    ) -> Option<MoveAction> {
        let active = index == 0;
        let mut action = None;
        let is_self_dragged =
            egui::DragAndDrop::payload::<usize>(ui.ctx()).is_some_and(|p| *p == index);

        let card_res = Frame::new()
            .fill(if is_self_dragged {
                Color32::from_rgba_unmultiplied(29, 32, 29, 130)
            } else if active {
                ACTIVE_SURFACE
            } else {
                SURFACE
            })
            .stroke(Stroke::new(
                1.0_f32,
                if is_self_dragged {
                    BORDER
                } else if active {
                    ACCENT_SOFT
                } else {
                    BORDER
                },
            ))
            .corner_radius(CornerRadius::same(14))
            .inner_margin(Margin::same(14))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let handle_res = drag_handle(ui, controls_enabled);
                    if controls_enabled {
                        handle_res.dnd_set_drag_payload(index);
                    }

                    let badge_sense = if controls_enabled {
                        Sense::drag()
                    } else {
                        Sense::hover()
                    };
                    let badge_res = ui
                        .add(
                            egui::Label::new(
                                RichText::new(if active {
                                    "NEXT".into()
                                } else {
                                    (index + 1).to_string()
                                })
                                .size(10.0)
                                .strong()
                                .color(if active { ACCENT } else { MUTED }),
                            )
                            .sense(badge_sense),
                        )
                        .on_hover_cursor(CursorIcon::Grab);
                    if controls_enabled {
                        badge_res.dnd_set_drag_payload(index);
                    }

                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if trash_button(ui, controls_enabled, false)
                            .on_hover_text("Delete item")
                            .clicked()
                        {
                            action = Some(MoveAction::Delete(index));
                        }
                        if arrow_button(
                            ui,
                            ArrowIcon::Down,
                            controls_enabled && index + 1 < total,
                            false,
                        )
                        .on_hover_text("Move item down")
                        .clicked()
                        {
                            action = Some(MoveAction::Down(index));
                        }
                        if arrow_button(ui, ArrowIcon::Up, controls_enabled && index > 0, false)
                            .on_hover_text("Move item up")
                            .clicked()
                        {
                            action = Some(MoveAction::Up(index));
                        }
                        ui.label(RichText::new(item.kind()).size(9.0).color(FAINT));
                    });
                });
                ui.add_space(9.0);
                match &item.preview {
                    Preview::Text(text) => {
                        if item.is_pure_color && !item.detected_colors.is_empty() {
                            draw_color_row(ui, &item.detected_colors[0]);
                        } else {
                            ui.label(RichText::new(text).size(14.0).color(TEXT));
                            if !item.detected_colors.is_empty() {
                                ui.add_space(6.0);
                                for color_info in &item.detected_colors {
                                    draw_color_row(ui, color_info);
                                }
                            }
                        }
                    }
                    Preview::Image { width, height } => {
                        if item.texture.is_none() {
                            if let Ok(reader) = image::ImageReader::new(Cursor::new(&item.data)).with_guessed_format() {
                                if let Ok(decoded) = reader.decode() {
                                    // Generate a compact thumbnail (max 420x160) instead of loading full resolution into GPU/RAM
                                    let thumb = decoded.thumbnail(420, 160);
                                    let rgba = thumb.to_rgba8();
                                    let size = [rgba.width() as usize, rgba.height() as usize];
                                    item.texture = Some(ui.ctx().load_texture(
                                        format!("clipboard-image-{}", item.data.as_ptr() as usize),
                                        egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw()),
                                        egui::TextureOptions::LINEAR,
                                    ));
                                    drop(decoded);
                                    drop(thumb);
                                    drop(rgba);
                                    trim_memory();
                                }
                            }
                        }
                        if let Some(texture) = &item.texture {
                            let scale = (ui.available_width() / *width as f32)
                                .min(150.0 / *height as f32)
                                .min(1.0);
                            ui.add(egui::Image::new(texture).fit_to_exact_size(Vec2::new(
                                *width as f32 * scale,
                                *height as f32 * scale,
                            )));
                        }
                        ui.label(
                            RichText::new(format!("{width} × {height}"))
                                .size(11.0)
                                .color(MUTED),
                        );
                    }
                    Preview::Binary(bytes) => {
                        ui.label(
                            RichText::new(format!("Binary data · {bytes} bytes"))
                                .size(13.0)
                                .color(MUTED),
                        );
                    }
                }
            });

        if controls_enabled {
            let drop_id = ui.id().with("card_drop").with(index);
            let drop_res = ui.interact(card_res.response.rect, drop_id, Sense::hover());

            if let Some(from_arc) = drop_res.dnd_hover_payload::<usize>() {
                if *from_arc != index {
                    ui.painter().rect_stroke(
                        card_res.response.rect,
                        CornerRadius::same(14),
                        Stroke::new(2.0_f32, ACCENT),
                        egui::StrokeKind::Inside,
                    );
                }
            }

            if let Some(from_arc) = drop_res.dnd_release_payload::<usize>() {
                let from = *from_arc;
                if from != index {
                    action = Some(MoveAction::Reorder { from, to: index });
                }
            }
        }
        action
    }
}

impl eframe::App for SequentialApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if self.last_trim.elapsed().is_ok_and(|d| d >= Duration::from_secs(5)) {
            self.last_trim = SystemTime::now();
            trim_memory();
        }

        let mut reverse_clicked = false;
        let mut clear_clicked = false;
        let mut pause_clicked = false;
        let mut move_action = None;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        egui::CentralPanel::default()
            .frame(Frame::new().fill(BG).inner_margin(Margin::same(20)))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Clipboard").size(25.0).strong().color(TEXT));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        clear_clicked = trash_button(
                            ui,
                            !state.items.is_empty() && !state.paste_in_flight,
                            true,
                        )
                        .on_hover_text("Delete all items")
                        .clicked();
                        pause_clicked = capture_button(
                            ui,
                            if state.paused {
                                CaptureIcon::Resume
                            } else {
                                CaptureIcon::Pause
                            },
                            true,
                            state.paused,
                        )
                        .on_hover_text(if state.paused {
                            "Resume capture"
                        } else {
                            "Pause capture"
                        })
                        .clicked();
                        reverse_clicked = arrow_button(
                            ui,
                            ArrowIcon::Both,
                            state.items.len() > 1 && !state.paste_in_flight,
                            true,
                        )
                        .on_hover_text("Reverse order")
                        .clicked();
                        Frame::new()
                            .fill(SURFACE_RAISED)
                            .corner_radius(CornerRadius::same(10))
                            .inner_margin(Margin::symmetric(11, 7))
                            .show(ui, |ui| {
                                ui.label(
                                    RichText::new(format!("{} items", state.items.len()))
                                        .size(11.0)
                                        .color(MUTED),
                                );
                            });
                    });
                });
                ui.add_space(18.0);
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if state.items.is_empty() {
                            ui.add_space(95.0);
                            ui.vertical_centered(|ui| {
                                ui.label(
                                    RichText::new("No items yet")
                                        .size(16.0)
                                        .strong()
                                        .color(TEXT),
                                );
                            });
                        } else {
                            let total = state.items.len();
                            let controls_enabled = !state.paste_in_flight;
                            for (index, item) in state.items.iter_mut().enumerate() {
                                let clicked =
                                    Self::item_ui(ui, item, index, total, controls_enabled);
                                if move_action.is_none() {
                                    move_action = clicked;
                                }
                                ui.add_space(10.0);
                            }
                        }
                    });
                ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
                    if let Some(error) = &state.last_error {
                        ui.label(RichText::new(error).size(11.0).color(DANGER));
                    }
                    if let Some(error) = &state.input_error {
                        ui.label(RichText::new(error).size(10.0).color(DANGER));
                    }
                });
            });

        if let Some(dragged_idx) = egui::DragAndDrop::payload::<usize>(ctx) {
            if let Some(pointer_pos) = ctx.pointer_interact_pos() {
                let painter =
                    ctx.layer_painter(LayerId::new(Order::Tooltip, Id::new("dnd_ghost")));
                let text = format!("Move item #{}", *dragged_idx + 1);
                let galley = painter.layout_no_wrap(text, egui::FontId::proportional(12.0), TEXT);
                let padding = Vec2::new(9.0, 5.0);
                let size = galley.size() + padding * 2.0;
                let rect = egui::Rect::from_min_size(pointer_pos + Vec2::new(14.0, 14.0), size);
                painter.rect_filled(rect, CornerRadius::same(6), SURFACE_RAISED);
                painter.rect_stroke(
                    rect,
                    CornerRadius::same(6),
                    Stroke::new(1.0_f32, ACCENT),
                    egui::StrokeKind::Inside,
                );
                painter.galley(rect.min + padding, galley, TEXT);
            }
        }

        if pause_clicked {
            state.paused = !state.paused;
        }
        drop(state);

        if clear_clicked {
            let state = self.state.clone();
            let context = ctx.clone();
            let _ = spawn_thread("clipvault-clear", move || {
                clear_queue(&state);
                context.request_repaint();
            });
        } else if reverse_clicked {
            let state = self.state.clone();
            let context = ctx.clone();
            let _ = spawn_thread("clipvault-reverse", move || {
                reverse_queue(&state);
                context.request_repaint();
            });
        } else if let Some(MoveAction::Delete(index)) = move_action {
            let state = self.state.clone();
            let context = ctx.clone();
            let _ = spawn_thread("clipvault-delete", move || {
                delete_queue_item(&state, index);
                context.request_repaint();
            });
        } else if let Some(action) = move_action {
            let state = self.state.clone();
            let context = ctx.clone();
            let _ = spawn_thread("clipvault-move", move || {
                move_queue_item(&state, action);
                context.request_repaint();
            });
        }
    }
}

fn drag_handle(ui: &mut egui::Ui, enabled: bool) -> Response {
    let size = Vec2::new(12.0, 18.0);
    let sense = if enabled {
        Sense::drag()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(size, sense);
    let response = response.on_hover_cursor(CursorIcon::Grab);

    let is_dragged = response.dragged();
    let is_hovered = response.hovered();

    let color = if enabled {
        if is_dragged || is_hovered {
            ACCENT
        } else {
            MUTED
        }
    } else {
        FAINT
    };

    let center = rect.center();
    let painter = ui.painter();
    for col in [-2.5, 2.5] {
        for row in [-4.5, 0.0, 4.5] {
            painter.circle_filled(
                Pos2::new(center.x + col, center.y + row),
                1.3,
                color,
            );
        }
    }

    response.on_hover_text("Drag to reorder")
}

fn arrow_button(ui: &mut egui::Ui, icon: ArrowIcon, enabled: bool, framed: bool) -> Response {
    let size = if framed { 30.0 } else { 22.0 };
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), sense);
    let hovered = enabled && response.hovered();

    if framed || hovered {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(if framed { 10 } else { 6 }),
            if hovered {
                Color32::from_rgb(51, 57, 49)
            } else {
                SURFACE_RAISED
            },
        );
    }

    let color = if enabled { TEXT } else { FAINT };
    let stroke = Stroke::new(1.6_f32, color);
    let center = rect.center();
    let top = center.y - 6.0;
    let bottom = center.y + 6.0;
    let painter = ui.painter();

    match icon {
        ArrowIcon::Up => {
            painter.line_segment(
                [Pos2::new(center.x, bottom), Pos2::new(center.x, top)],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(center.x, top),
                    Pos2::new(center.x - 4.0, top + 4.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(center.x, top),
                    Pos2::new(center.x + 4.0, top + 4.0),
                ],
                stroke,
            );
        }
        ArrowIcon::Down => {
            painter.line_segment(
                [Pos2::new(center.x, top), Pos2::new(center.x, bottom)],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(center.x, bottom),
                    Pos2::new(center.x - 4.0, bottom - 4.0),
                ],
                stroke,
            );
            painter.line_segment(
                [
                    Pos2::new(center.x, bottom),
                    Pos2::new(center.x + 4.0, bottom - 4.0),
                ],
                stroke,
            );
        }
        ArrowIcon::Both => {
            painter.line_segment(
                [Pos2::new(center.x, bottom), Pos2::new(center.x, top)],
                stroke,
            );
            for side in [-4.0, 4.0] {
                painter.line_segment(
                    [
                        Pos2::new(center.x, top),
                        Pos2::new(center.x + side, top + 4.0),
                    ],
                    stroke,
                );
                painter.line_segment(
                    [
                        Pos2::new(center.x, bottom),
                        Pos2::new(center.x + side, bottom - 4.0),
                    ],
                    stroke,
                );
            }
        }
    }

    response
}

fn capture_button(ui: &mut egui::Ui, icon: CaptureIcon, enabled: bool, active: bool) -> Response {
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(30.0), sense);
    let hovered = enabled && response.hovered();
    ui.painter().rect_filled(
        rect,
        CornerRadius::same(10),
        if active {
            Color32::from_rgb(61, 51, 34)
        } else if hovered {
            Color32::from_rgb(51, 57, 49)
        } else {
            SURFACE_RAISED
        },
    );

    let color = if active { ACCENT } else { TEXT };
    let center = rect.center();
    let painter = ui.painter();
    match icon {
        CaptureIcon::Pause => {
            painter.rect_filled(
                egui::Rect::from_center_size(
                    Pos2::new(center.x - 3.0, center.y),
                    Vec2::new(2.2, 11.0),
                ),
                CornerRadius::same(1),
                color,
            );
            painter.rect_filled(
                egui::Rect::from_center_size(
                    Pos2::new(center.x + 3.0, center.y),
                    Vec2::new(2.2, 11.0),
                ),
                CornerRadius::same(1),
                color,
            );
        }
        CaptureIcon::Resume => {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(center.x - 4.0, center.y - 6.0),
                    Pos2::new(center.x + 6.0, center.y),
                    Pos2::new(center.x - 4.0, center.y + 6.0),
                ],
                color,
                Stroke::NONE,
            ));
        }
    }
    response
}

fn trash_button(ui: &mut egui::Ui, enabled: bool, framed: bool) -> Response {
    let size = if framed { 30.0 } else { 22.0 };
    let sense = if enabled {
        Sense::click()
    } else {
        Sense::hover()
    };
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(size), sense);
    let hovered = enabled && response.hovered();
    if framed || hovered {
        ui.painter().rect_filled(
            rect,
            CornerRadius::same(if framed { 10 } else { 6 }),
            if hovered {
                Color32::from_rgb(66, 45, 42)
            } else {
                SURFACE_RAISED
            },
        );
    }

    let color = if enabled {
        if hovered { DANGER } else { MUTED }
    } else {
        FAINT
    };
    let stroke = Stroke::new(1.5_f32, color);
    let center = rect.center();
    let painter = ui.painter();
    painter.line_segment(
        [
            Pos2::new(center.x - 5.0, center.y - 4.0),
            Pos2::new(center.x + 5.0, center.y - 4.0),
        ],
        stroke,
    );
    painter.line_segment(
        [
            Pos2::new(center.x - 2.5, center.y - 6.5),
            Pos2::new(center.x + 2.5, center.y - 6.5),
        ],
        stroke,
    );
    painter.rect_stroke(
        egui::Rect::from_min_max(
            Pos2::new(center.x - 4.0, center.y - 2.0),
            Pos2::new(center.x + 4.0, center.y + 6.0),
        ),
        CornerRadius::same(1),
        stroke,
        egui::StrokeKind::Inside,
    );
    response
}

impl Drop for SequentialApp {
    fn drop(&mut self) {
        let _ = self.watcher.kill();
        let _ = self.watcher.wait();
        let _ = fs::remove_file(&self.socket_path);
    }
}

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.visuals.dark_mode = true;
    style.visuals.panel_fill = BG;
    style.visuals.window_corner_radius = CornerRadius::same(18);
    style.visuals.override_text_color = Some(TEXT);
    style.visuals.selection.bg_fill = ACCENT_SOFT;
    style.visuals.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    style.visuals.hyperlink_color = ACCENT;
    style.visuals.widgets.inactive.bg_fill = SURFACE_RAISED;
    style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(51, 57, 49);
    style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, ACCENT_SOFT);
    ctx.set_style(style);
}

fn unique_socket_path() -> PathBuf {
    let runtime = env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir);
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    runtime.join(format!(
        "clipvault-sequential-{}-{nonce}.sock",
        std::process::id()
    ))
}

fn spawn_clipboard_listener(
    listener: UnixListener,
    state: Arc<Mutex<QueueState>>,
    context: egui::Context,
) {
    let _ = spawn_thread("clipvault-clipboard", move || {
        for connection in listener.incoming() {
            let Ok(mut stream) = connection else {
                continue;
            };
            let mut data = Vec::new();
            if stream.read_to_end(&mut data).is_ok() && !data.is_empty() {
                let data: Arc<[u8]> = Arc::from(data.into_boxed_slice());
                {
                    let mut state = state
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    if state.suppress_next.as_deref() == Some(&*data) {
                        state.suppress_next = None;
                        continue;
                    }
                    if state.paused {
                        continue;
                    }
                }
                let item = ClipboardItem::new(data.clone());
                let mut state = state
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if state.paused {
                    continue;
                }
                if state.suppress_next.as_deref() == Some(&*data) {
                    state.suppress_next = None;
                } else {
                    state.items.push_front(item);
                }
                drop(state);
                context.request_repaint();
            }
        }
    });
}

fn spawn_keyboard_listeners(state: Arc<Mutex<QueueState>>, context: egui::Context) {
    let _ = spawn_thread("clipvault-key-enum", move || {
        let keyboards: Vec<_> = evdev::enumerate()
            .filter(|(_, device)| is_keyboard(device))
            .collect();
        if keyboards.is_empty() {
            state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .input_error =
                Some("Cannot read the evdev keyboard — check /dev/input/event* permissions".into());
            context.request_repaint();
            return;
        }
        for (_, device) in keyboards {
            let state = state.clone();
            let context = context.clone();
            let _ = spawn_thread("clipvault-key-watch", move || {
                watch_keyboard(device, state, context);
            });
        }
    });
}

fn is_keyboard(device: &Device) -> bool {
    device.supported_keys().is_some_and(|keys| {
        keys.contains(KeyCode::KEY_V)
            && keys.contains(KeyCode::KEY_LEFTCTRL)
            && keys.contains(KeyCode::KEY_SPACE)
    })
}

fn watch_keyboard(mut device: Device, state: Arc<Mutex<QueueState>>, context: egui::Context) {
    let (mut left_ctrl, mut right_ctrl, mut alt, mut meta, mut armed) =
        (false, false, false, false, false);
    loop {
        let events = match device.fetch_events() {
            Ok(events) => events,
            Err(_) => return,
        };
        for event in events {
            let EventSummary::Key(_, key, value) = event.destructure() else {
                continue;
            };
            let pressed = value != 0;
            match key {
                KeyCode::KEY_LEFTCTRL => left_ctrl = pressed,
                KeyCode::KEY_RIGHTCTRL => right_ctrl = pressed,
                KeyCode::KEY_LEFTALT | KeyCode::KEY_RIGHTALT => alt = pressed,
                KeyCode::KEY_LEFTMETA | KeyCode::KEY_RIGHTMETA => meta = pressed,
                KeyCode::KEY_V if value == 1 => {
                    armed = (left_ctrl || right_ctrl) && !alt && !meta;
                }
                KeyCode::KEY_V if value == 0 && armed => {
                    armed = false;
                    // Some clients request clipboard data after the key-up
                    // event. Keep the current owner alive until that transfer
                    // has had enough time to begin.
                    thread::sleep(Duration::from_millis(220));
                    advance_clipboard(&state);
                    context.request_repaint();
                }
                _ => {}
            }
        }
    }
}

fn advance_clipboard(shared: &Arc<Mutex<QueueState>>) {
    let (current, next) = {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.items.is_empty() || state.paste_in_flight {
            return;
        }
        state.paste_in_flight = true;
        let current = state.items[0].data.clone();
        let next = state.items.get(1).map(|item| item.data.clone());
        state.suppress_next.clone_from(&next);
        (current, next)
    };

    // Never hold the state lock while invoking an external process. wl-copy
    // can wait on the compositor, and the GUI must remain free to render.
    let result = next
        .as_deref()
        .map_or_else(clear_clipboard, write_clipboard);

    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.paste_in_flight = false;
    match result {
        Ok(()) => {
            // A new user copy may have arrived while wl-copy was running. Do
            // not remove that newer item by mistake.
            if state
                .items
                .front()
                .is_some_and(|item| Arc::ptr_eq(&item.data, &current) || *item.data == *current)
            {
                state.items.pop_front();
                trim_memory();
            }
            state.last_error = None;
        }
        Err(error) => {
            state.suppress_next = None;
            state.last_error = Some(format!("Failed to update the clipboard: {error}"));
        }
    }
}

fn delete_queue_item(shared: &Arc<Mutex<QueueState>>, index: usize) {
    let (current, next) = {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.paste_in_flight || index >= state.items.len() {
            return;
        }
        if index > 0 {
            state.items.remove(index);
            state.last_error = None;
            trim_memory();
            return;
        }

        state.paste_in_flight = true;
        let current = state.items[0].data.clone();
        let next = state.items.get(1).map(|item| item.data.clone());
        state.suppress_next.clone_from(&next);
        (current, next)
    };

    let result = next
        .as_deref()
        .map_or_else(clear_clipboard, write_clipboard);
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.paste_in_flight = false;
    match result {
        Ok(()) => {
            if let Some(position) = state
                .items
                .iter()
                .position(|item| Arc::ptr_eq(&item.data, &current) || *item.data == *current)
            {
                state.items.remove(position);
                trim_memory();
            }
            state.last_error = None;
        }
        Err(error) => {
            state.suppress_next = None;
            state.last_error = Some(format!("Failed to delete the item: {error}"));
        }
    }
}

fn clear_queue(shared: &Arc<Mutex<QueueState>>) {
    {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.items.is_empty() || state.paste_in_flight {
            return;
        }
        state.paste_in_flight = true;
        state.suppress_next = None;
    }

    let result = clear_clipboard();
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.paste_in_flight = false;
    match result {
        Ok(()) => {
            state.items.clear();
            state.last_error = None;
            trim_memory();
        }
        Err(error) => state.last_error = Some(format!("Failed to delete all items: {error}")),
    }
}

fn move_queue_item(shared: &Arc<Mutex<QueueState>>, action: MoveAction) {
    let (from, to) = match action {
        MoveAction::Up(index) if index > 0 => (index, index - 1),
        MoveAction::Down(index) => (index, index + 1),
        MoveAction::Reorder { from, to } if from != to => (from, to),
        _ => return,
    };

    let new_front = {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.paste_in_flight || from >= state.items.len() || to >= state.items.len() {
            return;
        }
        let old_front = state.items[0].data.clone();
        if let Some(item) = state.items.remove(from) {
            state.items.insert(to, item);
        }
        let new_front = state.items[0].data.clone();
        if Arc::ptr_eq(&new_front, &old_front) || *new_front == *old_front {
            state.last_error = None;
            return;
        }
        state.paste_in_flight = true;
        state.suppress_next = Some(new_front.clone());
        new_front
    };

    let result = write_clipboard(&new_front);
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.paste_in_flight = false;
    match result {
        Ok(()) => state.last_error = None,
        Err(error) => {
            if from < state.items.len()
                && to < state.items.len()
                && state
                    .items
                    .front()
                    .is_some_and(|item| Arc::ptr_eq(&item.data, &new_front) || *item.data == *new_front)
            {
                if let Some(item) = state.items.remove(to) {
                    state.items.insert(from, item);
                }
            }
            state.suppress_next = None;
            state.last_error = Some(format!("Failed to move the item: {error}"));
        }
    }
}

fn reverse_queue(shared: &Arc<Mutex<QueueState>>) {
    let data = {
        let mut state = shared
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.items.len() < 2 || state.paste_in_flight {
            return;
        }
        state.paste_in_flight = true;
        state.items.make_contiguous().reverse();
        let data = state.items[0].data.clone();
        state.suppress_next = Some(data.clone());
        data
    };

    let result = write_clipboard(&data);
    let mut state = shared
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.paste_in_flight = false;
    match result {
        Ok(()) => state.last_error = None,
        Err(error) => {
            // Restore the previous order when the compositor rejects the new
            // active clipboard.
            state.items.make_contiguous().reverse();
            state.suppress_next = None;
            state.last_error = Some(format!("Failed to reverse the queue: {error}"));
        }
    }
}

fn write_clipboard(data: &[u8]) -> io::Result<()> {
    let mut child = Command::new("wl-copy").stdin(Stdio::piped()).spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "wl-copy stdin unavailable"))?
        .write_all(data)?;
    let status = child.wait()?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| io::Error::other(format!("wl-copy exited with {status}")))
}

fn clear_clipboard() -> io::Result<()> {
    let status = Command::new("wl-copy").arg("--clear").status()?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| io::Error::other(format!("wl-copy --clear exited with {status}")))
}

impl DetectedColor {
    fn new(r: u8, g: u8, b: u8, a: u8, raw: String) -> Self {
        let color = Color32::from_rgba_unmultiplied(r, g, b, a);
        let hex = if a == 255 {
            format!("#{r:02X}{g:02X}{b:02X}")
        } else {
            format!("#{r:02X}{g:02X}{b:02X}{a:02X}")
        };
        let rgb_str = if a == 255 {
            format!("rgb({r}, {g}, {b})")
        } else {
            let alpha = (a as f32 / 255.0 * 100.0).round() / 100.0;
            format!("rgba({r}, {g}, {b}, {alpha})")
        };
        let (h, s, l) = rgb_to_hsl(r, g, b);
        let hsl_str = if a == 255 {
            format!("hsl({h:.0}, {s:.0}%, {l:.0}%)")
        } else {
            let alpha = (a as f32 / 255.0 * 100.0).round() / 100.0;
            format!("hsla({h:.0}, {s:.0}%, {l:.0}%, {alpha})")
        };
        Self {
            color,
            raw,
            hex,
            rgb_str,
            hsl_str,
        }
    }
}

fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let l = (max + min) / 2.0;
    if delta.abs() < 1e-5 {
        return (0.0, 0.0, (l * 100.0).clamp(0.0, 100.0));
    }
    let s = if l > 0.5 {
        delta / (2.0 - max - min)
    } else {
        delta / (max + min)
    };
    let mut h = if (max - r).abs() < 1e-5 {
        (g - b) / delta + (if g < b { 6.0 } else { 0.0 })
    } else if (max - g).abs() < 1e-5 {
        (b - r) / delta + 2.0
    } else {
        (r - g) / delta + 4.0
    };
    h *= 60.0;
    (
        h.round().rem_euclid(360.0),
        (s * 100.0).round().clamp(0.0, 100.0),
        (l * 100.0).round().clamp(0.0, 100.0),
    )
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> (u8, u8, u8) {
    let h = (h % 360.0 + 360.0) % 360.0;
    let s = s.clamp(0.0, 1.0);
    let l = l.clamp(0.0, 1.0);
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r1, g1, b1) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    (
        ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let h = (h % 360.0 + 360.0) % 360.0;
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let m = v - c;
    let (r1, g1, b1) = if h < 60.0 {
        (c, x, 0.0)
    } else if h < 120.0 {
        (x, c, 0.0)
    } else if h < 180.0 {
        (0.0, c, x)
    } else if h < 240.0 {
        (0.0, x, c)
    } else if h < 300.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    (
        ((r1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b1 + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    )
}

fn draw_color_box(
    painter: &egui::Painter,
    rect: egui::Rect,
    color: Color32,
    radius: CornerRadius,
) {
    if color.a() < 255 {
        let tile_size = 5.0;
        let c1 = Color32::from_rgb(42, 45, 41);
        let c2 = Color32::from_rgb(65, 70, 64);
        painter.rect_filled(rect, radius, c1);
        let mut y = rect.min.y;
        let mut yi = 0;
        while y < rect.max.y {
            let next_y = (y + tile_size).min(rect.max.y);
            let mut x = rect.min.x;
            let mut xi = 0;
            while x < rect.max.x {
                let next_x = (x + tile_size).min(rect.max.x);
                if (xi + yi) % 2 == 1 {
                    let tile_rect =
                        egui::Rect::from_min_max(Pos2::new(x, y), Pos2::new(next_x, next_y));
                    painter.rect_filled(tile_rect, CornerRadius::ZERO, c2);
                }
                x += tile_size;
                xi += 1;
            }
            y += tile_size;
            yi += 1;
        }
    }
    painter.rect_filled(rect, radius, color);

    let border_stroke = if color.a() < 100 {
        Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(220, 220, 220, 90))
    } else {
        let brightness =
            (color.r() as u32 * 299 + color.g() as u32 * 587 + color.b() as u32 * 114) / 1000;
        if brightness < 70 {
            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(255, 255, 255, 60))
        } else {
            Stroke::new(1.0_f32, Color32::from_rgba_unmultiplied(0, 0, 0, 70))
        }
    };
    painter.rect_stroke(rect, radius, border_stroke, egui::StrokeKind::Inside);
}

fn draw_color_row(ui: &mut egui::Ui, color_info: &DetectedColor) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing = Vec2::new(10.0, 0.0);

        let (rect, response) = ui.allocate_exact_size(Vec2::new(34.0, 34.0), Sense::hover());
        draw_color_box(ui.painter(), rect, color_info.color, CornerRadius::same(6));

        response.on_hover_ui(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(&color_info.raw).size(12.0).strong().color(TEXT));
                ui.add_space(2.0);
                ui.label(RichText::new(format!("HEX: {}", color_info.hex)).size(11.0).color(ACCENT));
                ui.label(RichText::new(format!("RGB: {}", color_info.rgb_str)).size(11.0).color(MUTED));
                ui.label(RichText::new(format!("HSL: {}", color_info.hsl_str)).size(11.0).color(MUTED));
            });
        });

        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(0.0, 2.0);

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(
                    RichText::new(&color_info.raw)
                        .size(14.5)
                        .strong()
                        .color(TEXT),
                );
                if !color_info.raw.eq_ignore_ascii_case(&color_info.hex) {
                    ui.label(
                        RichText::new(&color_info.hex)
                            .size(12.5)
                            .color(ACCENT),
                    );
                }
            });

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 6.0;
                ui.label(RichText::new(&color_info.rgb_str).size(11.5).color(MUTED));
                ui.label(RichText::new("·").size(11.0).color(FAINT));
                ui.label(RichText::new(&color_info.hsl_str).size(11.5).color(MUTED));
            });
        });
    });
}

fn is_word_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

fn push_unique(results: &mut Vec<DetectedColor>, color: DetectedColor) {
    if results.len() < 8 && !results.iter().any(|existing| existing.color == color.color) {
        results.push(color);
    }
}

fn parse_hex_color(s: &str) -> Option<(u8, u8, u8, u8)> {
    let hex = s
        .strip_prefix('#')
        .or_else(|| s.strip_prefix("0x"))
        .or_else(|| s.strip_prefix("0X"))?;

    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }

    match hex.len() {
        3 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
            Some((r * 17, g * 17, b * 17, 255))
        }
        4 => {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
            let a = u8::from_str_radix(&hex[3..4], 16).ok()?;
            Some((r * 17, g * 17, b * 17, a * 17))
        }
        6 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            Some((r, g, b, 255))
        }
        8 => {
            let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
            let a = u8::from_str_radix(&hex[6..8], 16).ok()?;
            Some((r, g, b, a))
        }
        _ => None,
    }
}

fn parse_bare_hex_color(s: &str) -> Option<(u8, u8, u8, u8)> {
    if s.len() == 6
        && s.chars().all(|c| c.is_ascii_hexdigit())
        && s.chars().any(|c| c.is_ascii_alphabetic())
    {
        let r = u8::from_str_radix(&s[0..2], 16).ok()?;
        let g = u8::from_str_radix(&s[2..4], 16).ok()?;
        let b = u8::from_str_radix(&s[4..6], 16).ok()?;
        Some((r, g, b, 255))
    } else {
        None
    }
}

fn tokenize_args(inner: &str) -> Vec<&str> {
    inner
        .split(|c: char| c == ',' || c == '/' || c.is_whitespace())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_channel(s: &str, max: f32) -> Option<f32> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix('%') {
        num.trim().parse::<f32>().ok().map(|p| (p / 100.0 * max).clamp(0.0, max))
    } else {
        s.parse::<f32>().ok().map(|v| v.clamp(0.0, max))
    }
}

fn parse_alpha(s: &str) -> Option<u8> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix('%') {
        num.trim()
            .parse::<f32>()
            .ok()
            .map(|p| (p * 2.55).round().clamp(0.0, 255.0) as u8)
    } else if let Ok(val) = s.parse::<f32>() {
        if val <= 1.0 && s.contains('.') {
            Some((val * 255.0).round().clamp(0.0, 255.0) as u8)
        } else if val <= 1.0 && val > 0.0 {
            if val == 1.0 {
                Some(255)
            } else {
                Some((val * 255.0).round().clamp(0.0, 255.0) as u8)
            }
        } else if val == 0.0 {
            Some(0)
        } else {
            Some(val.round().clamp(0.0, 255.0) as u8)
        }
    } else {
        None
    }
}

fn parse_hue(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(deg) = s.strip_suffix("deg") {
        deg.trim().parse::<f32>().ok()
    } else if let Some(rad) = s.strip_suffix("rad") {
        rad.trim().parse::<f32>().ok().map(|r| r.to_degrees())
    } else if let Some(turn) = s.strip_suffix("turn") {
        turn.trim().parse::<f32>().ok().map(|t| t * 360.0)
    } else {
        s.parse::<f32>().ok()
    }
}

fn parse_percent(s: &str) -> Option<f32> {
    let s = s.trim();
    if let Some(num) = s.strip_suffix('%') {
        num.trim().parse::<f32>().ok().map(|p| (p / 100.0).clamp(0.0, 1.0))
    } else {
        s.parse::<f32>().ok().map(|v| if v > 1.0 { (v / 100.0).clamp(0.0, 1.0) } else { v.clamp(0.0, 1.0) })
    }
}

fn parse_fn_color(s: &str) -> Option<DetectedColor> {
    let s = s.trim();
    let paren_start = s.find('(')?;
    let paren_end = s.rfind(')')?;
    if paren_end <= paren_start {
        return None;
    }
    let fn_name = s[..paren_start].trim().to_ascii_lowercase();
    let inner = &s[paren_start + 1..paren_end];
    let args = tokenize_args(inner);

    let (r, g, b, a) = match fn_name.as_str() {
        "rgb" | "rgba" => {
            if args.len() < 3 || args.len() > 4 {
                return None;
            }
            let r = parse_channel(args[0], 255.0)? as u8;
            let g = parse_channel(args[1], 255.0)? as u8;
            let b = parse_channel(args[2], 255.0)? as u8;
            let a = if args.len() == 4 {
                parse_alpha(args[3])?
            } else {
                255
            };
            (r, g, b, a)
        }
        "hsl" | "hsla" => {
            if args.len() < 3 || args.len() > 4 {
                return None;
            }
            let h = parse_hue(args[0])?;
            let s = parse_percent(args[1])?;
            let l = parse_percent(args[2])?;
            let a = if args.len() == 4 {
                parse_alpha(args[3])?
            } else {
                255
            };
            let (r, g, b) = hsl_to_rgb(h, s, l);
            (r, g, b, a)
        }
        "hsv" | "hsva" | "hsb" | "hsba" => {
            if args.len() < 3 || args.len() > 4 {
                return None;
            }
            let h = parse_hue(args[0])?;
            let s = parse_percent(args[1])?;
            let v = parse_percent(args[2])?;
            let a = if args.len() == 4 {
                parse_alpha(args[3])?
            } else {
                255
            };
            let (r, g, b) = hsv_to_rgb(h, s, v);
            (r, g, b, a)
        }
        "cmyk" => {
            if args.len() != 4 {
                return None;
            }
            let c = parse_percent(args[0])?;
            let m = parse_percent(args[1])?;
            let y = parse_percent(args[2])?;
            let k = parse_percent(args[3])?;
            let r = ((1.0 - c) * (1.0 - k) * 255.0).round().clamp(0.0, 255.0) as u8;
            let g = ((1.0 - m) * (1.0 - k) * 255.0).round().clamp(0.0, 255.0) as u8;
            let b = ((1.0 - y) * (1.0 - k) * 255.0).round().clamp(0.0, 255.0) as u8;
            (r, g, b, 255)
        }
        _ => return None,
    };

    Some(DetectedColor::new(r, g, b, a, s.to_string()))
}

fn parse_named_color(s: &str) -> Option<(u8, u8, u8, u8)> {
    let lower = s.to_ascii_lowercase();
    let (r, g, b) = match lower.as_str() {
        "black" => (0, 0, 0),
        "white" => (255, 255, 255),
        "red" => (255, 0, 0),
        "green" => (0, 128, 0),
        "blue" => (0, 0, 255),
        "yellow" => (255, 255, 0),
        "cyan" | "aqua" => (0, 255, 255),
        "magenta" | "fuchsia" => (255, 0, 255),
        "silver" => (192, 192, 192),
        "gray" | "grey" => (128, 128, 128),
        "maroon" => (128, 0, 0),
        "purple" => (128, 0, 128),
        "lime" => (0, 255, 0),
        "olive" => (128, 128, 0),
        "navy" => (0, 0, 128),
        "teal" => (0, 128, 128),
        "orange" => (255, 165, 0),
        "pink" => (255, 192, 203),
        "gold" => (255, 215, 0),
        "coral" => (255, 127, 80),
        "tomato" => (255, 99, 71),
        "crimson" => (220, 20, 60),
        "indigo" => (75, 0, 130),
        "violet" => (238, 130, 238),
        "turquoise" => (64, 224, 208),
        "brown" => (165, 42, 42),
        "salmon" => (250, 128, 114),
        "khaki" => (240, 230, 140),
        "plum" => (221, 160, 221),
        "orchid" => (218, 112, 214),
        "skyblue" => (135, 206, 235),
        "hotpink" => (255, 105, 180),
        "dodgerblue" => (30, 144, 255),
        "rebeccapurple" => (102, 51, 153),
        "transparent" => return Some((0, 0, 0, 0)),
        _ => return None,
    };
    Some((r, g, b, 255))
}

fn parse_single_color(s: &str) -> Option<DetectedColor> {
    let trimmed =
        s.trim_matches(|c: char| c == '"' || c == '\'' || c == ';' || c.is_whitespace());
    if let Some((r, g, b, a)) = parse_hex_color(trimmed) {
        return Some(DetectedColor::new(r, g, b, a, trimmed.to_string()));
    }
    if let Some((r, g, b, a)) = parse_bare_hex_color(trimmed) {
        return Some(DetectedColor::new(r, g, b, a, trimmed.to_string()));
    }
    if let Some(color) = parse_fn_color(trimmed) {
        return Some(color);
    }
    if let Some((r, g, b, a)) = parse_named_color(trimmed) {
        return Some(DetectedColor::new(r, g, b, a, trimmed.to_string()));
    }
    None
}

fn extract_colors(text: &str) -> (Vec<DetectedColor>, bool) {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return (Vec::new(), false);
    }

    if let Some(color) = parse_single_color(trimmed) {
        return (vec![color], true);
    }

    let mut results = Vec::new();
    let bytes = text.as_bytes();
    let len = bytes.len();
    let mut i = 0;

    let fn_prefixes: [&[u8]; 9] = [
        b"rgb(", b"rgba(", b"hsl(", b"hsla(", b"hsv(", b"hsva(", b"hsb(", b"hsba(", b"cmyk(",
    ];

    while i < len {
        // 1. Check for '#'
        if bytes[i] == b'#' {
            let preceded_ok = i == 0 || !is_word_char(bytes[i - 1]);
            if preceded_ok {
                let start = i + 1;
                let mut end = start;
                while end < len && bytes[end].is_ascii_hexdigit() {
                    end += 1;
                }
                let hex_len = end - start;
                let followed_ok = end >= len || !is_word_char(bytes[end]);
                if followed_ok && (hex_len == 3 || hex_len == 4 || hex_len == 6 || hex_len == 8) {
                    let is_all_digits = bytes[start..end].iter().all(|b| b.is_ascii_digit());
                    let in_code_context = i > 0
                        && matches!(
                            bytes[i - 1],
                            b':' | b'=' | b'"' | b'\'' | b'(' | b'[' | b'{' | b','
                        );
                    if hex_len == 6 || hex_len == 8 || !is_all_digits || in_code_context {
                        if let Ok(candidate) = std::str::from_utf8(&bytes[i..end]) {
                            if let Some((r, g, b, a)) = parse_hex_color(candidate) {
                                push_unique(
                                    &mut results,
                                    DetectedColor::new(r, g, b, a, candidate.to_string()),
                                );
                            }
                        }
                    }
                }
                i = end;
                continue;
            }
        }

        // 2. Check for "0x" or "0X"
        if i + 2 < len && bytes[i] == b'0' && (bytes[i + 1] == b'x' || bytes[i + 1] == b'X') {
            let preceded_ok = i == 0 || !is_word_char(bytes[i - 1]);
            if preceded_ok {
                let start = i + 2;
                let mut end = start;
                while end < len && bytes[end].is_ascii_hexdigit() {
                    end += 1;
                }
                let hex_len = end - start;
                let followed_ok = end >= len || !is_word_char(bytes[end]);
                if followed_ok && (hex_len == 6 || hex_len == 8) {
                    if let Ok(candidate) = std::str::from_utf8(&bytes[i..end]) {
                        if let Some((r, g, b, a)) = parse_hex_color(candidate) {
                            push_unique(
                                &mut results,
                                DetectedColor::new(r, g, b, a, candidate.to_string()),
                            );
                        }
                    }
                }
                i = end;
                continue;
            }
        }

        // 3. Check for function prefixes
        let preceded_ok = i == 0 || !is_word_char(bytes[i - 1]);
        if preceded_ok {
            let matched_prefix = fn_prefixes.iter().find(|prefix| {
                i + prefix.len() <= len
                    && bytes[i..i + prefix.len()].eq_ignore_ascii_case(prefix)
            });

            if matched_prefix.is_some() {
                let remaining_bytes = &bytes[i..];
                if let Some(close_offset) = remaining_bytes.iter().position(|&b| b == b')') {
                    let end_pos = i + close_offset + 1;
                    if let Ok(candidate) = std::str::from_utf8(&bytes[i..end_pos]) {
                        if let Some(color) = parse_fn_color(candidate) {
                            push_unique(&mut results, color);
                            i = end_pos;
                            continue;
                        }
                    }
                }
            }
        }

        i += 1;
    }

    (results, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_hex_colors() {
        assert_eq!(parse_hex_color("#fff"), Some((255, 255, 255, 255)));
        assert_eq!(parse_hex_color("#FF5733"), Some((255, 87, 51, 255)));
        assert_eq!(parse_hex_color("#ff573380"), Some((255, 87, 51, 128)));
        assert_eq!(parse_hex_color("#1234"), Some((17, 34, 51, 68)));
        assert_eq!(parse_hex_color("0x3b82f6"), Some((59, 130, 246, 255)));
    }

    #[test]
    fn test_parse_rgb_colors() {
        assert_eq!(
            parse_fn_color("rgb(255, 87, 51)").map(|c| c.color),
            Some(Color32::from_rgb(255, 87, 51))
        );
        assert_eq!(
            parse_fn_color("rgba(255, 87, 51, 0.5)").map(|c| c.color),
            Some(Color32::from_rgba_unmultiplied(255, 87, 51, 128))
        );
        assert_eq!(
            parse_fn_color("rgb(100%, 0%, 0%)").map(|c| c.color),
            Some(Color32::from_rgb(255, 0, 0))
        );
        assert_eq!(
            parse_fn_color("rgb(255 87 51 / 0.5)").map(|c| c.color),
            Some(Color32::from_rgba_unmultiplied(255, 87, 51, 128))
        );
    }

    #[test]
    fn test_parse_hsl_colors() {
        assert_eq!(
            parse_fn_color("hsl(0, 100%, 50%)").map(|c| c.color),
            Some(Color32::from_rgb(255, 0, 0))
        );
        assert_eq!(
            parse_fn_color("hsl(120, 100%, 50%)").map(|c| c.color),
            Some(Color32::from_rgb(0, 255, 0))
        );
        assert_eq!(
            parse_fn_color("hsl(240, 100%, 50%)").map(|c| c.color),
            Some(Color32::from_rgb(0, 0, 255))
        );
    }

    #[test]
    fn test_parse_named_and_bare() {
        assert_eq!(
            parse_single_color("coral").map(|c| c.color),
            Some(Color32::from_rgb(255, 127, 80))
        );
        assert_eq!(
            parse_single_color("3b82f6").map(|c| c.color),
            Some(Color32::from_rgb(59, 130, 246))
        );
    }

    #[test]
    fn test_extract_colors_pure_and_mixed() {
        let (colors, is_pure) = extract_colors("#ff5733");
        assert!(is_pure);
        assert_eq!(colors.len(), 1);

        let (colors, is_pure) = extract_colors("background: #3b82f6; border: 1px solid rgba(0, 0, 0, 0.5);");
        assert!(!is_pure);
        assert_eq!(colors.len(), 2);
        assert_eq!(colors[0].hex, "#3B82F6");

        let (colors, is_pure) = extract_colors("Fix issue #123 in repository");
        assert!(!is_pure);
        assert!(colors.is_empty());
    }

    #[test]
    fn test_parse_hsv_and_cmyk() {
        assert_eq!(
            parse_fn_color("hsv(0, 100%, 100%)").map(|c| c.color),
            Some(Color32::from_rgb(255, 0, 0))
        );
        assert_eq!(
            parse_fn_color("hsb(120, 100%, 100%)").map(|c| c.color),
            Some(Color32::from_rgb(0, 255, 0))
        );
        assert_eq!(
            parse_fn_color("cmyk(0%, 100%, 100%, 0%)").map(|c| c.color),
            Some(Color32::from_rgb(255, 0, 0))
        );
    }

    #[test]
    fn test_multibyte_utf8_text() {
        // Multi-byte character '×' right near prefix-length check (which previously caused a panic)
        let text = "Resolution: 1920 × 1080, Color: #7C9884, emoji: 🎨, text: こんにちは";
        let (colors, is_pure) = extract_colors(text);
        assert!(!is_pure);
        assert_eq!(colors.len(), 1);
        assert_eq!(colors[0].hex, "#7C9884");

        let text2 = "1920 × 1080 rgb(124, 152, 132)";
        let (colors2, is_pure2) = extract_colors(text2);
        assert!(!is_pure2);
        assert_eq!(colors2.len(), 1);
        assert_eq!(colors2[0].hex, "#7C9884");

        let text3 = "××××××";
        let (colors3, is_pure3) = extract_colors(text3);
        assert!(!is_pure3);
        assert!(colors3.is_empty());
    }

    #[test]
    fn test_reorder_queue_logic() {
        let mut deque = VecDeque::from(vec!["A", "B", "C", "D"]);

        // Move C (index 2) to front (index 0)
        let item = deque.remove(2).unwrap();
        deque.insert(0, item);
        assert_eq!(deque, VecDeque::from(vec!["C", "A", "B", "D"]));

        // Move C (index 0) to end (index 3)
        let item = deque.remove(0).unwrap();
        deque.insert(3, item);
        assert_eq!(deque, VecDeque::from(vec!["A", "B", "D", "C"]));

        // Move B (index 1) to index 2
        let item = deque.remove(1).unwrap();
        deque.insert(2, item);
        assert_eq!(deque, VecDeque::from(vec!["A", "D", "B", "C"]));
    }

    #[test]
    fn test_image_reader_api() {
        use std::io::Cursor;
        let dummy = vec![0u8; 10];
        let res = image::ImageReader::new(Cursor::new(&dummy))
            .with_guessed_format()
            .ok()
            .and_then(|r| r.into_dimensions().ok());
        assert_eq!(res, None);
    }

    #[test]
    fn test_probe_image_dimensions_and_clipboard_item() {
        // Minimal 1x1 PNG bytes
        let png_1x1: &[u8] = &[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82,
            0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 31, 21, 196, 137,
            0, 0, 0, 10, 73, 68, 65, 84, 120, 156, 99, 0, 1, 0, 0, 5, 0,
            1, 13, 10, 45, 180, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130
        ];
        assert_eq!(probe_image_dimensions(png_1x1), Some((1, 1)));

        let item = ClipboardItem::new(Arc::from(png_1x1));
        assert_eq!(item.kind(), "IMAGE");
        match item.preview {
            Preview::Image { width, height } => {
                assert_eq!(width, 1);
                assert_eq!(height, 1);
            }
            _ => panic!("Expected Preview::Image"),
        }
    }

    #[test]
    fn test_clipboard_item_text_and_color() {
        let text_item = ClipboardItem::new(Arc::from("hello world".as_bytes()));
        assert_eq!(text_item.kind(), "TEXT");

        let color_item = ClipboardItem::new(Arc::from("#7C9884".as_bytes()));
        assert_eq!(color_item.kind(), "COLOR");
        assert!(color_item.is_pure_color);
        assert_eq!(color_item.detected_colors.len(), 1);
    }

    #[test]
    fn test_glibc_trim_call() {
        configure_allocator();
        trim_memory();
    }
}
