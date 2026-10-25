slint::include_modules!();

use i_slint_backend_winit::{CustomApplicationHandler, EventResult};
use image::ImageDecoder;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use winit::event::WindowEvent as WinitWindowEvent;

// Hardware capability profile detected at startup via DXGI
#[derive(Clone, Debug)]
pub struct HardwareProfile {
    pub gpu_name: String,
    pub dedicated_vram_mb: usize,
    pub shared_vram_mb: usize,
    pub is_software_adapter: bool,
    pub use_gpu_acceleration: bool,
}

static HARDWARE_PROFILE: OnceLock<HardwareProfile> = OnceLock::new();

#[cfg(target_os = "windows")]
fn probe_hardware_capabilities() -> HardwareProfile {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG, DXGI_ADAPTER_FLAG_SOFTWARE,
    };

    let mut best_name = String::from("Default CPU Renderer");
    let mut best_dedicated_mb = 0usize;
    let mut best_shared_mb = 0usize;
    let mut best_is_software = true;
    let mut best_score = 0usize;

    unsafe {
        if let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() {
            let mut i = 0;
            while let Ok(adapter) = factory.EnumAdapters1(i) {
                i += 1;
                if let Ok(desc) = adapter.GetDesc1() {
                    let is_sw = (DXGI_ADAPTER_FLAG(desc.Flags as i32) & DXGI_ADAPTER_FLAG_SOFTWARE)
                        != DXGI_ADAPTER_FLAG(0);
                    let dedicated_mb = desc.DedicatedVideoMemory / (1024 * 1024);
                    let shared_mb = desc.SharedSystemMemory / (1024 * 1024);

                    let name_len = desc
                        .Description
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(desc.Description.len());
                    let name = String::from_utf16_lossy(&desc.Description[..name_len]);

                    // Score hardware adapters: prefer dedicated VRAM, or high-capacity modern iGPUs
                    let score = if is_sw || name.contains("Basic Render Driver") {
                        0
                    } else {
                        dedicated_mb * 4 + shared_mb
                    };

                    if score > best_score {
                        best_score = score;
                        best_name = name;
                        best_dedicated_mb = dedicated_mb;
                        best_shared_mb = shared_mb;
                        best_is_software = is_sw;
                    }
                }
            }
        }
    }

    // Use GPU acceleration only when a capable hardware GPU is present:
    // - Dedicated GPU with >= 512 MB VRAM, OR
    // - Modern Integrated GPU with >= 128 MB dedicated or >= 2048 MB shared memory
    let use_gpu = !best_is_software
        && (best_dedicated_mb >= 128 || best_shared_mb >= 2048);

    HardwareProfile {
        gpu_name: best_name,
        dedicated_vram_mb: best_dedicated_mb,
        shared_vram_mb: best_shared_mb,
        is_software_adapter: best_is_software,
        use_gpu_acceleration: use_gpu,
    }
}

#[cfg(not(target_os = "windows"))]
fn probe_hardware_capabilities() -> HardwareProfile {
    HardwareProfile {
        gpu_name: String::from("Generic GPU"),
        dedicated_vram_mb: 1024,
        shared_vram_mb: 2048,
        is_software_adapter: false,
        use_gpu_acceleration: true,
    }
}

fn get_hardware_profile() -> &'static HardwareProfile {
    HARDWARE_PROFILE.get_or_init(probe_hardware_capabilities)
}

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
pub enum PixelData {
    Rgb8(slint::SharedPixelBuffer<slint::Rgb8Pixel>),
    Rgba8(slint::SharedPixelBuffer<slint::Rgba8Pixel>),
}

impl PixelData {
    pub fn to_slint_image(&self) -> slint::Image {
        match self {
            PixelData::Rgb8(buf) => slint::Image::from_rgb8(buf.clone()),
            PixelData::Rgba8(buf) => slint::Image::from_rgba8(buf.clone()),
        }
    }
}

#[derive(Clone)]
pub struct DecodedRawImage {
    pub pixel_data: PixelData,
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
    in_flight: HashSet<PathBuf>,
    max_capacity: usize,
}

