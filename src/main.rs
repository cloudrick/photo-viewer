slint::include_modules!();

use i_slint_backend_winit::{CustomApplicationHandler, EventResult};
use image::ImageDecoder;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use winit::event::WindowEvent as WinitWindowEvent;

// View state for each file during the current session
#[derive(Clone, Copy, Debug)]
pub struct FileViewState {
    pub is_fit_mode: bool,
    pub zoom_factor: f32,
    pub offset_x: f32,
    pub offset_y: f32,
    pub rotation_angle: i32,
}

// Custom Winit Application Handler for native OS Drag & Drop file handling
struct DragDropHandler {
    file_dropped_tx: std::sync::mpsc::Sender<PathBuf>,
}

impl CustomApplicationHandler for DragDropHandler {
    fn window_event(
        &mut self,
        _event_loop: &winit::event_loop::ActiveEventLoop,
        _window_id: winit::window::WindowId,
        _winit_window: Option<&winit::window::Window>,
        _slint_window: Option<&i_slint_core::api::Window>,
        event: &WinitWindowEvent,
    ) -> EventResult {
        if let WinitWindowEvent::DroppedFile(path) = event {
            let _ = self.file_dropped_tx.send(path.clone());
        }
        EventResult::Propagate
    }
}

// Decoded raw pixel buffer which implements Send + Sync across threads
#[derive(Clone)]
pub struct DecodedRawImage {
    pub pixel_buffer: slint::SharedPixelBuffer<slint::Rgba8Pixel>,
    pub width: u32,
    pub height: u32,
    pub file_name: String,
    pub file_path: String,
    pub file_size_str: String,
}

// Thread-safe in-memory cache holding DecodedRawImage
struct AppCache {
    map: HashMap<PathBuf, Arc<DecodedRawImage>>,
    recent_keys: Vec<PathBuf>,
    max_capacity: usize,
}

impl AppCache {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            recent_keys: Vec::new(),
            max_capacity: capacity,
        }
    }

    fn get(&mut self, path: &Path) -> Option<Arc<DecodedRawImage>> {
        if let Some(data) = self.map.get(path) {
            if let Some(pos) = self.recent_keys.iter().position(|p| p == path) {
                let p = self.recent_keys.remove(pos);
                self.recent_keys.push(p);
            }
            return Some(data.clone());
        }
        None
    }

    fn insert(&mut self, path: PathBuf, data: Arc<DecodedRawImage>) {
        if self.map.contains_key(&path) {
            self.map.insert(path.clone(), data);
            return;
        }

        if self.recent_keys.len() >= self.max_capacity {
            if let Some(oldest) = self.recent_keys.first().cloned() {
                self.recent_keys.remove(0);
                self.map.remove(&oldest);
            }
        }

        self.recent_keys.push(path.clone());
        self.map.insert(path, data);
    }
}

// Helper to format file size cleanly
fn format_file_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

// Check if file is a supported image by extension
fn is_supported_image(path: &Path) -> bool {
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        matches!(
            ext.to_lowercase().as_str(),
            "jpg" | "jpeg" | "png" | "webp" | "bmp" | "gif" | "tiff" | "tif" | "jfif" | "ico"
        )
    } else {
        false
    }
}

// Load and decode an image from disk into Send-able DecodedRawImage
fn decode_image_file(path: &Path) -> Result<DecodedRawImage, String> {
    let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
    let file_size_str = format_file_size(metadata.len());
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown")
        .to_string();
    let file_path = path.to_string_lossy().to_string();

    let reader = image::ImageReader::open(path)
        .map_err(|e| e.to_string())?
        .with_guessed_format()
        .map_err(|e| e.to_string())?;

    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);

    let width = img.width();
    let height = img.height();

    let rgba = img.to_rgba8();
    let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
        rgba.as_raw(),
        width,
        height,
    );

    Ok(DecodedRawImage {
        pixel_buffer,
        width,
        height,
        file_name,
        file_path,
        file_size_str,
    })
}

fn scan_folder(folder: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(entries) = fs::read_dir(folder) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() && is_supported_image(&path) {
                files.push(path);
            }
        }
    }
    // Sort naturally by path
    files.sort_by(|a, b| {
        a.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
            .cmp(
                &b.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase(),
            )
    });
    files
}

