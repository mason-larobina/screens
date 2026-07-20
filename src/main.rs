//! `screens` — Video Screenlist Sheet Generator.
//!
//! CLI entry point: clap CLI, dispatch, exit codes.
//!
//! - Exit `2` for hard pre-work errors (missing/invalid ROOT, ffmpeg/ffprobe
//!   not on PATH, invalid flag values).
//! - Exit `1` for a failed run (corrupt video, orphan cleanup error).
//! - Exit `0` only when every video produced a sheet and the orphan sweep
//!   completed cleanly.

mod frames;
mod paths;
mod probe;
mod sheet;
mod stats;
mod text;
mod worker;

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::Parser;
use sheet::Layout;
use text::TextRenderer;
/// Run-kind error classification.
///
/// - `Hard`: pre-work problems (bad flags, missing/invalid ROOT, missing
///   ffmpeg/ffprobe) → exit `2`.
/// - `Runtime`: a failure during the actual run (corrupt video, orphan
///   cleanup error) → exit `1`.
#[derive(Debug, thiserror::Error)]
enum RunError {
    #[error("{0}")]
    Hard(String),
    #[error("{0}")]
    Runtime(String),
}

/// Generate JPEG screenlist (contact sheet) images for a directory tree of
/// video files.
#[derive(Debug, Parser)]
#[command(name = "screens", version, about)]
struct Cli {
    /// Input directory tree to scan (required, must be a directory).
    #[arg(value_name = "ROOT")]
    root: PathBuf,

    /// Name of the mirrored output subtree, created directly under ROOT.
    #[arg(long, default_value = "Screens")]
    screens_dir: String,

    /// Target thumbnail area in megapixels (1 MP = 1_000_000 px). The
    /// thumbnail L×H is derived per video from the source aspect ratio so the
    /// area is constant across orientations.
    #[arg(long, default_value_t = 0.3)]
    thumb_mp: f64,

    /// Header font size in pixels.
    #[arg(long, default_value_t = 22)]
    font_size: u32,

    /// Font for the header text. Either a path to a font file (TTF/OTF/TTC)
    /// or a fontconfig family name resolved at runtime via `fc-match`. The
    /// font is **not** bundled with the binary, so it must be installed on
    /// the system; the default `"Noto Sans Mono"` requires that family to be
    /// present (e.g. the `fonts-noto-mono` / `google-noto-sans-mono-fonts`
    /// package, depending on distro). The header layout assumes a
    /// monospace font; a proportional font will still render but the column
    /// width used for word-wrapping is computed from a single glyph.
    #[arg(long, default_value = "Noto Sans Mono")]
    font: String,

    /// Header padding in pixels.
    #[arg(long, default_value_t = 8)]
    header_pad: u32,

    /// Thin black gap between thumbnails, in pixels.
    #[arg(long, default_value_t = 4)]
    gap: u32,

    /// Outer margin in pixels.
    #[arg(long, default_value_t = 0)]
    margin: u32,

    /// JPEG quality (1..100).
    #[arg(long, default_value_t = 85)]
    quality: u32,

    /// Worker parallelism (default = number of CPUs).
    #[arg(long)]
    jobs: Option<usize>,

    /// Disable automatic orphan removal; orphans are still reported on stderr.
    #[arg(long, default_value_t = false)]
    no_orphan_cleanup: bool,

    /// Regenerate sheets that already exist. By default a video whose sheet
    /// already exists on disk is skipped (and counted), so re-runs only do
    /// work for new or changed sources. Pass `--force` to overwrite
    /// existing sheets unconditionally.
    #[arg(long, default_value_t = false)]
    force: bool,

    /// Save every sampled frame at native (full source) resolution into a
    /// `<ROOT>/<frames-dir>/` sibling tree, as JPEG files named
    /// `<complete-filename>.<frame_n>.jpg` (1-based, zero-padded to 4 digits:
    /// `0001`, `0002`, ...) sitting beside the source's relative path. Off by
    /// default. The sheet is built from these same native frames — extracted
    /// once via ffmpeg, then resized to thumbnail size in-process — so
    /// `--frames` adds no extra ffmpeg work: it just re-encodes the already-
    /// decoded native image as a JPEG on disk instead of discarding it.
    /// Sheet output is identical whether or not this flag is set. Like the
    /// screens tree, orphan frame files whose source video no longer exists
    /// (and any extra non-frame `.jpg` images in the tree) are removed
    /// (subject to `--no-orphan-cleanup`), and a video's prior frame files
    /// are cleared before regeneration so stale frames from a prior run (e.g.
    /// a different frame count) are removed.
    #[arg(long, default_value_t = false)]
    frames: bool,