impl AppCache {
    fn new(capacity: usize) -> Self {
        Self {
            map: HashMap::new(),
            recent_keys: Vec::new(),
            in_flight: HashSet::new(),
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

    fn mark_in_flight(&mut self, path: PathBuf) -> bool {
        if self.map.contains_key(&path) || self.in_flight.contains(&path) {
            false
        } else {
            self.in_flight.insert(path);
            true
        }
    }

    fn clear_in_flight(&mut self, path: &Path) {
        self.in_flight.remove(path);
    }

    fn insert(&mut self, path: PathBuf, data: Arc<DecodedRawImage>) {
        self.in_flight.remove(&path);
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

// Extract embedded JPEG preview (APP1 EXIF thumbnail or MPF preview) for instant <20ms display
fn extract_embedded_jpeg_preview(bytes: &[u8]) -> Option<&[u8]> {
    if bytes.len() < 4 || bytes[0] != 0xFF || bytes[1] != 0xD8 {
        return None;
    }
    let mut best_preview: Option<&[u8]> = None;
    let search_limit = bytes.len().min(512 * 1024); // Search first 512 KB for APP1/APP2 preview headers
    let mut pos = 2;
    while pos + 4 < search_limit {
        if bytes[pos] == 0xFF && bytes[pos + 1] == 0xD8 && bytes[pos + 2] == 0xFF {
            // Found embedded SOI marker; look for corresponding EOI (FF D9)
            let max_end = (pos + 2 * 1024 * 1024).min(bytes.len());
            let mut end = pos + 4;
            while end + 1 < max_end {
                if bytes[end] == 0xFF && bytes[end + 1] == 0xD9 {
                    let candidate = &bytes[pos..=end + 1];
                    if candidate.len() > 2048
                        && best_preview.map_or(true, |b| candidate.len() > b.len())
                    {
                        best_preview = Some(candidate);
                    }
                    pos = end + 2;
                    break;
                }
                end += 1;
            }
        }
        pos += 1;
    }
    best_preview
}

// Fast Stage-1 preview decoder: extracts embedded EXIF/MPF JPEG preview + reads primary image dimensions & orientation
fn decode_quick_preview(path: &Path) -> Option<DecodedRawImage> {
    let ext = path.extension()?.to_str()?.to_lowercase();
    if !matches!(ext.as_str(), "jpg" | "jpeg" | "jfif") {
        return None;
    }

    let metadata = fs::metadata(path).ok()?;
    // Only bother with 2-stage preview for files larger than 500 KB
    if metadata.len() < 500 * 1024 {
        return None;
    }
    let file_size_str = format_file_size(metadata.len());
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown")
        .to_string();
    let file_path = path.to_string_lossy().to_string();

    // Read header + embedded preview bytes
    let file_bytes = fs::read(path).ok()?;
    let preview_bytes = extract_embedded_jpeg_preview(&file_bytes)?;

    // Read main image dimensions and EXIF orientation from header without full decode
    let reader = image::ImageReader::new(Cursor::new(&file_bytes))
        .with_guessed_format()
        .ok()?;
    let mut decoder = reader.into_decoder().ok()?;
    let (raw_w, raw_h) = decoder.dimensions();
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);

    let (natural_w, natural_h) = match orientation {
        image::metadata::Orientation::Rotate90
        | image::metadata::Orientation::Rotate270
        | image::metadata::Orientation::Rotate90FlipH
        | image::metadata::Orientation::Rotate270FlipH => (raw_h, raw_w),
        _ => (raw_w, raw_h),
    };

    // Decode the small embedded preview JPEG
    let mut prev_img = image::load_from_memory_with_format(preview_bytes, image::ImageFormat::Jpeg).ok()?;
    prev_img.apply_orientation(orientation);

    let pw = prev_img.width();
    let ph = prev_img.height();
    let rgb = prev_img.into_rgb8();
    let pixel_buffer =
        slint::SharedPixelBuffer::<slint::Rgb8Pixel>::clone_from_slice(rgb.as_raw(), pw, ph);

    Some(DecodedRawImage {
        pixel_data: PixelData::Rgb8(pixel_buffer),
        width: natural_w,
        height: natural_h,
        file_name,
        file_path,
        file_size_str,
    })
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

// Windows Native WIC Hardware-Accelerated Image Decoder
#[cfg(target_os = "windows")]
fn decode_image_wic_hardware(
    path: &Path,
    file_name: &str,
    file_path: &str,
    file_size_str: &str,
) -> Option<DecodedRawImage> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::{Interface, PCWSTR};
    use windows::Win32::Foundation::GENERIC_READ;
    use windows::Win32::Graphics::Imaging::{
        CLSID_WICImagingFactory, GUID_WICPixelFormat24bppRGB, GUID_WICPixelFormat32bppRGBA,
        IWICBitmapFlipRotator, IWICFormatConverter, IWICImagingFactory, WICBitmapDitherTypeNone,
        WICBitmapPaletteTypeCustom, WICBitmapTransformFlipHorizontal,
        WICBitmapTransformFlipVertical, WICBitmapTransformRotate0, WICBitmapTransformRotate180,
        WICBitmapTransformRotate270, WICBitmapTransformRotate90, WICDecodeMetadataCacheOnDemand,
    };
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
    };

    // Read EXIF orientation quickly via ImageReader header (no pixel decode)
    let orientation = (|| -> Option<image::metadata::Orientation> {
        let file = fs::File::open(path).ok()?;
        let reader = image::ImageReader::new(BufReader::with_capacity(64 * 1024, file))
            .with_guessed_format()
            .ok()?;
        let mut dec = reader.into_decoder().ok()?;
        dec.orientation().ok()
    })()
    .unwrap_or(image::metadata::Orientation::NoTransforms);

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        let factory: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;

        let wide_path: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let decoder = factory
            .CreateDecoderFromFilename(
                PCWSTR(wide_path.as_ptr()),
                None,
                GENERIC_READ,
                WICDecodeMetadataCacheOnDemand,
            )
            .ok()?;

        let frame = decoder.GetFrame(0).ok()?;

        let transform = match orientation {
            image::metadata::Orientation::FlipHorizontal => WICBitmapTransformFlipHorizontal,
            image::metadata::Orientation::Rotate180 => WICBitmapTransformRotate180,
            image::metadata::Orientation::FlipVertical => WICBitmapTransformFlipVertical,
            image::metadata::Orientation::Rotate90FlipH => {
                windows::Win32::Graphics::Imaging::WICBitmapTransformOptions(
                    WICBitmapTransformRotate90.0 | WICBitmapTransformFlipHorizontal.0,
                )
            }
            image::metadata::Orientation::Rotate90 => WICBitmapTransformRotate90,
            image::metadata::Orientation::Rotate270FlipH => {
                windows::Win32::Graphics::Imaging::WICBitmapTransformOptions(
                    WICBitmapTransformRotate270.0 | WICBitmapTransformFlipHorizontal.0,
                )
            }
            image::metadata::Orientation::Rotate270 => WICBitmapTransformRotate270,
            _ => WICBitmapTransformRotate0,
        };

        // Convert to 24bppRGB (for opaque formats like JPEG) or 32bppRGBA (for formats that may have alpha)
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        let is_opaque = matches!(ext.as_str(), "jpg" | "jpeg" | "jfif" | "bmp");

        let converter: IWICFormatConverter = factory.CreateFormatConverter().ok()?;
        let target_guid = if is_opaque {
            &GUID_WICPixelFormat24bppRGB
        } else {
            &GUID_WICPixelFormat32bppRGBA
        };

        converter
            .Initialize(
                &frame,
                target_guid,
                WICBitmapDitherTypeNone,
                None,
                0.0,
                WICBitmapPaletteTypeCustom,
            )
            .ok()?;

        let source_bitmap: windows::Win32::Graphics::Imaging::IWICBitmapSource =
            if transform != WICBitmapTransformRotate0 {
                let rotator: IWICBitmapFlipRotator = factory.CreateBitmapFlipRotator().ok()?;
                rotator.Initialize(&converter, transform).ok()?;
                rotator.cast().ok()?
            } else {
                converter.cast().ok()?
            };

        let mut width = 0u32;
        let mut height = 0u32;
        source_bitmap.GetSize(&mut width, &mut height).ok()?;

        if is_opaque {
            let mut pixel_buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(width, height);
            let slice = pixel_buffer.make_mut_slice();
            let stride = width * 3;
            let buf_bytes = std::slice::from_raw_parts_mut(
                slice.as_mut_ptr() as *mut u8,
                (stride * height) as usize,
            );
            source_bitmap
                .CopyPixels(std::ptr::null(), stride, buf_bytes)
                .ok()?;

            Some(DecodedRawImage {
                pixel_data: PixelData::Rgb8(pixel_buffer),
                width,
                height,
                file_name: file_name.to_string(),
                file_path: file_path.to_string(),
                file_size_str: file_size_str.to_string(),
            })
        } else {
            let mut pixel_buffer =
                slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
            let slice = pixel_buffer.make_mut_slice();
            let stride = width * 4;
            let buf_bytes = std::slice::from_raw_parts_mut(
                slice.as_mut_ptr() as *mut u8,
                (stride * height) as usize,
            );
            source_bitmap
                .CopyPixels(std::ptr::null(), stride, buf_bytes)
                .ok()?;

            Some(DecodedRawImage {
                pixel_data: PixelData::Rgba8(pixel_buffer),
                width,
                height,
                file_name: file_name.to_string(),
                file_path: file_path.to_string(),
                file_size_str: file_size_str.to_string(),
            })
        }
    }
}

// Load and decode an image from disk into Send-able DecodedRawImage
// Automatically chooses between GPU/Hardware-assisted WIC pipeline and multi-threaded CPU SIMD pipeline based on hardware profile.
fn decode_image_file(path: &Path) -> Result<DecodedRawImage, String> {
    let metadata = fs::metadata(path).map_err(|e| e.to_string())?;
    let file_size_str = format_file_size(metadata.len());
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("Unknown")
        .to_string();
    let file_path = path.to_string_lossy().to_string();

    // If hardware profile indicates a capable GPU is available, try Windows WIC Hardware-Accelerated Decoding first
    #[cfg(target_os = "windows")]
    if get_hardware_profile().use_gpu_acceleration {
        if let Some(wic_decoded) =
            decode_image_wic_hardware(path, &file_name, &file_path, &file_size_str)
        {
            return Ok(wic_decoded);
        }
    }

    // Fallback / CPU-preferred path: multi-threaded SIMD Rust image decoder with zero-copy SharedPixelBuffer
    let file = fs::File::open(path).map_err(|e| e.to_string())?;
    let buf_reader = BufReader::with_capacity(256 * 1024, file);

    let reader = image::ImageReader::new(buf_reader)
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

    // Fast path: keep RGB8 without converting to RGBA8 when image has no alpha channel (saves 25% RAM & GPU upload bandwidth)
    let pixel_data = if img.color().has_alpha() {
        let rgba = img.into_rgba8();
        PixelData::Rgba8(
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                rgba.as_raw(),
                width,
                height,
            ),
        )
    } else {
        let rgb = img.into_rgb8();
        PixelData::Rgb8(
            slint::SharedPixelBuffer::<slint::Rgb8Pixel>::clone_from_slice(
                rgb.as_raw(),
                width,
                height,
            ),
        )
    };

