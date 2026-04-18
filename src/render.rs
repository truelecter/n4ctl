//! Image loading and sizing helpers.
//!
//! Supports raster formats via the `image` crate (PNG/JPEG/BMP/GIF) and vector
//! SVG via `resvg`. SVGs are rasterised at the exact key size so they stay
//! crisp regardless of the source artwork's native resolution. Animated GIFs are
//! decoded to frames at key size; playback is driven by the app event loop (see
//! `state::AppHandle::render_current_page`).

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Local};
use font8x8::UnicodeFonts;
use image::{AnimationDecoder, DynamicImage, ImageBuffer, Rgb, codecs::gif::GifDecoder, imageops::FilterType};
use mirajazz::types::{ImageFormat, ImageMirroring, ImageMode, ImageRotation};
use resvg::{tiny_skia, usvg};

/// Image format used by the N4-family for displayed keys (112x112 JPEG rot 180).
pub fn key_format() -> ImageFormat {
    ImageFormat {
        mode: ImageMode::JPEG,
        size: (112, 112),
        rotation: ImageRotation::Rot180,
        mirror: ImageMirroring::None,
    }
}

/// Key tile size as `(u32, u32)`; avoids repeating the `as u32` casts that most
/// pixel-layout math needs.
fn key_size_u32() -> (u32, u32) {
    let (w, h) = key_format().size;
    (w as u32, h as u32)
}

/// Loaded artwork for one key: a single bitmap or an animated GIF (frames + delays).
pub enum KeyImage {
    Static(DynamicImage),
    Animated {
        frames: Vec<DynamicImage>,
        /// Sleep duration after showing each frame (milliseconds).
        delays_ms: Vec<u32>,
    },
}

/// Load an image from disk and scale to the N4 key size.
///
/// GIF files with multiple frames become [`KeyImage::Animated`]; all other
/// supported rasters and SVG stay [`KeyImage::Static`].
pub fn load_key_visual(path: &Path) -> Result<KeyImage> {
    let (w, h) = key_format().size;
    let w = w as u32;
    let h = h as u32;
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .map(|s| s.trim().to_ascii_lowercase());
    match ext.as_deref() {
        Some("svg") | Some("svgz") => Ok(KeyImage::Static(load_svg(path, w, h)?)),
        Some("gif") => load_gif(path, w, h),
        Some("png") | Some("jpg") | Some("jpeg") | Some("bmp") => {
            let img = image::open(path).with_context(|| format!("open image {}", path.display()))?;
            Ok(KeyImage::Static(img.resize_exact(w, h, FilterType::Lanczos3)))
        }
        _ => {
            if sniff_is_gif(path)? {
                return load_gif(path, w, h);
            }
            let img = image::open(path).with_context(|| format!("open image {}", path.display()))?;
            Ok(KeyImage::Static(img.resize_exact(w, h, FilterType::Lanczos3)))
        }
    }
}

/// True if the file begins with a GIF signature (extensionless or mis-labelled `.gif`).
fn sniff_is_gif(path: &Path) -> Result<bool> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut buf = [0u8; 6];
    let n = f.read(&mut buf).with_context(|| format!("read {}", path.display()))?;
    if n < 6 {
        return Ok(false);
    }
    Ok(&buf == b"GIF87a" || &buf == b"GIF89a")
}

fn load_gif(path: &Path, w: u32, h: u32) -> Result<KeyImage> {
    let file = BufReader::new(
        File::open(path).with_context(|| format!("open gif {}", path.display()))?,
    );
    let decoder = GifDecoder::new(file).with_context(|| format!("gif decoder {}", path.display()))?;
    let frames = decoder
        .into_frames()
        .collect_frames()
        .with_context(|| format!("decode gif {}", path.display()))?;
    if frames.is_empty() {
        return Err(anyhow!("empty gif {}", path.display()));
    }

    let mut imgs = Vec::with_capacity(frames.len());
    let mut delays_ms = Vec::with_capacity(frames.len());
    for frame in frames {
        delays_ms.push(gif_frame_delay_ms(frame.delay()));
        let rgba = DynamicImage::ImageRgba8(frame.into_buffer());
        imgs.push(rgba.resize_exact(w, h, FilterType::Lanczos3));
    }

    if imgs.len() == 1 {
        Ok(KeyImage::Static(imgs.pop().expect("one frame")))
    } else {
        Ok(KeyImage::Animated {
            frames: imgs,
            delays_ms,
        })
    }
}