fn main() -> Result<(), slint::PlatformError> {
    // Channel for native OS Drag & Drop file events
    let (file_dropped_tx, file_dropped_rx) = std::sync::mpsc::channel::<PathBuf>();
    let backend = i_slint_backend_winit::Backend::builder()
        .with_custom_application_handler(Box::new(DragDropHandler { file_dropped_tx }))
        .build()
        .expect("Failed to initialize Slint winit backend");
    slint::platform::set_platform(Box::new(backend)).expect("Failed to set Slint platform");

    let ui = MainWindow::new()?;
    ui.window().set_fullscreen(true);

    // Shared state
    let folder_images = Arc::new(Mutex::new(Vec::<PathBuf>::new()));
    let current_index = Arc::new(Mutex::new(0usize));
    let cache = Arc::new(Mutex::new(AppCache::new(8))); // Keep up to 8 images in RAM cache
    let file_view_states = Arc::new(Mutex::new(HashMap::<PathBuf, FileViewState>::new()));

    // Helper closure to calculate Fit Screen zoom mode:
    // Entire image fits inside window without cropping, maintaining exact aspect ratio.
    let calculate_fit_zoom =
        |img_w: f32, img_h: f32, win_w: f32, win_h: f32, rot_angle: i32| -> f32 {
            if img_w <= 0.0 || img_h <= 0.0 || win_w <= 0.0 || win_h <= 0.0 {
                return 1.0;
            }

            let (effective_w, effective_h) = if rot_angle == 90 || rot_angle == 270 {
                (img_h, img_w)
            } else {
                (img_w, img_h)
            };

            let scale_w = win_w / effective_w;
            let scale_h = win_h / effective_h;

            // Scale to 90% of container fit so image has a pleasant 5% padding around frame (e.g. 6000x4000 in 1920x1080 => 1458x972)
            0.90 * scale_w.min(scale_h)
        };

    // Helper to get window size in logical pixels (accounting for DPI scaling)
    let get_window_logical_size = |ui: &MainWindow| -> (f32, f32) {
        let scale_factor = ui.window().scale_factor();
        let win_size = ui.window().size();
        let w = if win_size.width > 0 {
            win_size.width as f32 / scale_factor
        } else {
            1280.0
        };
        let h = if win_size.height > 0 {
            win_size.height as f32 / scale_factor
        } else {
            800.0
        };
        (w, h)
    };

    // Helper to zoom at target focal point (mouse cursor position or center)
    let zoom_at_point = move |ui: &MainWindow, new_zoom: f32, mx: f32, my: f32| {
        let cur_zoom = ui.get_zoom_factor();
        if cur_zoom <= 0.0 {
            return;
        }
        let (win_w, win_h) = get_window_logical_size(ui);
        let mx = if mx >= 0.0 { mx } else { win_w / 2.0 };
        let my = if my >= 0.0 { my } else { win_h / 2.0 };

        let cx = mx - win_w / 2.0;
        let cy = my - win_h / 2.0;

        let cur_ox = ui.get_offset_x();
        let cur_oy = ui.get_offset_y();

        let new_ox = cur_ox + (cx - cur_ox) * (1.0 - new_zoom / cur_zoom);
        let new_oy = cur_oy + (cy - cur_oy) * (1.0 - new_zoom / cur_zoom);

        ui.set_zoom_factor(new_zoom);
        ui.set_offset_x(new_ox);
        ui.set_offset_y(new_oy);
        ui.set_zoom_percent_str(format!("{:.0}%", new_zoom * 100.0).into());
        ui.set_is_fit_mode(false);
    };

    // Save view state of the currently active file before switching
    let save_current_view_state = {
        let folder_images = folder_images.clone();
        let current_index = current_index.clone();
        let file_view_states = file_view_states.clone();
        let ui_handle = ui.as_weak();

        move || {
            if let Some(ui) = ui_handle.upgrade() {
                let images = folder_images.lock().unwrap();
                if images.is_empty() {
                    return;
                }
                let cur = *current_index.lock().unwrap();
                if cur < images.len() {
                    let path = images[cur].clone();
                    let state = FileViewState {
                        is_fit_mode: ui.get_is_fit_mode(),
                        zoom_factor: ui.get_zoom_factor(),
                        offset_x: ui.get_offset_x(),
                        offset_y: ui.get_offset_y(),
                        rotation_angle: ui.get_rotation_angle(),
                    };
                    file_view_states.lock().unwrap().insert(path, state);
                }
            }
        }
    };

    // Core function to display image at specified index
    let load_image_at_index = {
        let ui_handle = ui.as_weak();
        let folder_images = folder_images.clone();
        let current_index = current_index.clone();
        let cache = cache.clone();
        let file_view_states = file_view_states.clone();

        move |target_index: usize, _force_reset_zoom: bool| {
            let images = folder_images.lock().unwrap().clone();
            if images.is_empty() || target_index >= images.len() {
                return;
            }

            *current_index.lock().unwrap() = target_index;
            let target_path = images[target_index].clone();
            let total_count = images.len();

            // Check session view state memory
            let saved_state = file_view_states.lock().unwrap().get(&target_path).cloned();

            // Check cache first
            let cached_raw = cache.lock().unwrap().get(&target_path);

            if let Some(raw) = cached_raw {
                if let Some(ui) = ui_handle.upgrade() {
                    let slint_image = slint::Image::from_rgba8(raw.pixel_buffer.clone());
                    ui.set_current_image(slint_image);
                    ui.set_image_loaded(true);
                    ui.set_is_loading(false);
                    ui.set_file_name(raw.file_name.clone().into());
                    ui.set_full_path(raw.file_path.clone().into());
                    ui.set_img_natural_width(raw.width as f32);
                    ui.set_img_natural_height(raw.height as f32);
                    ui.set_file_info(
                        format!(
                            "{} × {} px  •  {}",
                            raw.width, raw.height, raw.file_size_str
                        )
                        .into(),
                    );
                    ui.set_folder_info(
                        format!("[ {} / {} ]", target_index + 1, total_count).into(),
                    );

                    // Apply saved session view state if available; otherwise default to Fit Screen mode (entire image fits inside window boundary)
                    if let Some(state) = saved_state {
                        if state.is_fit_mode {
                            let (win_w, win_h) = get_window_logical_size(&ui);
                            let fit_zoom = calculate_fit_zoom(
                                raw.width as f32,
                                raw.height as f32,
                                win_w,
                                win_h,
                                state.rotation_angle,
                            );
                            ui.set_is_fit_mode(true);
                            ui.set_zoom_factor(fit_zoom);
                            ui.set_offset_x(0.0);
                            ui.set_offset_y(0.0);
                            ui.set_rotation_angle(state.rotation_angle);
                            ui.set_zoom_percent_str(format!("{:.0}%", fit_zoom * 100.0).into());
                        } else {
                            ui.set_is_fit_mode(false);
                            ui.set_zoom_factor(state.zoom_factor);
                            ui.set_offset_x(state.offset_x);
                            ui.set_offset_y(state.offset_y);
                            ui.set_rotation_angle(state.rotation_angle);
                            ui.set_zoom_percent_str(
                                format!("{:.0}%", state.zoom_factor * 100.0).into(),
                            );
                        }
                    } else {
                        let (win_w, win_h) = get_window_logical_size(&ui);
                        let rot = 0;
                        let fit_zoom = calculate_fit_zoom(
                            raw.width as f32,
                            raw.height as f32,
                            win_w,
                            win_h,
                            rot,
                        );
                        ui.set_zoom_factor(fit_zoom);
                        ui.set_offset_x(0.0);
                        ui.set_offset_y(0.0);
                        ui.set_rotation_angle(0);
                        ui.set_zoom_percent_str(format!("{:.0}%", fit_zoom * 100.0).into());
                        ui.set_is_fit_mode(true);
                    }
                }
            } else {
                // Not in cache: show loading spinner and decode on background thread
                if let Some(ui) = ui_handle.upgrade() {
                    ui.set_is_loading(true);
                }

                let ui_handle = ui_handle.clone();
                let cache = cache.clone();
                let path_to_load = target_path.clone();

                thread::spawn(move || {
                    let res = decode_image_file(&path_to_load);
                    let _ = slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_handle.upgrade() {
                            match res {
                                Ok(raw) => {
                                    let raw_arc = Arc::new(raw);
                                    cache.lock().unwrap().insert(path_to_load, raw_arc.clone());

                                    let slint_image =
                                        slint::Image::from_rgba8(raw_arc.pixel_buffer.clone());
                                    ui.set_current_image(slint_image);
                                    ui.set_image_loaded(true);
                                    ui.set_is_loading(false);
                                    ui.set_file_name(raw_arc.file_name.clone().into());
                                    ui.set_full_path(raw_arc.file_path.clone().into());
                                    ui.set_img_natural_width(raw_arc.width as f32);
                                    ui.set_img_natural_height(raw_arc.height as f32);
                                    ui.set_file_info(
                                        format!(
                                            "{} × {} px  •  {}",
                                            raw_arc.width, raw_arc.height, raw_arc.file_size_str
                                        )
                                        .into(),
                                    );
                                    ui.set_folder_info(
                                        format!("[ {} / {} ]", target_index + 1, total_count)
                                            .into(),
                                    );

                                    // Apply saved session view state if available; otherwise default to Fit Screen mode
                                    if let Some(state) = saved_state {
                                        if state.is_fit_mode {
                                            let (win_w, win_h) = get_window_logical_size(&ui);
                                            let fit_zoom = calculate_fit_zoom(
                                                raw_arc.width as f32,
                                                raw_arc.height as f32,
                                                win_w,
                                                win_h,
                                                state.rotation_angle,
                                            );
                                            ui.set_is_fit_mode(true);
                                            ui.set_zoom_factor(fit_zoom);
                                            ui.set_offset_x(0.0);
                                            ui.set_offset_y(0.0);
                                            ui.set_rotation_angle(state.rotation_angle);
                                            ui.set_zoom_percent_str(
                                                format!("{:.0}%", fit_zoom * 100.0).into(),
                                            );
                                        } else {
                                            ui.set_is_fit_mode(false);
                                            ui.set_zoom_factor(state.zoom_factor);
                                            ui.set_offset_x(state.offset_x);
                                            ui.set_offset_y(state.offset_y);
                                            ui.set_rotation_angle(state.rotation_angle);
                                            ui.set_zoom_percent_str(
                                                format!("{:.0}%", state.zoom_factor * 100.0).into(),
                                            );
                                        }
                                    } else {
                                        let (win_w, win_h) = get_window_logical_size(&ui);
                                        let rot = 0;
                                        let fit_zoom = calculate_fit_zoom(
                                            raw_arc.width as f32,
                                            raw_arc.height as f32,
                                            win_w,
                                            win_h,
                                            rot,
                                        );
                                        ui.set_zoom_factor(fit_zoom);
                                        ui.set_offset_x(0.0);
                                        ui.set_offset_y(0.0);
                                        ui.set_rotation_angle(0);
                                        ui.set_zoom_percent_str(
                                            format!("{:.0}%", fit_zoom * 100.0).into(),
                                        );
                                        ui.set_is_fit_mode(true);
                                    }
                                }
                                Err(err_msg) => {
                                    ui.set_is_loading(false);
                                    ui.set_file_info(format!("Error: {}", err_msg).into());
                                }
                            }
                        }
                    });
                });
            }

            // Preload adjacent images in background worker threads
            let pre_indices = vec![
                if target_index + 1 < total_count {
                    Some(target_index + 1)
                } else {
                    None
                },
                if target_index > 0 {
                    Some(target_index - 1)
                } else {
                    None
                },
                if target_index + 2 < total_count {
                    Some(target_index + 2)
                } else {
                    None
                },
                if target_index >= 2 {
                    Some(target_index - 2)
                } else {
                    None
                },
            ];

            for idx_opt in pre_indices {
                if let Some(idx) = idx_opt {
                    let pre_path = images[idx].clone();
                    let cache = cache.clone();
                    let in_cache = cache.lock().unwrap().get(&pre_path).is_some();
                    if !in_cache {
                        thread::spawn(move || {
                            if let Ok(raw) = decode_image_file(&pre_path) {
                                cache.lock().unwrap().insert(pre_path, Arc::new(raw));
                            }
                        });
                    }
                }
            }
        }
    };

    // Timer to handle native OS Drag & Drop file events and window resize in fit mode
    let drag_drop_timer = slint::Timer::default();
    {
        let load_fn = load_image_at_index.clone();
        let folder_images = folder_images.clone();
        let save_state_fn = save_current_view_state.clone();
        let ui_handle = ui.as_weak();
        let last_win_size = Arc::new(Mutex::new((0.0f32, 0.0f32)));

        drag_drop_timer.start(
            slint::TimerMode::Repeated,
            std::time::Duration::from_millis(50),
            move || {
                if let Some(ui) = ui_handle.upgrade() {
                    let (win_w, win_h) = get_window_logical_size(&ui);
                    let mut prev = last_win_size.lock().unwrap();
                    if (win_w - prev.0).abs() > 1.0 || (win_h - prev.1).abs() > 1.0 {
                        *prev = (win_w, win_h);
                        if ui.get_is_fit_mode() && ui.get_image_loaded() {
                            let img_w = ui.get_img_natural_width();
                            let img_h = ui.get_img_natural_height();
                            let rot = ui.get_rotation_angle();
                            let fit_zoom = calculate_fit_zoom(img_w, img_h, win_w, win_h, rot);
                            ui.set_zoom_factor(fit_zoom);
                            ui.set_zoom_percent_str(format!("{:.0}%", fit_zoom * 100.0).into());
                        }
                    }
                }

                if let Ok(dropped_path) = file_dropped_rx.try_recv() {
                    if is_supported_image(&dropped_path) {
                        save_state_fn();
                        if let Some(folder) = dropped_path.parent() {
                            let all_imgs = scan_folder(folder);
                            let selected_idx = all_imgs
                                .iter()
                                .position(|p| p == &dropped_path)
                                .unwrap_or(0);
                            *folder_images.lock().unwrap() = all_imgs;
                            load_fn(selected_idx, false);
                        }
                    }
                }
            },
        );
    }

    // Callback: Next Image (Right Arrow / Space / PageDown)
    {
        let load_fn = load_image_at_index.clone();
        let folder_images = folder_images.clone();
        let current_index = current_index.clone();
        let save_state_fn = save_current_view_state.clone();

        ui.on_next_image_clicked(move || {
            let total = folder_images.lock().unwrap().len();
            if total == 0 {
                return;
            }
            save_state_fn();
            let cur = *current_index.lock().unwrap();
            let next_idx = (cur + 1) % total;
            load_fn(next_idx, false);
        });
    }

    // Callback: Previous Image (Left Arrow / PageUp)
    {
        let load_fn = load_image_at_index.clone();
        let folder_images = folder_images.clone();
        let current_index = current_index.clone();
        let save_state_fn = save_current_view_state.clone();

        ui.on_prev_image_clicked(move || {
            let total = folder_images.lock().unwrap().len();
            if total == 0 {
                return;
            }
            save_state_fn();
            let cur = *current_index.lock().unwrap();
            let prev_idx = if cur == 0 { total - 1 } else { cur - 1 };
            load_fn(prev_idx, false);
        });
    }

    // Callback: First Image
    {
        let load_fn = load_image_at_index.clone();
        let folder_images = folder_images.clone();
        let save_state_fn = save_current_view_state.clone();

        ui.on_first_image_clicked(move || {
            if !folder_images.lock().unwrap().is_empty() {
                save_state_fn();
                load_fn(0, false);
            }
        });
    }

    // Callback: Last Image
    {
        let load_fn = load_image_at_index.clone();
        let folder_images = folder_images.clone();
        let save_state_fn = save_current_view_state.clone();

        ui.on_last_image_clicked(move || {
            let total = folder_images.lock().unwrap().len();
            if total > 0 {
                save_state_fn();
                load_fn(total - 1, false);
            }
        });
    }

    // Callback: Zoom In (Up Arrow / +)
    {
        let ui_handle = ui.as_weak();
        let save_state_fn = save_current_view_state.clone();

        ui.on_zoom_in_clicked(move || {
            if let Some(ui) = ui_handle.upgrade() {
                let cur_zoom = ui.get_zoom_factor();
                let new_zoom = (cur_zoom * 1.25).min(30.0);
                let mx = ui.get_mouse_x();
                let my = ui.get_mouse_y();
                zoom_at_point(&ui, new_zoom, mx, my);
                save_state_fn();
            }
        });
    }

    // Callback: Zoom Out (Down Arrow / -)
    {
        let ui_handle = ui.as_weak();
        let save_state_fn = save_current_view_state.clone();

        ui.on_zoom_out_clicked(move || {
            if let Some(ui) = ui_handle.upgrade() {
                let cur_zoom = ui.get_zoom_factor();
                let new_zoom = (cur_zoom / 1.25).max(0.05);
                let mx = ui.get_mouse_x();
                let my = ui.get_mouse_y();
                zoom_at_point(&ui, new_zoom, mx, my);
                save_state_fn();
            }
        });
    }

    // Callback: Fit Screen Mode (Esc / 0)
    {
        let ui_handle = ui.as_weak();
        let save_state_fn = save_current_view_state.clone();

        ui.on_zoom_fit_clicked(move || {
            if let Some(ui) = ui_handle.upgrade() {
                let (win_w, win_h) = get_window_logical_size(&ui);
                let img_w = ui.get_img_natural_width();
                let img_h = ui.get_img_natural_height();
                let rot = ui.get_rotation_angle();

                let fit_zoom = calculate_fit_zoom(img_w, img_h, win_w, win_h, rot);
                ui.set_zoom_factor(fit_zoom);
                ui.set_offset_x(0.0);
                ui.set_offset_y(0.0);
                ui.set_zoom_percent_str(format!("{:.0}%", fit_zoom * 100.0).into());
                ui.set_is_fit_mode(true);
                save_state_fn();
            }
        });
    }

    // Callback: 100% Zoom
    {
        let ui_handle = ui.as_weak();
        let save_state_fn = save_current_view_state.clone();

        ui.on_zoom_100_clicked(move || {
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_zoom_factor(1.0);
                ui.set_offset_x(0.0);
                ui.set_offset_y(0.0);
                ui.set_zoom_percent_str("100%".into());
                ui.set_is_fit_mode(false);
                save_state_fn();
            }
        });
    }

    // Callback: Rotate Left
    {
        let ui_handle = ui.as_weak();
        let save_state_fn = save_current_view_state.clone();

        ui.on_rotate_left_clicked(move || {
            if let Some(ui) = ui_handle.upgrade() {
                let cur = ui.get_rotation_angle();
                let next = (cur + 270) % 360;
                ui.set_rotation_angle(next);
                if ui.get_is_fit_mode() {
                    let (win_w, win_h) = get_window_logical_size(&ui);
                    let img_w = ui.get_img_natural_width();
                    let img_h = ui.get_img_natural_height();
                    let fit_zoom = calculate_fit_zoom(img_w, img_h, win_w, win_h, next);
                    ui.set_zoom_factor(fit_zoom);
                    ui.set_offset_x(0.0);
                    ui.set_offset_y(0.0);
                    ui.set_zoom_percent_str(format!("{:.0}%", fit_zoom * 100.0).into());
                }
                save_state_fn();
            }
        });
    }

    // Callback: Rotate Right
    {
        let ui_handle = ui.as_weak();
        let save_state_fn = save_current_view_state.clone();

        ui.on_rotate_right_clicked(move || {
            if let Some(ui) = ui_handle.upgrade() {
                let cur = ui.get_rotation_angle();
                let next = (cur + 90) % 360;
                ui.set_rotation_angle(next);
                if ui.get_is_fit_mode() {
                    let (win_w, win_h) = get_window_logical_size(&ui);
                    let img_w = ui.get_img_natural_width();
                    let img_h = ui.get_img_natural_height();
                    let fit_zoom = calculate_fit_zoom(img_w, img_h, win_w, win_h, next);
                    ui.set_zoom_factor(fit_zoom);
                    ui.set_offset_x(0.0);
                    ui.set_offset_y(0.0);
                    ui.set_zoom_percent_str(format!("{:.0}%", fit_zoom * 100.0).into());
                }
                save_state_fn();
            }
        });
    }

    // Callback: Mouse Wheel Focal Zoom
    {
        let ui_handle = ui.as_weak();
        let save_state_fn = save_current_view_state.clone();

        ui.on_viewport_scrolled(move |mouse_x, mouse_y, delta_y| {
            if let Some(ui) = ui_handle.upgrade() {
                let cur_zoom = ui.get_zoom_factor();
                let zoom_ratio = if delta_y > 0.0 { 1.18 } else { 1.0 / 1.18 };
                let new_zoom = (cur_zoom * zoom_ratio).clamp(0.05, 30.0);
                zoom_at_point(&ui, new_zoom, mouse_x, mouse_y);
                save_state_fn();
            }
        });
    }

    // Callback: Mouse Drag (Pan)
    {
        let ui_handle = ui.as_weak();
        let save_state_fn = save_current_view_state.clone();

        ui.on_viewport_dragged(move |dx, dy| {
            if let Some(ui) = ui_handle.upgrade() {
                let cur_ox = ui.get_offset_x();
                let cur_oy = ui.get_offset_y();
                ui.set_offset_x(cur_ox + dx);
                ui.set_offset_y(cur_oy + dy);
                ui.set_is_fit_mode(false);
                save_state_fn();
            }
        });
    }

    // Callback: Viewport Double Click (Toggle Fit Screen vs 100%)
    {
        let ui_handle = ui.as_weak();
        let save_state_fn = save_current_view_state.clone();

        ui.on_viewport_double_clicked(move || {
            if let Some(ui) = ui_handle.upgrade() {
                let is_fit = ui.get_is_fit_mode();
                if !is_fit {
                    // Switch to Fit Screen
                    let (win_w, win_h) = get_window_logical_size(&ui);
                    let img_w = ui.get_img_natural_width();
                    let img_h = ui.get_img_natural_height();
                    let rot = ui.get_rotation_angle();

                    let fit_zoom = calculate_fit_zoom(img_w, img_h, win_w, win_h, rot);
                    ui.set_zoom_factor(fit_zoom);
                    ui.set_offset_x(0.0);
                    ui.set_offset_y(0.0);
                    ui.set_zoom_percent_str(format!("{:.0}%", fit_zoom * 100.0).into());
                    ui.set_is_fit_mode(true);
                } else {
                    // Switch to 100% Zoom
                    ui.set_zoom_factor(1.0);
                    ui.set_offset_x(0.0);
                    ui.set_offset_y(0.0);
                    ui.set_zoom_percent_str("100%".into());
                    ui.set_is_fit_mode(false);
                }
                save_state_fn();
            }
        });
    }

    // Callback: Delete image
    {
        let load_fn = load_image_at_index.clone();
        let folder_images = folder_images.clone();
        let current_index = current_index.clone();
        let cache = cache.clone();
        let file_view_states = file_view_states.clone();

        ui.on_delete_image_clicked(move || {
            let mut images = folder_images.lock().unwrap();
            let total = images.len();
            if total == 0 {
                return;
            }
            let cur = *current_index.lock().unwrap();
            let path_to_del = images[cur].clone();

            if let Ok(_) = fs::remove_file(&path_to_del) {
                images.remove(cur);
                cache.lock().unwrap().map.remove(&path_to_del);
                file_view_states.lock().unwrap().remove(&path_to_del);
                let new_total = images.len();
                drop(images);

                if new_total > 0 {
                    let next_idx = if cur >= new_total { new_total - 1 } else { cur };
                    load_fn(next_idx, false);
                }
            }
        });
    }

    // Callback: Close application (Esc key or Close button)
    ui.on_close_requested(move || {
        let _ = slint::quit_event_loop();
    });

    // Callback: Open File Button
    {
        let load_fn = load_image_at_index.clone();
        let folder_images = folder_images.clone();
        let save_state_fn = save_current_view_state.clone();

        ui.on_open_file_clicked(move || {
            if let Some(file_path) = rfd::FileDialog::new()
                .add_filter(
                    "Images",
                    &[
                        "jpg", "jpeg", "png", "webp", "bmp", "gif", "tiff", "tif", "jfif", "ico",
                    ],
                )
                .pick_file()
            {
                if is_supported_image(&file_path) {
                    save_state_fn();
                    if let Some(folder) = file_path.parent() {
                        let all_imgs = scan_folder(folder);
                        let selected_idx =
                            all_imgs.iter().position(|p| p == &file_path).unwrap_or(0);
                        *folder_images.lock().unwrap() = all_imgs;
                        load_fn(selected_idx, false);
                    }
                }
            }
        });
    }

    // Callback: Open Folder Button
    {
        let load_fn = load_image_at_index.clone();
        let folder_images = folder_images.clone();
        let save_state_fn = save_current_view_state.clone();

        ui.on_open_folder_clicked(move || {
            if let Some(folder_path) = rfd::FileDialog::new().pick_folder() {
                save_state_fn();
                let all_imgs = scan_folder(&folder_path);
                if !all_imgs.is_empty() {
                    *folder_images.lock().unwrap() = all_imgs;
                    load_fn(0, false);
                }
            }
        });
    }

    // Check CLI argument if user passed an image file on launch
    if let Some(arg) = std::env::args().nth(1) {
        let path = PathBuf::from(arg);
        if path.exists() && is_supported_image(&path) {
            if let Some(folder) = path.parent() {
                let all_imgs = scan_folder(folder);
                let selected_idx = all_imgs.iter().position(|p| p == &path).unwrap_or(0);
                *folder_images.lock().unwrap() = all_imgs;
                load_image_at_index(selected_idx, false);
            }
        }
    }

    ui.run()
}