    Ok(DecodedRawImage {
        pixel_data,
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
    // Probe GPU/CPU hardware capabilities via DXGI at startup
    let hw_profile = get_hardware_profile();
    let renderer_name = if hw_profile.use_gpu_acceleration {
        "femtovg"
    } else {
        "software"
    };

    // Channel for native OS Drag & Drop file events
    let (file_dropped_tx, file_dropped_rx) = std::sync::mpsc::channel::<PathBuf>();
    let backend = i_slint_backend_winit::Backend::builder()
        .with_renderer_name(renderer_name)
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

        ui.set_is_panning(false);
        ui.set_zoom_factor(new_zoom);
        ui.set_offset_x(new_ox);
        ui.set_offset_y(new_oy);
        ui.set_zoom_percent_str(format!("{:.0}%", new_zoom * 100.0).into());
        ui.set_is_fit_mode(false);
    };

    // Helper to check if a logical viewport point (mx, my) is inside the currently displayed image bounds
    let is_point_inside_image = move |ui: &MainWindow, mx: f32, my: f32| -> bool {
        if !ui.get_image_loaded() {
            return false;
        }
        let (win_w, win_h) = get_window_logical_size(ui);
        let img_w = ui.get_img_natural_width();
        let img_h = ui.get_img_natural_height();
        let zoom = ui.get_zoom_factor();
        let ox = ui.get_offset_x();
        let oy = ui.get_offset_y();
        let rot = ui.get_rotation_angle();

        let (eff_w, eff_h) = if rot == 90 || rot == 270 {
            (img_h * zoom, img_w * zoom)
        } else {
            (img_w * zoom, img_h * zoom)
        };

        let center_x = win_w / 2.0 + ox;
        let center_y = win_h / 2.0 + oy;

        (mx - center_x).abs() <= eff_w / 2.0 && (my - center_y).abs() <= eff_h / 2.0
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

    // Helper to apply a decoded image to the UI
    let apply_image_to_ui = move |ui: &MainWindow,
                                  raw: &DecodedRawImage,
                                  target_index: usize,
                                  total_count: usize,
                                  saved_state: Option<FileViewState>,
                                  preserve_current_view: bool| {
        let slint_image = raw.pixel_data.to_slint_image();
        ui.set_current_image(slint_image);
        ui.set_image_loaded(true);
        ui.set_is_loading(false);
        ui.set_file_name(raw.file_name.clone().into());
        ui.set_full_path(raw.file_path.clone().into());
        ui.set_img_natural_width(raw.width as f32);
        ui.set_img_natural_height(raw.height as f32);
        let accel_badge = if get_hardware_profile().use_gpu_acceleration {
            "GPU"
        } else {
            "CPU"
        };
        ui.set_file_info(
            format!(
                "{} × {} px  •  {}  •  {}",
                raw.width, raw.height, raw.file_size_str, accel_badge
            )
            .into(),
        );
        ui.set_folder_info(format!("[ {} / {} ]", target_index + 1, total_count).into());

        if preserve_current_view {
            return;
        }

        // Apply saved session view state if available; otherwise default to Fit Screen mode
        if let Some(state) = saved_state {
            if state.is_fit_mode {
                let (win_w, win_h) = get_window_logical_size(ui);
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
                ui.set_zoom_percent_str(format!("{:.0}%", state.zoom_factor * 100.0).into());
            }
        } else {
            let (win_w, win_h) = get_window_logical_size(ui);
            let rot = 0;
            let fit_zoom =
                calculate_fit_zoom(raw.width as f32, raw.height as f32, win_w, win_h, rot);
            ui.set_zoom_factor(fit_zoom);
            ui.set_offset_x(0.0);
            ui.set_offset_y(0.0);
            ui.set_rotation_angle(0);
            ui.set_zoom_percent_str(format!("{:.0}%", fit_zoom * 100.0).into());
            ui.set_is_fit_mode(true);
        }
    };

    // Helper to preload adjacent images without competing with active image decoding
    let spawn_preload_neighbors = {
        let cache = cache.clone();
        move |images: Vec<PathBuf>, target_index: usize| {
            let total_count = images.len();
            let pre_indices = [
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

            for idx in pre_indices.into_iter().flatten() {
                let pre_path = images[idx].clone();
                let should_spawn = cache.lock().unwrap().mark_in_flight(pre_path.clone());
                if should_spawn {
                    let cache = cache.clone();
                    rayon::spawn(move || match decode_image_file(&pre_path) {
                        Ok(raw) => {
                            cache.lock().unwrap().insert(pre_path, Arc::new(raw));
                        }
                        Err(_) => {
                            cache.lock().unwrap().clear_in_flight(&pre_path);
                        }
                    });
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
        let spawn_preload = spawn_preload_neighbors.clone();

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
                    apply_image_to_ui(&ui, &raw, target_index, total_count, saved_state, false);
                }
                spawn_preload(images, target_index);
            } else {
                // Not in cache: show loading spinner and decode on background thread with top priority
                if let Some(ui) = ui_handle.upgrade() {
                    ui.set_is_loading(true);
                }

                let _ = cache.lock().unwrap().mark_in_flight(target_path.clone());

                let ui_handle = ui_handle.clone();
                let cache = cache.clone();
                let current_index = current_index.clone();
                let path_to_load = target_path.clone();
                let spawn_preload = spawn_preload.clone();

                thread::spawn(move || {
                    // Stage 1: Try instant embedded preview (<20ms) for large JPEGs so user sees image immediately
                    let mut preview_shown = false;
                    if *current_index.lock().unwrap() == target_index {
                        if let Some(preview_raw) = decode_quick_preview(&path_to_load) {
                            preview_shown = true;
                            let ui_handle_prev = ui_handle.clone();
                            let current_index_prev = current_index.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if *current_index_prev.lock().unwrap() == target_index {
                                    if let Some(ui) = ui_handle_prev.upgrade() {
                                        apply_image_to_ui(
                                            &ui,
                                            &preview_raw,
                                            target_index,
                                            total_count,
                                            saved_state,
                                            false,
                                        );
                                    }
                                }
                            });
                        }
                    }

                    // Stage 2: Decode full-resolution 8K image using dedicated priority thread
                    let res = decode_image_file(&path_to_load);
                    let _ = slint::invoke_from_event_loop(move || {
                        let is_still_current = *current_index.lock().unwrap() == target_index;
                        match res {
                            Ok(raw) => {
                                let raw_arc = Arc::new(raw);
                                cache.lock().unwrap().insert(path_to_load, raw_arc.clone());

                                if is_still_current {
                                    if let Some(ui) = ui_handle.upgrade() {
                                        apply_image_to_ui(
                                            &ui,
                                            &raw_arc,
                                            target_index,
                                            total_count,
                                            saved_state,
                                            preview_shown,
                                        );
                                    }
                                }
                                // Only start background neighbor preloading AFTER active image finishes decoding
                                spawn_preload(images, target_index);
                            }
                            Err(err_msg) => {
                                cache.lock().unwrap().clear_in_flight(&path_to_load);
                                if is_still_current {
                                    if let Some(ui) = ui_handle.upgrade() {
                                        ui.set_is_loading(false);
                                        ui.set_file_info(format!("Error: {}", err_msg).into());
                                    }
                                }
                            }
                        }
                    });
                });
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
                ui.set_is_panning(false);
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
                ui.set_is_panning(false);
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
                    ui.set_is_panning(false);
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
                    ui.set_is_panning(false);
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
                let zoom_ratio = if delta_y > 0.0 { 1.14 } else { 1.0 / 1.14 };
                let new_zoom = (cur_zoom * zoom_ratio).clamp(0.05, 30.0);
                zoom_at_point(&ui, new_zoom, mouse_x, mouse_y);
                save_state_fn();
            }
        });
    }