/// GIF delay → milliseconds; `0` is treated as 100 ms (common convention).
fn gif_frame_delay_ms(delay: image::Delay) -> u32 {
    let dur = std::time::Duration::from(delay);
    let ms = dur.as_millis().min(u128::from(u32::MAX)) as u32;
    if ms == 0 { 100 } else { ms }
}

/// A solid-color placeholder tile at the key size (used as fallback / clear).
pub fn solid_tile(rgb: [u8; 3]) -> DynamicImage {
    let (w, h) = key_size_u32();
    let mut buf = image::RgbImage::new(w, h);
    for p in buf.pixels_mut() {
        *p = image::Rgb(rgb);
    }
    DynamicImage::ImageRgb8(buf)
}

/// Local-time tile: top row = hours (`%H`), bottom row = minutes (`%M`); 8×8 glyphs at 4× scale.
pub fn render_clock_image(now: DateTime<Local>) -> DynamicImage {
    let (tw, th) = key_size_u32();
    let scale = 4u32;
    let gap = 2u32;
    let row_h = 8 * scale;
    let row_gap = 8u32;
    let hours = now.format("%H").to_string();
    let minutes = now.format("%M").to_string();
    let total_h = row_h + row_gap + row_h;
    let y0 = th.saturating_sub(total_h) / 2;
    let mut img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(tw, th, Rgb([0u8, 0u8, 0u8]));
    let fg = Rgb([235u8, 235u8, 235u8]);
    blit_text_centered_row(&mut img, &hours, y0, scale, fg, tw, th, gap);
    blit_text_centered_row(&mut img, &minutes, y0 + row_h + row_gap, scale, fg, tw, th, gap);
    DynamicImage::ImageRgb8(img)
}

fn rect_fill(
    img: &mut ImageBuffer<Rgb<u8>, Vec<u8>>,
    x0: u32,
    y0: u32,
    w: u32,
    h: u32,
    color: Rgb<u8>,
    tw: u32,
    th: u32,
) {
    for y in y0..(y0 + h).min(th) {
        for x in x0..(x0 + w).min(tw) {
            img.put_pixel(x, y, color);
        }
    }
}

/// Draw a rectangular bar: flat `track` fill with a 1px `border` outline.
///
/// Returns the interior rectangle `(inner_x, inner_y, inner_w, inner_h)` suitable
/// for subsequent fill calls, where `inner_*` = the 2-pixel-inset region that
/// `draw_bar_fill_*` expects.
fn draw_bar(
    img: &mut ImageBuffer<Rgb<u8>, Vec<u8>>,
    bx: u32,
    by: u32,
    bw: u32,
    bh: u32,
    track: Rgb<u8>,
    border: Rgb<u8>,
    tw: u32,
    th: u32,
) -> (u32, u32, u32, u32) {
    rect_fill(img, bx, by, bw, bh, track, tw, th);
    rect_fill(img, bx, by, 1, bh, border, tw, th);
    rect_fill(img, bx + bw - 1, by, 1, bh, border, tw, th);
    rect_fill(img, bx, by, bw, 1, border, tw, th);
    rect_fill(img, bx, by + bh - 1, bw, 1, border, tw, th);
    (bx + 2, by + 2, bw.saturating_sub(4), bh.saturating_sub(4))
}

