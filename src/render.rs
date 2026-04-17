//! Image loading and sizing helpers.

use std::path::Path;

use anyhow::{Context, Result};
use image::{DynamicImage, imageops::FilterType};
use mirajazz::types::{ImageFormat, ImageMirroring, ImageMode, ImageRotation};

/// Image format used by the N4-family for displayed keys (112x112 JPEG rot 180).
pub fn key_format() -> ImageFormat {
    ImageFormat {
        mode: ImageMode::JPEG,
        size: (112, 112),
        rotation: ImageRotation::Rot180,
        mirror: ImageMirroring::None,
    }
}

/// Load an image from disk and scale to the N4 key size.
pub fn load_key_image(path: &Path) -> Result<DynamicImage> {
    let img = image::open(path).with_context(|| format!("open image {}", path.display()))?;
    let (w, h) = key_format().size;
    Ok(img.resize_exact(w as u32, h as u32, FilterType::Lanczos3))
}

/// A solid-color placeholder tile at the key size (used as fallback / clear).
pub fn solid_tile(rgb: [u8; 3]) -> DynamicImage {
    let (w, h) = key_format().size;
    let mut buf = image::RgbImage::new(w as u32, h as u32);
    for p in buf.pixels_mut() {
        *p = image::Rgb(rgb);
    }
    DynamicImage::ImageRgb8(buf)
}