    // Callback: Mouse Pointer Down & Drag (Pan photo if pressed on photo; drag window if pressed outside photo in Window Mode)
    {
        #[derive(Clone, Copy)]
        enum DragMode {
            None,
            Photo,
            Window {
                start_win_x: i32,
                start_win_y: i32,
                start_cursor: Option<(i32, i32)>,
                down_mx: f32,
                down_my: f32,
            },
        }

        #[cfg(target_os = "windows")]
        fn get_screen_cursor_pos() -> Option<(i32, i32)> {
            #[repr(C)]
            struct Point {
                x: i32,
                y: i32,
            }
            #[link(name = "user32")]
            extern "system" {
                fn GetCursorPos(lp_point: *mut Point) -> i32;
            }
            let mut pt = Point { x: 0, y: 0 };
            if unsafe { GetCursorPos(&mut pt) } != 0 {
                Some((pt.x, pt.y))
            } else {
                None
            }
        }

        #[cfg(not(target_os = "windows"))]
        fn get_screen_cursor_pos() -> Option<(i32, i32)> {
            None
        }

        let drag_mode = Arc::new(Mutex::new(DragMode::None));

        {
            let ui_handle = ui.as_weak();
            let drag_mode = drag_mode.clone();

            ui.on_viewport_pointer_down(move |mx, my| {
                if let Some(ui) = ui_handle.upgrade() {
                    let mode = if is_point_inside_image(&ui, mx, my) {
                        DragMode::Photo
                    } else if !ui.window().is_fullscreen() {
                        let win_pos = ui.window().position();
                        DragMode::Window {
                            start_win_x: win_pos.x,
                            start_win_y: win_pos.y,
                            start_cursor: get_screen_cursor_pos(),
                            down_mx: mx,
                            down_my: my,
                        }
                    } else {
                        DragMode::None
                    };
                    *drag_mode.lock().unwrap() = mode;
                }
            });
        }

        {
            let ui_handle = ui.as_weak();
            let save_state_fn = save_current_view_state.clone();
            let drag_mode = drag_mode.clone();

            ui.on_viewport_dragged(move |dx, dy| {
                if let Some(ui) = ui_handle.upgrade() {
                    let mode = *drag_mode.lock().unwrap();
                    match mode {
                        DragMode::Photo => {
                            let cur_ox = ui.get_offset_x();
                            let cur_oy = ui.get_offset_y();
                            ui.set_offset_x(cur_ox + dx);
                            ui.set_offset_y(cur_oy + dy);
                            ui.set_is_fit_mode(false);
                            save_state_fn();
                        }
                        DragMode::Window {
                            start_win_x,
                            start_win_y,
                            start_cursor,
                            down_mx,
                            down_my,
                        } => {
                            if !ui.window().is_fullscreen() {
                                if let (Some((start_cx, start_cy)), Some((cur_cx, cur_cy))) =
                                    (start_cursor, get_screen_cursor_pos())
                                {
                                    let new_x = start_win_x + (cur_cx - start_cx);
                                    let new_y = start_win_y + (cur_cy - start_cy);
                                    ui.window()
                                        .set_position(slint::PhysicalPosition::new(new_x, new_y));
                                } else {
                                    let scale = ui.window().scale_factor().max(1.0);
                                    let cur_pos = ui.window().position();
                                    let step_x =
                                        ((ui.get_mouse_x() - down_mx) * scale).round() as i32;
                                    let step_y =
                                        ((ui.get_mouse_y() - down_my) * scale).round() as i32;
                                    ui.window().set_position(slint::PhysicalPosition::new(
                                        cur_pos.x + step_x,
                                        cur_pos.y + step_y,
                                    ));
                                }
                            }
                        }
                        DragMode::None => {}
                    }
                }
            });
        }
    }

