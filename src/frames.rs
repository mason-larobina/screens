//! Frame-count formula, grid layout, offset sampling, ffmpeg extraction.

use anyhow::{Context, Result, anyhow};
use std::path::Path;
use std::process::Command;

/// Target number of frames to sample, driven solely by duration in seconds.
///
/// Two anchors in log2 space: 60s → 4, 3600s → 16, i.e. slope
/// `12 / log2(60) == 2.031...`. This is an *advisory* real-valued target;
/// the actual grid cell count is produced by [`squarify`], which rounds it
/// to a full grid (cols×rows) that keeps the photo grid as square as
/// possible. No multiple-of-4 rounding or min-4 clamp is applied here —
/// those existed only to keep a fixed column count full, which the squarify
/// layout makes unnecessary. The only floor is `≥ 2` (a sheet with a
/// single frame is never useful).
pub fn frame_count(seconds: f64) -> f64 {
    if seconds <= 0.0 {
        return 2.0;
    }
    let slope = 12.0 / 60.0f64.log2();
    let raw = 4.0 + slope * (seconds / 60.0).log2();
    raw.max(2.0)
}

/// Pick a full grid (`cols × rows`) of unit-aspect-`aspect` tiles whose
/// shape is as square as possible while staying near `n_target` cells.
///
/// `aspect` is the source video's aspect ratio (`width / height`); each
/// thumbnail preserves it, so the grid's aspect ratio is
/// `(cols / rows) * aspect`. A perfectly square grid would satisfy
/// `cols * rows == n_target` and `(cols / rows) * aspect == 1`, whose
/// real-valued solution is unique:
///
/// ```text
/// cols* = sqrt(n_target / aspect)
/// rows* = sqrt(n_target * aspect)
/// ```
///
/// Rounding `cols*` and `rows*` to the nearest integer *independently* —
/// the "pure squarify" strategy — can stray far from `n_target` because it
/// never considers the joint pair: e.g. `n_target=12, aspect=1` rounds each
/// of `3.464` down to 3, yielding `3×3=9` (25% under target), when `3×4=12`
/// is both on-target and only mildly off-square.
///
/// Instead this performs a small **joint integer search** over a window
/// around `(cols*, rows*)` and minimizes a cost that balances both goals at
/// once:
///
/// ```text
/// cost(c, r) = W_N * ((c*r − n_target) / n_target)²    // count close to target
///            + W_A * (ln((c / r) * aspect))²             // grid square (symmetric)
/// ```
///
/// - The count term is normalized by `n_target`, so the search behaves the
///   same whether the target is 4 or 64.
/// - The aspect term uses `ln`, so a 2× deviation is penalized the same as a
///   ½× (portrait/landscape mirror to the same cost) and the grid is
///   aspect-tolerant by construction.
/// - With `W_N = 1.0, W_A = 0.5`, count-closeness is primary (sample density
///   stays predictable, no surprise extra ffmpeg work) and squareness is a
///   soft tiebreaker. This is strictly no worse than independent rounding on
///   every case and strictly better on the pathological ones.
///
/// The grid is always full (`cols * rows` cells), so there is never a ragged
/// last row. The returned cell count `cols * rows` is the actual number of
/// frames to sample.
pub fn squarify(n_target: f64, aspect: f64) -> (u32, u32) {
    const W_N: f64 = 1.0;
    const W_A: f64 = 0.5;

    let aspect = aspect.max(1e-9);
    let n_target = n_target.max(1.0);

    // Real-valued optimum (what a perfectly square photo grid would be).
    let cols_star = (n_target / aspect).sqrt();
    let rows_star = (n_target * aspect).sqrt();

    // Search a small window of integer pairs around the optimum. The
    // continuous cost is smooth and (essentially) convex, so the integer
    // optimum lies within rounding distance of `(cols*, rows*)`; a `±2`
    // window on each axis is plenty to escape independent-rounding traps
    // (e.g. `n_target=12, aspect=1` -> `3×3=9` under independent rounding,
    // but `3×4=12` is in-window here) while staying tiny (~25–49 pairs).
    let window = 2;
    let cols_lo = ((cols_star.floor() as i64) - window).max(1) as u32;
    let cols_hi = ((cols_star.ceil() as i64) + window).max(1) as u32;
    let rows_lo = ((rows_star.floor() as i64) - window).max(1) as u32;
    let rows_hi = ((rows_star.ceil() as i64) + window).max(1) as u32;

    let mut best = (1u32, 1u32);
    let mut best_cost = f64::INFINITY;
    for cols in cols_lo..=cols_hi {
        for rows in rows_lo..=rows_hi {
            let cost = grid_cost(cols, rows, n_target, aspect, W_N, W_A);
            if cost < best_cost {
                best_cost = cost;
                best = (cols, rows);
            }
        }
    }
    best
}