    /// Name of the neighbouring frame-output subtree, created directly under
    /// ROOT when `--frames` is set. Must be a single path component and
    /// must differ from `--screens-dir`. Like `--screens-dir`, this name is
    /// reserved under ROOT (such a directory is pruned from the source scan)
    /// so a pre-existing `<frames-dir>` of generated frames is never
    /// mistaken for source content.
    #[arg(long, default_value = "Frames")]
    frames_dir: String,

    /// Video extensions (case-insensitive, no leading dot) eligible for
    /// sheet generation. Comma-separated. Files whose extension is **not** in
    /// this set are skipped and reported (grouped by extension) so a missed
    /// video type can be spotted and added here.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "mp4,m4v,mkv,mov,avi,wmv,flv,f4v,webm,mpg,mpeg,ts,m2ts,vob,3gp,ogv"
    )]
    video_exts: Vec<String>,
}

fn main() -> ExitCode {
    init_logging();
    match try_main() {
        Ok(code) => code,
        Err(RunError::Hard(msg)) => {
            log::error!("{msg}");
            ExitCode::from(2)
        }
        Err(RunError::Runtime(msg)) => {
            log::error!("{msg}");
            ExitCode::from(1)
        }
    }
}

fn try_main() -> Result<ExitCode, RunError> {
    let cli = Cli::parse();

    log::debug!("flags: {cli:#?}");

    // Validate flags.
    if cli.quality < 1 || cli.quality > 100 {
        return Err(RunError::Hard("--quality must be in 1..=100".into()));
    }
    if cli.thumb_mp <= 0.0 {
        return Err(RunError::Hard("--thumb-mp must be > 0".into()));
    }
    if cli.font_size == 0 {
        return Err(RunError::Hard("--font-size must be >= 1".into()));
    }
    if cli.screens_dir.is_empty() || cli.screens_dir.contains('/') || cli.screens_dir.contains('\\')
    {
        return Err(RunError::Hard(
            "--screens-dir must be a single path component".into(),
        ));
    }
    if cli.frames_dir.is_empty() || cli.frames_dir.contains('/') || cli.frames_dir.contains('\\') {
        return Err(RunError::Hard(
            "--frames-dir must be a single path component".into(),
        ));
    }
    if cli.frames_dir == cli.screens_dir {
        return Err(RunError::Hard(
            "--frames-dir must differ from --screens-dir".into(),
        ));
    }

    let video_exts = paths::VideoExts::from_cli(&cli.video_exts).map_err(RunError::Hard)?;

    // Validate ROOT is a directory.
    if !cli.root.is_dir() {
        return Err(RunError::Hard(format!(
            "ROOT {} is not a directory",
            cli.root.display()
        )));
    }
    let root = cli
        .root
        .canonicalize()
        .map_err(|e| RunError::Hard(format!("ROOT {}: {e}", cli.root.display())))?;

    // Validate ffmpeg/ffprobe on PATH.
    for tool in ["ffmpeg", "ffprobe"] {
        if which::which(tool).is_err() {
            return Err(RunError::Hard(format!("{tool} not found on PATH")));
        }
    }

    let layout = Layout {
        gap: cli.gap,
        outer: cli.margin,
        thumb_mp: cli.thumb_mp,
        font_size: cli.font_size,
        header_pad: cli.header_pad,
        jpeg_q: cli.quality,
    };

    let jobs = cli.jobs.unwrap_or_else(num_cpus);
    let renderer = load_font(&cli.font)?;
    let cache = probe::ProbeCache::new();

    log::info!("root = {}", root.display());
    log::debug!("layout = {layout:#?}");
    log::info!("jobs = {jobs}");

    // Collect videos.
    log::info!("scanning for videos...");
    let videos = paths::collect_videos(&root, &cli.screens_dir, &cli.frames_dir, &video_exts);
    let video_count = videos.len();
    log::info!("found {} video(s)", video_count);
    log::debug!("videos: {videos:#?}");

    // Process videos (fail-fast on corrupt input → runtime error). The
    // worker logs its own start/done lines, including a per-sheet
    // `[done/total]` progress line as each sheet is written.
    let processed = worker::run_all(
        &videos,
        &root,
        &cli.screens_dir,
        &cli.frames_dir,
        &layout,
        jobs,
        &renderer,
        &cache,
        cli.force,
        cli.frames,
    )
    .map_err(|e| RunError::Runtime(e.to_string()))?;

    // Orphan sweep (scoped to the screens tree).
    log::info!("scanning for orphan sheets...");
    let orphans = paths::find_orphans(&root, &cli.screens_dir)
        .map_err(|e| RunError::Runtime(e.to_string()))?;
    log::info!("found {} orphan sheet(s)", orphans.len());
    log::debug!("orphans: {orphans:#?}");

    // Orphan sweep (scoped to the frames tree). Runs regardless of `--frames`
    // so stale frame files left by a prior `--frames` run are cleaned even on
    // a run that does not keep frames this time. Gated by `--no-orphan-cleanup`
    // like the screens sweep.
    log::info!("scanning for orphan frame files...");
    let frame_orphans = paths::find_frame_orphans(&root, &cli.frames_dir)
        .map_err(|e| RunError::Runtime(e.to_string()))?;
    log::info!("found {} orphan frame file(s)", frame_orphans.len());
    log::debug!("frame orphans: {frame_orphans:#?}");

    if !cli.no_orphan_cleanup {
        let orphans_removed = paths::cleanup_orphans(&orphans, &root, &cli.screens_dir)
            .map_err(|e| RunError::Runtime(e.to_string()))?;
        log::info!("orphan sheets removed count = {orphans_removed}");
        let frame_orphans_removed =
            paths::cleanup_frame_orphans(&frame_orphans, &root, &cli.frames_dir)
                .map_err(|e| RunError::Runtime(e.to_string()))?;
        log::info!("orphan frame files removed count = {frame_orphans_removed}");
    };

    log::info!("done: processed {processed}");

    // Library statistics: group and count every video under root by
    // extension, resolution, video codec, and audio codec. Rendered as a
    // plain-text block on stdout. Reuses the per-video probe results cached during the run
    // (every video — skipped or not — was probed exactly once), so this adds
    // no ffprobe calls.
    print_stats(
        &root,
        &cli.screens_dir,
        &cli.frames_dir,
        &video_exts,
        video_count,
        &cache,
    );

    // Report skipped (non-video) files last, so the warning appears at the
    // end of the log output — after all sheet/orphan work is complete.
    warn_skipped_files(
        &root,
        &cli.screens_dir,
        &cli.frames_dir,
        &video_exts,
        video_count,
    );

    Ok(ExitCode::SUCCESS)
}

