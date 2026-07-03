//! Per-video pipeline + rayon pool, fail-fast on corrupt input.
//!
//! Contract: a single corrupt video (ffprobe fails, any frame extraction
//! fails, image write fails) aborts the whole run with a printed error and
//! non-zero exit. Rayon's `collect::<Result<_>>()` short-circuits on the
//! first error and cancels in-flight workers best-effort — the first fatal
//! failure is surfaced through that mechanism (no manual channel/flag).

use crate::frames::{extract_frame, frame_count, offsets, squarify, thumb_dims};
use crate::paths::sheet_path;
use crate::probe::ProbeCache;
use crate::sheet::{self, Layout};
use crate::text::TextRenderer;
use anyhow::Result;
use image::RgbImage;
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

/// Process all videos. Returns `Ok(count)` on full success, or the first
/// fatal error as `Err`. Parallelism is bounded by `jobs`. Fail-fast: the
/// first corrupt video aborts the whole run.
///
/// When `force` is false (the default), a video whose sheet already exists
/// on disk is skipped — it is counted and logged but not regenerated. Pass
/// `force = true` to overwrite existing sheets unconditionally. The
/// returned `count` is the number of sheets actually written (skipped videos
/// are excluded).
///
/// Every video — skipped or not — is probed exactly once through `cache`
/// (probing on miss), so the same single ffprobe call feeds both sheet
/// generation and the end-of-run statistics. Skipped videos are probed solely
/// to populate the cache for stats; that is the cost of full-library
/// coverage while keeping ffprobe to one call per video.
#[allow(clippy::too_many_arguments)]
pub fn run_all(
    videos: &[PathBuf],
    root: &Path,
    screens_dir: &str,
    layout: &Layout,
    jobs: usize,
    renderer: &TextRenderer,
    cache: &ProbeCache,
    force: bool,
) -> Result<usize> {
    let total = videos.len();
    log::info!("processing {total} video(s) on {jobs} worker(s)");

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(jobs.max(1))
        .build()
        .map_err(|e| anyhow::anyhow!("building thread pool: {e}"))?;

    // Progress counter: incremented once per successfully written sheet, so
    // each completion prints a `[done/total]` line regardless of worker
    // interleaving. The count is monotonic; the ordering of paths is not
    // (workers finish in whatever order they complete).
    let done = AtomicUsize::new(0);
    // Skipped-sheets counter: incremented once per video whose sheet already
    // existed on disk (and was therefore not regenerated when `force` is
    // false). Reported once at the end of the run.
    let skipped = AtomicUsize::new(0);

    // `collect::<Result<Vec<_>>>()` short-circuits on the first `Err`: rayon
    // cancels in-flight tasks best-effort and surfaces that error. This is
    // the fail-fast contract.
    let results: Vec<()> = pool.install(|| {
        videos
            .par_iter()
            .map(|src| {
                let dest = sheet_path(root, screens_dir, src)?;

                // Skip already-generated sheets unless --force was passed.
                if !force && dest.exists() {
                    let s = skipped.fetch_add(1, Ordering::Relaxed) + 1;
                    log::info!(
                        "{src}: sheet already exists, skipping ({s} skipped so far; \
                         use --force to regenerate) -> {dest}",
                        src = src.display(),
                        dest = dest.display(),
                    );
                    // Probe solely to populate the shared cache so the
                    // stats pass can cover this video without a second
                    // ffprobe call.
                    let _meta = cache.get_or_probe(src).map_err(|e| {
                        log::error!("{}: {e}", src.display());
                        anyhow::anyhow!("{}: corrupt ({e})", src.display())
                    })?;
                    return Ok(());
                }

                process_one(src, &dest, layout, renderer, cache).map_err(|e| {
                    log::error!("{}: {e}", src.display());
                    anyhow::anyhow!("{}: corrupt ({e})", src.display())
                })?;
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                log::info!("[{n}/{total}] wrote {}", dest.display());
                Ok(())
            })
            .collect::<Result<Vec<_>>>()
    })?;

    let processed = results.len() - skipped.load(Ordering::Relaxed);
    let skipped_count = skipped.load(Ordering::Relaxed);
    log::info!("processed {processed}/{total} video(s)");
    if skipped_count > 0 {
        log::info!(
            "skipped {skipped_count} video(s) with existing sheet(s); \
             pass --force to regenerate"
        );
    }
    Ok(processed)
}