/// Flat-color fill of a bar interior to `level` (0.0..=1.0 of `inner_w`).
fn draw_bar_fill_solid(
    img: &mut ImageBuffer<Rgb<u8>, Vec<u8>>,
    inner: (u32, u32, u32, u32),
    level: f32,
    color: Rgb<u8>,
    tw: u32,
    th: u32,
) {
    let (ix, iy, iw, ih) = inner;
    let level = level.clamp(0.0, 1.0);
    let fill_w = ((iw as f32) * level).round() as u32;
    let fill_w = fill_w.max(if level > 0.0 { 1 } else { 0 }).min(iw);
    rect_fill(img, ix, iy, fill_w, ih, color, tw, th);
}

/// Per-column gradient fill: `color_at_frac(x/inner_w)` is evaluated for each
/// filled column to produce a horizontal gradient up to `level`.
fn draw_bar_fill_gradient<F>(
    img: &mut ImageBuffer<Rgb<u8>, Vec<u8>>,
    inner: (u32, u32, u32, u32),
    level: f32,
    mut color_at_frac: F,
    tw: u32,
    th: u32,
) where
    F: FnMut(f32) -> Rgb<u8>,
{
    let (ix, iy, iw, ih) = inner;
    let level = level.clamp(0.0, 1.0);
    let fill_w = ((iw as f32) * level).round() as u32;
    let fill_w = fill_w.max(if level > 0.001 { 1 } else { 0 }).min(iw);
    for xi in 0..fill_w {
        let frac = (xi as f32 + 0.5) / iw as f32;
        let c = color_at_frac(frac);
        rect_fill(img, ix + xi, iy, 1, ih, c, tw, th);
    }
}

/// Bitmap text at `scale_num / scale_den` (e.g. 3/2 is 1.5x, ~25% smaller than scale 2).
fn blit_text_scaled_rational(
    img: &mut ImageBuffer<Rgb<u8>, Vec<u8>>,
    text: &str,
    mut x: u32,
    y: u32,
    scale_num: u32,
    scale_den: u32,
    fg: Rgb<u8>,
    tw: u32,
    th: u32,
    gap: u32,
) {
    debug_assert!(scale_den > 0);
    let char_w = (8 * scale_num) / scale_den;
    for ch in text.chars() {
        let rows = UnicodeFonts::get(&font8x8::BASIC_FONTS, ch)
            .or_else(|| UnicodeFonts::get(&font8x8::BASIC_FONTS, '?'))
            .expect("font8x8 fallback");
        for row in 0u32..8 {
            let bits = rows[row as usize];
            for col in 0u32..8 {
                let on = bits & (1 << col) != 0;
                if on {
                    let x0 = x + col * scale_num / scale_den;
                    let x1 = x + (col + 1) * scale_num / scale_den;
                    let y0 = y + row * scale_num / scale_den;
                    let y1 = y + (row + 1) * scale_num / scale_den;
                    for py in y0..y1 {
                        for px in x0..x1 {
                            if px < tw && py < th {
                                img.put_pixel(px, py, fg);
                            }
                        }
                    }
                }
            }
        }
        x = x.saturating_add(char_w.saturating_add(gap));
    }
}

fn blit_text_centered_row(
    img: &mut ImageBuffer<Rgb<u8>, Vec<u8>>,
    text: &str,
    y: u32,
    scale: u32,
    fg: Rgb<u8>,
    tw: u32,
    th: u32,
    gap: u32,
) {
    let char_w = 8 * scale;
    let n = text.chars().count() as u32;
    let text_w = n.saturating_mul(char_w).saturating_add(n.saturating_sub(1).saturating_mul(gap));
    let x0 = tw.saturating_sub(text_w) / 2;
    blit_text_scaled_rational(img, text, x0, y, scale, 1, fg, tw, th, gap);
}