/// Resolve and load the header font.
///
/// `spec` is either an existing font file path (loaded directly) or a
/// fontconfig family name resolved to a file via `fc-match`. The font is
/// **not** bundled with the binary, so it must be installed on the system;
/// `fc-match` is only required when `spec` is a family name rather than a
/// direct file path. A failure to resolve/load the font is a hard pre-work
/// error (exit `2`).
fn load_font(spec: &str) -> Result<TextRenderer, RunError> {
    let path = resolve_font_path(spec)?;
    log::info!("font = {spec:?} -> {}", path.display());
    let bytes = std::fs::read(&path)
        .map_err(|e| RunError::Hard(format!("reading font {}: {e}", path.display())))?;
    TextRenderer::from_bytes(bytes).map_err(RunError::Hard)
}

/// Resolve a font `spec` to a font file path.
///
/// - If `spec` points to an existing file, it is used as-is (no `fc-match`
///   needed).
/// - Otherwise `spec` is treated as a fontconfig family name and resolved
///   via `fc-match --format='%{file}' <spec>`. This requires fontconfig on
///   PATH; a missing `fc-match` is reported as a hard error.
fn resolve_font_path(spec: &str) -> Result<PathBuf, RunError> {
    let direct = Path::new(spec);
    if direct.is_file() {
        return Ok(direct.to_path_buf());
    }

    // Not a direct path → treat as a fontconfig family name.
    if which::which("fc-match").is_err() {
        return Err(RunError::Hard(
            "fc-match not found on PATH (needed to resolve --font family names; \
             pass --font <PATH> to a font file, or install fontconfig)"
                .into(),
        ));
    }

    let output = std::process::Command::new("fc-match")
        .args(["--format", "%{file}\\n", spec])
        .output()
        .map_err(|e| RunError::Hard(format!("running fc-match: {e}")))?;
    if !output.status.success() {
        return Err(RunError::Hard(format!(
            "fc-match failed for --font {spec:?} (exit {:?})",
            output.status.code()
        )));
    }
    let file = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if file.is_empty() {
        return Err(RunError::Hard(format!(
            "fc-match returned no font file for --font {spec:?}; \
             is the family installed?"
        )));
    }
    Ok(PathBuf::from(file))
}

