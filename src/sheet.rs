//! Canvas allocation, header text rendering, grid compositing, JPEG write.

use crate::probe::{AudioMeta, ProbeMeta, VideoMeta};
use crate::text::TextRenderer;
use anyhow::{Context, Result};
use humansize::{BINARY, format_size};
use image::{Rgb, RgbImage};
use num_format::{Locale, ToFormattedString};
use std::path::Path;

/// Sheet layout constants with flag-driven overrides.
#[derive(Debug, Clone)]
pub struct Layout {
    pub gap: u32,
    pub outer: u32,
    /// Target thumbnail area in megapixels (1 MP = 1_000_000 px). Per-video
    /// thumbnail (L×H) is derived from the source aspect ratio so the area is
    /// constant across orientations — a portrait clip cannot out-size a
    /// landscape clip. See `frames::thumb_dims`.
    pub thumb_mp: f64,
    pub font_size: u32,
    pub header_pad: u32,
    pub jpeg_q: u32,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            gap: 4,
            outer: 0,
            thumb_mp: 0.3,
            font_size: 22,
            header_pad: 8,
            jpeg_q: 85,
        }
    }
}

const WHITE: Rgb<u8> = Rgb([255, 255, 255]);

/// Build the 4 logical header lines (filename, size+duration, video, audio).
pub fn header_lines(src: &Path, meta: &ProbeMeta) -> [String; 4] {
    let basename = src
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("<unknown>");
    let size = humanize_size(meta.size_bytes);
    let dur = format_duration(meta.duration);
    let video = format_video_line(&meta.video, meta.bit_rate.or(meta.video.bit_rate));
    let audio = format_audio_line(&meta.audio);
    [
        basename.to_string(),
        format!("Size: {size}   Duration: {dur}"),
        video,
        audio,
    ]
}

/// Render the header strip onto its own `RgbImage` of width `sheet_w` and a
/// dynamic height computed from word-wrapped lines. Returns `(image, height)`.
pub fn render_header_strip(
    renderer: &TextRenderer,
    lines: &[String; 4],
    sheet_w: u32,
    layout: &Layout,
) -> (RgbImage, u32) {
    let line_height = (layout.font_size as f32 * 1.3).round() as u32;
    let pad = layout.header_pad;
    let text_w = sheet_w.saturating_sub(pad * 2);

    let mut rows: Vec<String> = Vec::new();
    for line in lines {
        let wrapped = renderer.wrap_line(line, layout.font_size as f32, text_w.max(1));
        for w in wrapped {
            rows.push(w);
        }
    }
    let header_h = pad * 2 + rows.len() as u32 * line_height;

    let mut img = RgbImage::from_pixel(sheet_w, header_h.max(1), Rgb([0, 0, 0]));
    let mut y = pad;
    for row in &rows {
        renderer.draw_line(&mut img, row, pad, y, layout.font_size as f32, WHITE);
        y += line_height;
    }
    (img, header_h)
}

/// Composite a full sheet: header strip on top, photo grid below.
///
/// `frames` are the scaled frame images in grid order (left-to-right,
/// top-to-bottom). `cols`/`rows` describe the grid (always full: `frames.len()
/// == cols*rows`), derived per video by [`crate::frames::squarify`] so the
/// grid is as square as possible. `thumb_w` and `thumb_h` are this video's
/// thumbnail dimensions (the same for every frame, since they share one
/// source aspect).
#[allow(clippy::too_many_arguments)]
pub fn composite_sheet(
    renderer: &TextRenderer,
    lines: &[String; 4],
    frames: &[RgbImage],
    cols: u32,
    rows: u32,
    thumb_w: u32,
    thumb_h: u32,
    layout: &Layout,
) -> Result<RgbImage> {
    let n = frames.len() as u32;
    debug_assert_eq!(n, cols * rows, "grid must be full: n == cols*rows");
    let cols = cols.max(1);
    let rows = rows.max(1);

    let sheet_w = layout.outer * 2 + cols * thumb_w + (cols + 1) * layout.gap;
    let (header_img, header_h) = render_header_strip(renderer, lines, sheet_w, layout);

    let grid_h = rows * thumb_h + (rows + 1) * layout.gap;
    let sheet_h = layout.outer * 2 + header_h + grid_h;

    let mut sheet = RgbImage::from_pixel(sheet_w.max(1), sheet_h.max(1), Rgb([0, 0, 0]));

    // Paste header strip at the top.
    image::imageops::overlay(&mut sheet, &header_img, 0, 0);

    for (k, frame) in frames.iter().enumerate() {
        let k = k as u32;
        let col = k % cols;
        let row = k / cols;
        let x = layout.outer + layout.gap + col * (thumb_w + layout.gap);
        let y = layout.outer + header_h + layout.gap + row * (thumb_h + layout.gap);
        image::imageops::overlay(&mut sheet, frame, x as i64, y as i64);
    }
    Ok(sheet)
}

/// Write a sheet to a JPEG file, creating parent dirs as needed.
pub fn write_jpeg(sheet: &RgbImage, dest: &Path, q: u32) -> Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let q = q.clamp(1, 100);
    let file =
        std::fs::File::create(dest).with_context(|| format!("creating {}", dest.display()))?;
    let mut buf = std::io::BufWriter::new(file);
    let mut enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, q as u8);
    enc.encode_image(sheet)
        .with_context(|| format!("encoding JPEG {}", dest.display()))?;
    Ok(())
}

// ---- formatting helpers ----

fn humanize_size(bytes: u64) -> String {
    format_size(bytes, BINARY)
}

fn format_duration(secs: f64) -> String {
    let total = secs.round() as i64;
    let days = total / 86_400;
    let rem = total - days * 86_400;
    let h = rem / 3_600;
    let m = (rem % 3_600) / 60;
    let s = rem % 60;
    if days > 0 {
        format!("{days}d{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

fn format_video_line(v: &VideoMeta, bit_rate: Option<u64>) -> String {
    let codec = v.codec.clone().unwrap_or_else(|| "—".to_string());
    let res = format!("{}x{}", v.width, v.height);
    let br = match bit_rate {
        Some(b) => format!("{} kb/s", (b / 1000).to_formatted_string(&Locale::en)),
        None => "—".to_string(),
    };
    let fps = match v.fps {
        Some(f) => format!("{:.3}", f),
        None => "—".to_string(),
    };
    format!("Video: {codec}  {res}  {br}  {fps} fps")
}

fn format_audio_line(a: &Option<AudioMeta>) -> String {
    match a {
        Some(a) => {
            let codec = a.codec.clone().unwrap_or_else(|| "—".to_string());
            format!(
                "Audio: {}  {}ch  {} Hz",
                codec,
                a.channels,
                a.sample_rate.to_formatted_string(&Locale::en)
            )
        }
        None => "Audio: —".to_string(),
    }
}