/// Default playback volume 0.0..=1.0: centered `SYS`, horizontal bar below, then `000%`.
pub fn render_system_volume_meter(level: f32) -> DynamicImage {
    let level = level.clamp(0.0, 1.0);
    let (tw, th) = key_size_u32();
    let mut img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(tw, th, Rgb([0u8, 0u8, 0u8]));
    let fg_label = Rgb([200u8, 200u8, 200u8]);
    let label_y = 4u32;
    let label_scale = 2u32;
    blit_text_centered_row(&mut img, "SYS", label_y, label_scale, fg_label, tw, th, 2);

    let bx = 10u32;
    let bw = tw.saturating_sub(20);
    let by = label_y + 8 * label_scale + 6;
    let bh = 22u32;
    let track = Rgb([42u8, 42u8, 42u8]);
    let border = Rgb([130u8, 130u8, 130u8]);
    let inner = draw_bar(&mut img, bx, by, bw, bh, track, border, tw, th);

    let r = (255.0 * level) as u8;
    let g = (200.0 * (1.0 - level)) as u8;
    let fill_rgb = Rgb([r, g.saturating_add(40), 48u8]);
    draw_bar_fill_solid(&mut img, inner, level, fill_rgb, tw, th);

    let pct = (level * 100.0).round().clamp(0.0, 100.0) as i32;
    let value = format!("{pct:03}%");
    let y_val = by + bh + 8;
    blit_text_centered_row(&mut img, &value, y_val, 3, Rgb([228u8, 228u8, 228u8]), tw, th, 2);

    DynamicImage::ImageRgb8(img)
}

const VM_GAIN_MIN_DB: f32 = -60.0;
const VM_GAIN_MAX_DB: f32 = 12.0;
/// dB at which the bar color starts blending from green toward red.
const VM_GAIN_GRADIENT_START_DB: f32 = -15.0;

fn vm_bar_color_for_db(db: f32) -> Rgb<u8> {
    let db = db.clamp(VM_GAIN_MIN_DB, VM_GAIN_MAX_DB);
    let green = Rgb([72u8, 200u8, 130u8]);
    let red = Rgb([240u8, 50u8, 55u8]);
    if db <= VM_GAIN_GRADIENT_START_DB {
        green
    } else {
        let span = VM_GAIN_MAX_DB - VM_GAIN_GRADIENT_START_DB;
        let t = ((db - VM_GAIN_GRADIENT_START_DB) / span).clamp(0.0, 1.0);
        let lerp = |a: u8, b: u8| -> u8 { (a as f32 + (b as f32 - a as f32) * t).round() as u8 };
        Rgb([
            lerp(green.0[0], red.0[0]),
            lerp(green.0[1], red.0[1]),
            lerp(green.0[2], red.0[2]),
        ])
    }
}

/// Voicemeeter `Gain` in dB (`VM_GAIN_MIN_DB`..=`VM_GAIN_MAX_DB`): black tile; **top** = rounded dB (centered);
/// **next row** = strip/bus id (e.g. `B0`) **left** at ~1.5× scale, horizontal meter **right**; stack vertically centered.
/// Fill is a horizontal gradient by position on the scale: green through −15 dB, then green→red to +12 dB.
pub fn render_voicemeeter_gain_meter(db: f32, label: &str) -> DynamicImage {
    let db = db.clamp(VM_GAIN_MIN_DB, VM_GAIN_MAX_DB);
    let span = VM_GAIN_MAX_DB - VM_GAIN_MIN_DB;
    let n = ((db - VM_GAIN_MIN_DB) / span).clamp(0.0, 1.0);
    let (tw, th) = key_size_u32();
    let mut img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(tw, th, Rgb([0u8, 0u8, 0u8]));

    let db_i = db.round() as i32;
    let value_str = format!("{db_i}");
    let val_scale = 3u32;
    let val_gap = 2u32;
    let val_h = 8 * val_scale;

    let ch_num = 3u32;
    let ch_den = 2u32;
    let ch_gap = 2u32;
    let ch_h = (8 * ch_num) / ch_den;
    let margin_x = 8u32;
    let row_gap = 10u32;
    let bh = 20u32;
    let row2_h = ch_h.max(bh);
    let block_h = val_h + row_gap + row2_h;
    let y0 = th.saturating_sub(block_h) / 2;
    let val_y = y0;
    let row2_top = y0 + val_h + row_gap;

    blit_text_centered_row(
        &mut img,
        &value_str,
        val_y,
        val_scale,
        Rgb([228u8, 228u8, 228u8]),
        tw,
        th,
        val_gap,
    );

    let ch_n = label.chars().count() as u32;
    let ch_char_w = (8 * ch_num) / ch_den;
    let ch_px = ch_n
        .saturating_mul(ch_char_w)
        .saturating_add(ch_n.saturating_sub(1).saturating_mul(ch_gap));
    let bar_pad = 8u32;
    let bx = margin_x + ch_px + bar_pad;
    let bw = tw.saturating_sub(bx + margin_x).max(24);
    let by = row2_top + row2_h.saturating_sub(bh) / 2;
    let ch_y = row2_top + row2_h.saturating_sub(ch_h) / 2;

    blit_text_scaled_rational(
        &mut img,
        label,
        margin_x,
        ch_y,
        ch_num,
        ch_den,
        Rgb([200u8, 200u8, 200u8]),
        tw,
        th,
        ch_gap,
    );

    let track = Rgb([42u8, 42u8, 42u8]);
    let border = Rgb([130u8, 130u8, 130u8]);
    let inner = draw_bar(&mut img, bx, by, bw, bh, track, border, tw, th);

    draw_bar_fill_gradient(
        &mut img,
        inner,
        n,
        |frac| vm_bar_color_for_db(VM_GAIN_MIN_DB + frac * span),
        tw,
        th,
    );

    DynamicImage::ImageRgb8(img)
}