    // Callback: Viewport Click (Click outside image -> shrink to Window Mode)
    {
        let ui_handle = ui.as_weak();
        let save_state_fn = save_current_view_state.clone();

        ui.on_viewport_clicked(move |mx, my| {
            if let Some(ui) = ui_handle.upgrade() {
                if ui.get_image_loaded() && !is_point_inside_image(&ui, mx, my) {
                    if ui.window().is_fullscreen() {
                        let screen_phys = ui.window().size();
                        let scale = ui.window().scale_factor().max(1.0);
                        let target_log_w = 1280.0_f32;
                        let target_log_h = 800.0_f32;
                        let target_phys_w = (target_log_w * scale) as i32;
                        let target_phys_h = (target_log_h * scale) as i32;
                        let pos_x = ((screen_phys.width as i32 - target_phys_w) / 2).max(0);
                        let pos_y = ((screen_phys.height as i32 - target_phys_h) / 2).max(0);

                        ui.window().set_fullscreen(false);
                        ui.window()
                            .set_size(slint::LogicalSize::new(target_log_w, target_log_h));
                        ui.window()
                            .set_position(slint::PhysicalPosition::new(pos_x, pos_y));

                        if ui.get_is_fit_mode() {
                            let img_w = ui.get_img_natural_width();
                            let img_h = ui.get_img_natural_height();
                            let rot = ui.get_rotation_angle();
                            let fit_zoom =
                                calculate_fit_zoom(img_w, img_h, target_log_w, target_log_h, rot);
                            ui.set_is_panning(false);
                            ui.set_zoom_factor(fit_zoom);
                            ui.set_offset_x(0.0);
                            ui.set_offset_y(0.0);
                            ui.set_zoom_percent_str(format!("{:.0}%", fit_zoom * 100.0).into());
                            save_state_fn();
                        }
                    }
                }
            }
        });
    }