/// Process one video end-to-end: probe → compute n → extract n frames →
/// composite → write JPEG. `dest` is the precomputed sheet output path
/// (see [`crate::paths::sheet_path`]); the caller checks it for existence
/// to implement the skip-existing default / `--force` override, so this
/// function unconditionally writes the sheet. Returns `Ok(())` on success.
fn process_one(
    src: &Path,
    dest: &Path,
    layout: &Layout,
    renderer: &TextRenderer,
    cache: &ProbeCache,
) -> Result<()> {
    log::debug!("{}: probing", src.display());
    let meta = cache.get_or_probe(src)?;
    let n_target = frame_count(meta.duration);
    // Derive a full grid (cols×rows) that is as square as possible given the
    // source aspect ratio. The grid cell count `n` is the *actual* number of
    // frames to sample: squarify may bump `n_target` up or down so every grid
    // cell is filled (no ragged last row).
    let aspect = meta.video.width as f64 / meta.video.height.max(1) as f64;
    let (cols, rows) = squarify(n_target, aspect);
    let n = (cols * rows) as usize;
    let offs = offsets(meta.duration, n);
    log::debug!(
        "{}: duration={:.3}s n_target={n_target:.2} -> grid {cols}x{rows} (n={n}) aspect={aspect:.3}",
        src.display(),
        meta.duration
    );
    log::debug!("{}: offsets={:?}", src.display(), offs);

    // Thumbnail dimensions are derived per video from the source aspect ratio
    // and a fixed megapixel budget: `thumb_w*thumb_h ≈ thumb_mp` while keeping
    // `thumb_w/thumb_h == src_w/src_h`. This bounds both dimensions so a
    // portrait clip's sheet cannot out-size a landscape clip's.
    let (thumb_w, thumb_h) = thumb_dims(meta.video.width, meta.video.height, layout.thumb_mp);
    let src_area = meta.video.width as f64 * meta.video.height as f64;
    let capped = src_area <= layout.thumb_mp * 1_000_000.0;
    log::debug!(
        "{}: src {}x{} -> thumb {thumb_w}x{thumb_h} ({:.3} MP target{})",
        src.display(),
        meta.video.width,
        meta.video.height,
        layout.thumb_mp,
        if capped {
            "; source below MP target, matched frame size"
        } else {
            ""
        }
    );

    log::debug!("{}: sheet dest = {}", src.display(), dest.display());
    // `TempDir` is removed (with contents) on drop — even on early return /
    // panic — so cleanup is robust without manual `remove_dir_all`.
    let tmp = TempDir::new().map_err(|e| anyhow::anyhow!("creating temp dir: {e}"))?;
    log::debug!("{}: temp dir = {}", src.display(), tmp.path().display());
    let mut frames: Vec<RgbImage> = Vec::with_capacity(n);

    log::debug!("{}: extracting {n} frame(s)", src.display());
    for (i, off) in offs.iter().enumerate() {
        let png = tmp.path().join(format!("frame_{i:04}.png"));
        extract_frame(src, *off, thumb_w, thumb_h, layout.jpeg_q, &png)?;
        let img = image::open(&png)
            .map_err(|e| anyhow::anyhow!("decoding frame {i} ({e})"))?
            .to_rgb8();
        log::trace!(
            "{}: frame {i} @ {:.3}s -> {}x{}",
            src.display(),
            off,
            img.width(),
            img.height()
        );
        frames.push(img);
        // Remove mid-loop so we don't accumulate all n PNGs on disk at once.
        let _ = std::fs::remove_file(&png);
    }

    // All thumbnails share the same source aspect, so they all decode to the
    // requested thumb dimensions. Sanity-check the first frame and fall back
    // to the computed dims if ffmpeg ever disagrees.
    if let Some(first) = frames.first()
        && (first.width() != thumb_w || first.height() != thumb_h)
    {
        log::warn!(
            "{}: ffmpeg produced {}x{} (expected {thumb_w}x{thumb_h}); using actual",
            src.display(),
            first.width(),
            first.height()
        );
    }

    log::debug!(
        "{}: thumb = {thumb_w}x{thumb_h}, frames = {}",
        src.display(),
        frames.len()
    );

    log::debug!("{}: compositing sheet", src.display());
    let lines = sheet::header_lines(src, &meta);
    let sheet = sheet::composite_sheet(
        renderer, &lines, &frames, cols, rows, thumb_w, thumb_h, layout,
    )?;
    log::debug!(
        "{}: sheet = {}x{}",
        src.display(),
        sheet.width(),
        sheet.height()
    );
    sheet::write_jpeg(&sheet, dest, layout.jpeg_q)?;
    Ok(())
}