/// Placeholder when a volume source is unavailable (e.g. Voicemeeter on non-Windows).
#[cfg(not(windows))]
pub fn render_volume_stub(message: &str) -> DynamicImage {
    let (tw, th) = key_size_u32();
    let mut img: ImageBuffer<Rgb<u8>, Vec<u8>> = ImageBuffer::from_pixel(tw, th, Rgb([24u8, 20u8, 28u8]));
    blit_text_centered_row(
        &mut img,
        message,
        th / 2 - 4,
        1,
        Rgb([180u8, 140u8, 100u8]),
        tw,
        th,
        1,
    );
    DynamicImage::ImageRgb8(img)
}

/// Rasterise an SVG file to a `DynamicImage` at exactly `w`x`h` pixels.
/// The SVG is scaled uniformly to fit inside the target while preserving
/// aspect ratio, centered on an opaque black background (since the N4
/// displays do not do alpha).
fn load_svg(path: &Path, w: u32, h: u32) -> Result<DynamicImage> {
    let data = std::fs::read(path).with_context(|| format!("read svg {}", path.display()))?;
    let opts = usvg::Options::default();
    let tree = usvg::Tree::from_data(&data, &opts)
        .with_context(|| format!("parse svg {}", path.display()))?;

    let svg_size = tree.size();
    let scale = (w as f32 / svg_size.width()).min(h as f32 / svg_size.height());
    let tx = (w as f32 - svg_size.width() * scale) * 0.5;
    let ty = (h as f32 - svg_size.height() * scale) * 0.5;

    let mut pixmap = tiny_skia::Pixmap::new(w, h)
        .ok_or_else(|| anyhow!("failed to allocate {w}x{h} pixmap"))?;
    pixmap.fill(tiny_skia::Color::BLACK);

    let transform = tiny_skia::Transform::from_scale(scale, scale).post_translate(tx, ty);
    resvg::render(&tree, transform, &mut pixmap.as_mut());

    // tiny_skia stores premultiplied RGBA. Because we pre-filled the pixmap
    // with opaque black, every pixel ends up fully opaque and the
    // premultiplied channels equal the final composited RGB - just copy
    // them straight out with alpha forced to 255.
    let mut rgba = Vec::with_capacity((w * h * 4) as usize);
    for px in pixmap.pixels() {
        rgba.extend_from_slice(&[px.red(), px.green(), px.blue(), 255]);
    }
    let buf = image::RgbaImage::from_raw(w, h, rgba)
        .ok_or_else(|| anyhow!("could not build rgba image"))?;
    Ok(DynamicImage::ImageRgba8(buf))
}