    // Callback: Viewport Double Click (Double-click on image -> switch to Fullscreen Mode)
    {
        let ui_handle = ui.as_weak();
        let save_state_fn = save_current_view_state.clone();

        ui.on_viewport_double_clicked(move |mx, my| {
            if let Some(ui) = ui_handle.upgrade() {
                if ui.get_image_loaded() && is_point_inside_image(&ui, mx, my) {
                    if !ui.window().is_fullscreen() {
                        ui.window().set_fullscreen(true);

                        let ui_weak = ui.as_weak();
                        let save_fn = save_state_fn.clone();
                        slint::Timer::single_shot(
                            std::time::Duration::from_millis(30),
                            move || {
                                if let Some(ui) = ui_weak.upgrade() {
                                    if ui.get_is_fit_mode() {
                                        let (win_w, win_h) = get_window_logical_size(&ui);
                                        let img_w = ui.get_img_natural_width();
                                        let img_h = ui.get_img_natural_height();
                                        let rot = ui.get_rotation_angle();
                                        let fit_zoom =
                                            calculate_fit_zoom(img_w, img_h, win_w, win_h, rot);
                                        ui.set_is_panning(false);
                                        ui.set_zoom_factor(fit_zoom);
                                        ui.set_offset_x(0.0);
                                        ui.set_offset_y(0.0);
                                        ui.set_zoom_percent_str(
                                            format!("{:.0}%", fit_zoom * 100.0).into(),
                                        );
                                        save_fn();
                                    }
                                }
                            },
                        );
                    }
                }
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