/// Emit a warning about files under `root` that are **not** eligible for
/// sheet generation (i.e. whose extension is not in `--video-exts`), grouped
/// and counted by lowercased extension.
///
/// One `WARN` line is emitted per skipped extension (sorted), so a missed
/// video type — e.g. a directory full of `.rmvb` clips — stands out and can
/// be added to `--video-exts`. Files with **no** extension are additionally
/// listed one per line (relative path under root), since they cannot be
/// located by grepping for an extension and a bare count is little help in a
/// large directory. A single summary line then classifies the
/// directory:
///
/// - skipped files present and no eligible videos were found → the directory
///   appears video-only but uses formats this run does not cover (every file
///   was skipped);
/// - skipped files present alongside eligible videos → mixed content (videos
///   plus other files).
///
/// Called at the end of a run (after sheet/orphan work completes) so the
/// warning appears at the end of the log output. `video_count` is the
/// number of eligible videos collected, used only for the mixed/standalone
/// classification.
fn warn_skipped_files(
    root: &std::path::Path,
    screens_dir: &str,
    frames_dir: &str,
    video_exts: &paths::VideoExts,
    video_count: usize,
) {
    let skipped = paths::non_video_by_ext(root, screens_dir, frames_dir, video_exts);
    if skipped.by_ext.is_empty() {
        return;
    }

    let total: usize = skipped.by_ext.iter().map(|(_, n)| n).sum();
    let ext_kinds = skipped.by_ext.len();

    for (ext, n) in &skipped.by_ext {
        log::warn!(
            "skipped {n} file(s) of type {} (not in --video-exts)",
            paths::fmt_ext(ext)
        );
    }

    // No-extension files are listed individually (relative path under root):
    // unlike extension-bearing files, there is no extension to grep for, so a
    // count alone makes them hard to track down in a large directory.
    for rel in &skipped.no_ext_paths {
        log::warn!("skipped file without extension: {}", rel.display());
    }

    if video_count == 0 {
        log::warn!(
            "skipped {total} file(s) across {ext_kinds} extension(s) under root; \
             no eligible videos found — the directory appears video-only but uses \
             formats this run does not cover (extend --video-exts if any are video)"
        );
    } else {
        log::warn!(
            "skipped {total} non-video file(s) across {ext_kinds} extension(s) under root \
             ({video_count} eligible video(s) also found) — mixed videos and other content"
        );
    }
}

/// Emit end-of-run library statistics: every video under `root` grouped
/// and counted by extension, resolution, video codec, and audio codec,
/// rendered as a plain-text block on stdout. Reuses the per-video probe
/// results cached
/// during the run via `cache`, so no ffprobe calls are added. `video_count`
/// is the number of eligible videos collected, used only to short-circuit
/// the (cheap) report when there are none.
fn print_stats(
    root: &std::path::Path,
    screens_dir: &str,
    frames_dir: &str,
    video_exts: &paths::VideoExts,
    video_count: usize,
    cache: &probe::ProbeCache,
) {
    if video_count == 0 {
        return;
    }
    // Re-collect the (sorted) video list so the report order is deterministic
    // and independent of worker completion order. The probe results are read
    // from the cache populated during the run.
    let videos = paths::collect_videos(root, screens_dir, frames_dir, video_exts);
    let stats = stats::Stats::from_cache(&videos, cache);
    println!("{}", stats.render());
}

/// Initialize `env_logger`. Defaults to the `info` level when `RUST_LOG` is
/// unset, so system progress is visible without extra flags.
fn init_logging() {
    let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp(None)
        .try_init();
}

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}