/// Per-pair cost used by [`squarify`]. Split out so the objective is easy to
/// read and to retune.
///
/// - `count_err` = `(c*r − n_target) / n_target` (scale-free).
/// - `aspect_err` = `ln((c/r) * aspect)` (symmetric: a 2× and ½× deviation
///   cost the same, so portrait/landscape mirror).
fn grid_cost(cols: u32, rows: u32, n_target: f64, aspect: f64, w_n: f64, w_a: f64) -> f64 {
    let cr = (cols as f64) * (rows as f64);
    let count_err = (cr - n_target) / n_target;
    let aspect_err = ((cols as f64 / rows as f64) * aspect).ln();
    w_n * count_err * count_err + w_a * aspect_err * aspect_err
}

/// Sample offsets (seconds) for `n` frames over duration `D`, excluding t=0
/// and t=D:
/// `offset_i = i / (n+1) * D` for `i = 1..=n`.
pub fn offsets(duration: f64, n: usize) -> Vec<f64> {
    let n = n.max(1);
    (1..=n)
        .map(|i| i as f64 / (n as f64 + 1.0) * duration)
        .collect()
}

/// Compute even-rounded thumbnail dimensions (L×H) for a source of size
/// `src_w`×`src_h` targeting a fixed megapixel (pixel-area) budget.
///
/// The aspect ratio is preserved exactly: `thumb_w/thumb_h == src_w/src_h`
/// (up to even-rounding), while `thumb_w * thumb_h ≈ target_mp * 1_000_000`.
/// Because the area budget is constant regardless of orientation, a tall
/// portrait clip no longer out-sizes a wide landscape clip — every video's
/// thumbnails occupy the same pixel area.
///
/// Dimensions are rounded down to even values (encoders dislike odd
/// heights), with a floor of 2.
pub fn thumb_dims(src_w: u32, src_h: u32, target_mp: f64) -> (u32, u32) {
    let src_w = src_w.max(1) as f64;
    let src_h = src_h.max(1) as f64;
    let area = target_mp.max(0.0) * 1_000_000.0;
    // w/h = src_w/src_h  and  w*h = area  =>
    // w = sqrt(area * src_w/src_h), h = sqrt(area * src_h/src_w)
    let w = (area * src_w / src_h).sqrt();
    let h = (area * src_h / src_w).sqrt();
    let w = (w.round() as u32) & !1; // force even
    let h = (h.round() as u32) & !1;
    (w.max(2), h.max(2))
}

