//! ab_glyph wrapper around a font loaded from the system (not bundled).
//!
//! The font bytes are read at runtime — from a file path or from a fontconfig
//! family resolved via `fc-match` (see `main.rs`). No font is embedded in the
//! binary, so there is no bundled-asset licensing concern; the user is expected
//! to have the font installed on the system (default `--font "Noto Sans Mono"`).
//!
//! `FontArc` owns the font data, so `TextRenderer` is self-contained with no
//! lifetime entanglement on the caller's buffers.

use ab_glyph::{Font, FontArc, PxScale, ScaleFont};
use image::{Rgb, RgbImage};
use imageproc::drawing::draw_text_mut;

/// A loaded monospace font ready for rasterization.
pub struct TextRenderer {
    font: FontArc,
}

impl TextRenderer {
    /// Load a font from raw font-file bytes (TTF/OTF/TTC). The bytes are owned
    /// by the returned `TextRenderer` (via `FontArc`), so the caller's buffer
    /// does not need to outlive this struct.
    ///
    /// Returns an error if the bytes are not a parseable font (bad file, wrong
    /// format, etc.) — this is a runtime condition, not a build-time one.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, String> {
        let font =
            FontArc::try_from_vec(bytes).map_err(|e| format!("font data is malformed: {e}"))?;
        Ok(Self { font })
    }

    /// Render a single logical line of text as one visual row, drawing onto
    /// `img` at `(x, y)` (baseline at `y + ascent`). Returns the pixel advance
    /// width consumed.
    ///
    /// Uses `imageproc::drawing::draw_text_mut`, which blends the text color
    /// over the existing canvas pixel by glyph coverage. The header strip is
    /// always allocated black (`Rgb([0,0,0])`), so this reduces to
    /// `color * coverage` — identical to the prior hand-rolled blend.
    pub fn draw_line(
        &self,
        img: &mut RgbImage,
        text: &str,
        x: u32,
        y: u32,
        font_size: f32,
        color: Rgb<u8>,
    ) -> u32 {
        draw_text_mut(
            img,
            color,
            x as i32,
            y as i32,
            PxScale::from(font_size),
            &self.font,
            text,
        );
        self.measure_width(text, font_size)
    }

    /// Measure the pixel advance width of a string at the given font size,
    /// without drawing.
    pub fn measure_width(&self, text: &str, font_size: f32) -> u32 {
        let scale = PxScale::from(font_size);
        let scaled = self.font.as_scaled(scale);
        let mut w = 0.0f32;
        for ch in text.chars() {
            w += scaled.h_advance(scaled.glyph_id(ch));
        }
        w.round() as u32
    }

    /// Word-wrap a single logical line to fit `max_w` pixels, returning the
    /// list of visual rows.
    ///
    /// The font is monospaced (the default, Noto Sans Mono, is), so one column
    /// equals the advance width of one glyph. We convert `max_w` to a column
    /// of one glyph. We convert `max_w` to a column count and delegate to
    /// `textwrap`, with `break_words(true)` so a single word longer than the
    /// available width is hard-broken across it (the rare case the prior
    /// char-by-char fallback handled).
    ///
    /// Whitespace between words is pre-normalized to single spaces to match
    /// the prior `split_whitespace` collapse behavior.
    pub fn wrap_line(&self, line: &str, font_size: f32, max_w: u32) -> Vec<String> {
        let char_w = self.measure_width("M", font_size).max(1);
        let cols = ((max_w / char_w) as usize).max(1);
        let normalized: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        textwrap::wrap(&normalized, textwrap::Options::new(cols).break_words(true))
            .into_iter()
            .map(|cow| cow.into_owned())
            .collect()
    }
}
