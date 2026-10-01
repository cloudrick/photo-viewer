use image::{Rgb, RgbImage};
use std::fs;
use std::path::Path;

fn main() {
    let dir = Path::new("test_photos");
    if !dir.exists() {
        fs::create_dir_all(dir).unwrap();
    }

    println!("Generating high-resolution test images (~8000px)...");

    let specs = vec![
        ("photo_01_3x2_8000x5333.png", 8000, 5333, Rgb([30, 144, 255])),  // Blue 3:2
        ("photo_02_16x9_8000x4500.png", 8000, 4500, Rgb([255, 99, 71])),   // Tomato 16:9
        ("photo_03_4x3_8000x6000.png", 8000, 6000, Rgb([46, 139, 87])),   // SeaGreen
        ("photo_04_3x2_8000x5333.png", 8000, 5333, Rgb([147, 112, 219])), // MediumPurple 3:2
        ("photo_05_16x9_8000x4500.png", 8000, 4500, Rgb([255, 165, 0])),  // Orange 16:9
        ("photo_06_portrait_2x3_5333x8000.png", 5333, 8000, Rgb([233, 30, 99])), // Pink Portrait 2:3
    ];

    for (name, w, h, bg_color) in specs {
        let path = dir.join(name);
        println!("Creating {} ({}x{})...", name, w, h);
        let mut img = RgbImage::new(w, h);
        
        // Fill background and draw grid / gradient
        for y in 0..h {
            for x in 0..w {
                let is_grid = (x % 500 < 10) || (y % 500 < 10) || (x % 2000 < 30) || (y % 2000 < 30);
                if is_grid {
                    img.put_pixel(x, y, Rgb([240, 240, 240]));
                } else {
                    let r = ((bg_color[0] as u32 + x / 100) % 256) as u8;
                    let g = ((bg_color[1] as u32 + y / 100) % 256) as u8;
                    let b = bg_color[2];
                    img.put_pixel(x, y, Rgb([r, g, b]));
                }
            }
        }
        
        img.save(&path).expect("Failed to save image");
        println!("Saved {}", path.display());
    }

    println!("All test images generated successfully!");
}