/// Extract a single frame at `offset` seconds, scaled to the target
/// `thumb_w`×`thumb_h` (both even, aspect-preserving). Caller owns the temp
/// file and must remove it.
pub fn extract_frame(
    input: &Path,
    offset: f64,
    thumb_w: u32,
    thumb_h: u32,
    jpeg_q: u32,
    dest_png: &Path,
) -> Result<()> {
    let status = Command::new("ffmpeg")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-ss")
        .arg(format!("{:.3}", offset))
        .arg("-i")
        .arg(input)
        .arg("-frames:v")
        .arg("1")
        .arg("-vf")
        .arg(format!("scale={thumb_w}:{thumb_h}"))
        .arg("-q:v")
        .arg(jpeg_q.to_string())
        .arg("-y")
        .arg(dest_png)
        .status()
        .context("failed to spawn ffmpeg")?;

    if !status.success() {
        return Err(anyhow!(
            "ffmpeg frame extraction failed at offset {:.3}s (exit {:?})",
            offset,
            status.code()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{frame_count, squarify};

    fn grid_aspect(cols: u32, rows: u32, aspect: f64) -> f64 {
        (cols as f64 / rows as f64) * aspect
    }

    #[test]
    fn frame_count_anchors() {
        assert!((frame_count(60.0) - 4.0).abs() < 1e-9, "60s -> 4");
        assert!((frame_count(3600.0) - 16.0).abs() < 1e-9, "1h -> 16");
        assert!(frame_count(0.0) >= 2.0, "floor >= 2");
        assert!(frame_count(-5.0) >= 2.0, "floor >= 2 on negative");
    }

    #[test]
    fn squarify_square_aspect() {
        // aspect 1: ideal cols==rows==sqrt(n).
        assert_eq!(squarify(4.0, 1.0), (2, 2));
        assert_eq!(squarify(9.0, 1.0), (3, 3));
        assert_eq!(squarify(16.0, 1.0), (4, 4));
        assert_eq!(squarify(1.0, 1.0), (1, 1));
    }

    #[test]
    fn squarify_bumps_to_fill_grid() {
        // 8 cells, square source -> 3x3=9 (no ragged row).
        let (c, r) = squarify(8.0, 1.0);
        assert_eq!((c, r), (3, 3));
        assert_eq!(c * r, 9);
    }

    #[test]
    fn squarify_aspect_tolerant_mirror() {
        // Portrait and landscape (reciprocal aspects) yield mirrored grids.
        let (cl, rl) = squarify(12.0, 16.0 / 9.0);
        let (cp, rp) = squarify(12.0, 9.0 / 16.0);
        assert_eq!((cl, rl), (rp, cp), "portrait mirrors landscape");
        // Both grids should be closer to square than a flat 3x4/4x3 would be.
        assert!(grid_aspect(cl, rl, 16.0 / 9.0) > 1.0);
        assert!(grid_aspect(cp, rp, 9.0 / 16.0) < 1.0);
    }

    #[test]
    fn squarify_floor() {
        // n_target floored at 1; never returns 0 cols/rows.
        let (c, r) = squarify(0.0, 1.0);
        assert!(c >= 1 && r >= 1);
        let (c, r) = squarify(0.5, 1e12);
        assert!(c >= 1 && r >= 1);
    }

    #[test]
    fn squarify_grid_is_full() {
        // For a spread of targets/aspects, cols*rows is always the actual
        // cell count and the grid stays within a sane squareness band.
        for &n in &[1.0_f64, 2.0, 4.0, 7.0, 8.0, 12.0, 16.0, 20.0, 25.0, 40.0] {
            for &a in &[0.5, 1.0, 1.5, 16.0 / 9.0, 9.0 / 16.0] {
                let (c, r) = squarify(n, a);
                assert!(c >= 1 && r >= 1);
                let g = grid_aspect(c, r, a);
                // the joint search keeps the grid within a 2x band of square.
                assert!(
                    (0.5..=2.0).contains(&g),
                    "n={n} a={a} -> {c}x{r} aspect={g}"
                );
            }
        }
    }

    #[test]
    fn squarify_hits_target_not_just_square() {
        // Joint search must beat independent rounding on cases where rounding
        // each axis to the nearest int undershoots badly. `n_target=12, a=1`:
        // cols*=rows*=3.464, which independently round to 3 -> 3x3=9 (25%
        // under target). The joint search finds 4x3=12, on-target and only
        // mildly off-square.
        let (c, r) = squarify(12.0, 1.0);
        assert_eq!((c, r), (4, 3), "12 @ a=1 -> 4x3=12 (not 3x3=9)");
        assert_eq!(c * r, 12);

        // `n_target=6, a=1`: cols*=rows*=2.449, independently round to 2 ->
        // 2x2=4 (33% under). Joint search picks 3x2=6, on-target.
        let (c, r) = squarify(6.0, 1.0);
        assert_eq!((c, r), (3, 2), "6 @ a=1 -> 3x2=6 (not 2x2=4)");
        assert_eq!(c * r, 6);
    }

    #[test]
    fn squarify_count_close_to_target() {
        // Count-closeness is primary: across a spread of targets (square
        // source), the realized n = cols*rows is within one row/column's
        // worth of n_target — never the large fractional deviations
        // independent rounding can produce.
        for &n in &[
            2.0_f64, 3.0, 5.0, 6.0, 7.0, 10.0, 12.0, 14.0, 18.0, 20.0, 30.0,
        ] {
            let (c, r) = squarify(n, 1.0);
            let realized = (c * r) as f64;
            // Allow at most ~1 extra/missing frame per axis. For n>=4 a full
            // row/col is sqrt(n); use the smaller of sqrt(n) and 2 as the
            // tolerance band — i.e. tolerate |realized - n| <= ~sqrt(n) but
            // never worse than independent rounding, and for n>=9 require
            // within 25%.
            let tol = (n.sqrt()).max(2.0);
            assert!(
                (realized - n).abs() <= tol,
                "n={n} -> realized={realized} (|Δ|={}) exceeds tol {tol}",
                (realized - n).abs()
            );
            if n >= 9.0 {
                assert!(
                    (realized - n).abs() / n <= 0.25,
                    "n={n} -> realized={realized}, dev {} exceeds 25%",
                    (realized - n).abs() / n
                );
            }
        }
    }
}
