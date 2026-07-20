//! Per-video pipeline + rayon pool, fail-fast on corrupt input.
//!
//! Contract: a single corrupt video (ffprobe fails, any frame extraction
//! fails, image write fails) aborts the whole run with a printed error and
//! non-zero exit. Rayon's `collect::<Result<_>>()` short-circuits on the
//! first error and cancels in-flight workers best-effort — the first fatal
//! failure is surfaced through that mechanism (no manual channel/flag).

use crate::frames::{extract_native_frame, frame_count, offsets, squarify, thumb_dims};
use crate::paths::{clear_video_frames, frame_file_path, sheet_path};
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
///
/// The sheet is built from native-resolution frames (one ffmpeg pass per
/// sampled frame) resized down to thumbnail size in-process, so sheet output
/// is identical whether or not `--frames` is set. When `keep_frames` is true,
/// those native frames are also kept on disk as JPEGs under
/// `root/<frames_dir>/<rel dir>/<complete-filename>.<frame_n>.jpg` (see
/// [`crate::paths::frame_file_path`]); orphan frame files are cleaned
/// separately by the CLI after the run. See [`process_one`] for the
/// per-video frame handling.
#[allow(clippy::too_many_arguments)]
pub fn run_all(
    videos: &[PathBuf],
    root: &Path,
    screens_dir: &str,
    frames_dir: &str,
    layout: &Layout,
    jobs: usize,
    renderer: &TextRenderer,
    cache: &ProbeCache,
    force: bool,
    keep_frames: bool,
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

                process_one(
                    src,
                    &dest,
                    layout,
                    renderer,
                    cache,
                    keep_frames,
                    frames_dir,
                    root,
                )
                .map_err(|e| {
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

/// Process one video end-to-end: probe → compute n → extract n native frames
/// → resize each to thumbnail size → composite → write JPEG. `dest` is the
/// precomputed sheet output path (see [`crate::paths::sheet_path`]); the
/// caller checks it for existence to implement the skip-existing default /
/// `--force` override, so this function unconditionally writes the sheet.
/// Returns `Ok(())` on success.
///
/// Each sampled frame is extracted once via ffmpeg at native (full source)
/// resolution (a temp PNG), then decoded and resized in-process to the
/// megapixel-budgeted thumbnail size for the sheet — so sheet output is
/// identical whether or not `--frames` is set. When `keep_frames` is true the
/// decoded native frame is also re-encoded as a JPEG into the Frames tree at
/// `root/<frames_dir>/<rel dir>/<complete-filename>.<frame_n>.jpg` (see
/// [`crate::paths::frame_file_path`], 1-based `0001`, ...); the video's prior
/// frame files are cleared first so stale frames from a prior run (e.g. a
/// different frame count) are removed. Orphan frame files whose source was
/// deleted are cleaned separately by the CLI after the run.
#[allow(clippy::too_many_arguments)]
fn process_one(
    src: &Path,
    dest: &Path,
    layout: &Layout,
    renderer: &TextRenderer,
    cache: &ProbeCache,
    keep_frames: bool,
    frames_dir: &str,
    root: &Path,
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
    // `TempDir` holds the native PNG intermediates (one ffmpeg pass per
    // sampled frame) and is removed on drop — even on early return / panic —
    // so cleanup is robust without manual `remove_dir_all`. Kept frames are
    // written separately into the Frames tree as JPEGs.
    let tmp = TempDir::new().map_err(|e| anyhow::anyhow!("creating temp dir: {e}"))?;
    log::debug!("{}: temp dir = {}", src.display(), tmp.path().display());

    // When `--frames` is set, clear this video's prior frame files before
    // regenerating, so stale frames from a prior run (e.g. a different frame
    // count after the source changed) are removed — only freshly-sampled
    // frames remain. Orphan frame files whose source was deleted are cleaned
    // separately by the CLI's frame-orphan sweep after the run.
    if keep_frames {
        clear_video_frames(root, frames_dir, src)?;
    }

    log::debug!("{}: extracting {n} frame(s)", src.display());
    let mut frames: Vec<RgbImage> = Vec::with_capacity(n);
    for (i, off) in offs.iter().enumerate() {
        // 1-based, 4-digit zero-padded frame number (0001, 0002, ...).
        let frame_n = i as u32 + 1;
        // One ffmpeg pass at native (full source) resolution -> a temp PNG.
        let png = tmp.path().join(format!("frame_{i:04}.png"));
        extract_native_frame(src, *off, &png)?;
        let dyn_img = image::open(&png).map_err(|e| anyhow::anyhow!("decoding frame {i} ({e})"))?;
        // Resize to the exact thumbnail cell for the sheet. Aspect is already
        // preserved by `thumb_dims`, so this does not distort. Triangle is a
        // bilinear filter — a good, fast downscaler for thumbnails. The sheet
        // is built from this lossless PNG intermediate, so sheet output is
        // identical whether or not `--frames` is set.
        let thumb = dyn_img
            .resize(thumb_w, thumb_h, image::imageops::FilterType::Triangle)
            .to_rgb8();
        log::trace!(
            "{}: frame {frame_n} @ {:.3}s -> native, thumb {}x{}",
            src.display(),
            off,
            thumb.width(),
            thumb.height()
        );
        frames.push(thumb);
        // Keep the native frame as a JPEG in the Frames tree. This re-encodes
        // the already-decoded native image in-process (no extra ffmpeg pass):
        // `--frames` adds one image encode per frame, not a second ffmpeg
        // invocation.
        if keep_frames {
            let jpg = frame_file_path(root, frames_dir, src, frame_n)?;
            let native = dyn_img.to_rgb8();
            sheet::write_jpeg(&native, &jpg, layout.jpeg_q)?;
            log::trace!(
                "{}: kept native frame {frame_n} -> {}",
                src.display(),
                jpg.display()
            );
        }
        // Drop the temp PNG mid-loop so we don't accumulate full-res PNGs.
        let _ = std::fs::remove_file(&png);
    }

    // `image::resize` produces exactly `thumb_w`×`thumb_h`, so every frame
    // fills its grid cell exactly — no size fallback needed.

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
