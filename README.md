# Picasa Photo Viewer (Rust + Slint)

A high-performance, minimalist Google Picasa-style photo viewer built with Rust and Slint UI.

## Features

- 🖼️ **High-Resolution Support**: Fast multithreaded decoding for 8K+ images with background LRU caching.
- 📐 **Smart Contain Fit Mode**: Default 90% container fit with padding, preserving exact image aspect ratios without cropping.
- ↔️ **Sequential Directory Navigation**: Switch through all photos in the same folder using `←` / `→` or `A` / `D` keys.
- 🔍 **Cursor-Centered Focal Zoom**: Smooth, jitter-free focal zoom in/out centered at mouse cursor (`↑` / `↓`, `W` / `S`, mouse wheel, or zoom buttons).
- 🖱️ **Pan & Drag**: Smooth left-click dragging to pan across zoomed images.
- 📱 **Auto EXIF Rotation**: Automatic portrait orientation for vertical photos and smartphone camera JPEGs.
- 🖥️ **Full-Screen Borderless UI**: Seamless borderless frameless window (`no-frame: true`) with translucent dark Picasa backdrop.
- 📂 **Native Open File / Folder Dialogs**: Interactive file and folder pickers via `rfd`.
- 💾 **Session State Memory**: Per-photo zoom level, offset, and rotation state memory during the session.
- ❌ **Instant Exit**: Press `Esc` or click `✕` to quit the application cleanly.

## Shortcuts

| Action | Shortcut |
| :--- | :--- |
| **Prev / Next Photo** | `←` / `→`, `A` / `D`, `Space` |
| **Zoom In / Out** | `↑` / `↓`, `W` / `S`, `+` / `-`, Mouse Wheel |
| **Reset Fit Screen** | `0` or Double-Click |
| **Rotate Photo** | `L` (Left) / `R` (Right) |
| **Delete Photo** | `Delete` |
| **Close Application** | `Esc` |

## Build & Run

```bash
# Run release build
cargo run --release -- path/to/image.jpg

# Generate high-resolution test images
cargo run --example gen_images
```