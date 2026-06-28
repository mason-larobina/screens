//! Mirroring logic, whitelist, orphan detection/deletion.

use anyhow::{Context, Result};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

/// Whitelisted video extensions (case-insensitive, no leading dot), as built
/// from the `--video-exts` flag.
///
/// This is a newtype around a `HashSet<String>` of already-normalized
/// (lowercased, dot-free, non-empty) extensions. The constructor
/// [`VideoExts::from_cli`] is the single place that enforces those
/// invariants, so any value of this type can be trusted without re-checking.
#[derive(Debug, Clone)]
pub struct VideoExts(HashSet<String>);

impl VideoExts {
    /// Build a [`VideoExts`] from the raw `--video-exts` CLI vec.
    ///
    /// Each entry is trimmed, lowercased, and dropped if empty. Entries
    /// containing a `.` are rejected (operators should pass `mkv`, not `.mkv`).
    /// Returns an error if no extension survives normalization.
    pub fn from_cli(raw: &[String]) -> Result<Self, String> {
        let set: HashSet<String> = raw
            .iter()
            .map(|e| e.trim().to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        let bad: Vec<&str> = set
            .iter()
            .filter(|e| e.contains('.'))
            .map(String::as_str)
            .collect();
        if !bad.is_empty() {
            return Err(format!(
                "--video-exts entries must not include a leading dot (got: {})",
                bad.join(", ")
            ));
        }
        if set.is_empty() {
            return Err("--video-exts must list at least one extension".into());
        }
        Ok(Self(set))
    }

    /// True iff `path` has a whitelisted video extension (case-insensitive).
    pub fn is_video(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| self.0.contains(&e.to_ascii_lowercase()))
            .unwrap_or(false)
    }

    /// True iff no extensions survived normalization (always false for a
    /// value built via [`Self::from_cli`], which errors in that case).
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Recursively collect video files under `root`, sorted for deterministic
/// output. Any directory whose name equals `screens_dir` is pruned (we never
/// recurse into a previously-generated output tree). `exts` is the
/// lowercased set of eligible extensions (from `--video-exts`).
pub fn collect_videos(root: &Path, screens_dir: &str, exts: &VideoExts) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir() && e.file_name() == std::ffi::OsStr::new(screens_dir) {
                return false;
            }
            true
        })
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        if exts.is_video(p) {
            out.push(p.to_path_buf());
        }
    }
    out.sort();
    out
}

/// Result of scanning `root` for non-video (skipped) files.
///
/// - `by_ext`: counts grouped by lowercased extension, sorted by extension
///   for stable log output. The empty string `""` represents files with no
///   extension.
/// - `no_ext_paths`: the relative paths (under `root`) of every skipped file
///   that has **no** extension, sorted. These are returned separately so the
///   CLI can list them individually — unlike extension-bearing files, a
///   no-extension file cannot be located by grepping for an extension, so a
///   bare count is little help in a large directory.
pub struct Skipped {
    pub by_ext: Vec<(String, usize)>,
    pub no_ext_paths: Vec<PathBuf>,
}

/// Walk `root` (pruning any directory named `screens_dir`, matching
/// [`collect_videos`]) and group every **non-video** file by lowercased
/// extension. Files with no extension are counted under the empty string `""`
/// and their relative paths are also collected into
/// [`Skipped::no_ext_paths`]. The Screens tree itself is never scanned, so
/// generated sheets are never counted here. `exts` is the lowercased set of
/// eligible extensions (from `--video-exts`); a file whose extension is *not*
/// in `exts` is counted here.
///
/// This drives the end-of-run skipped-files warning in the CLI: the operator
/// scans the per-extension counts and, if any look like a missed video
/// container, extends `--video-exts` to cover it. It also makes a mixed
/// (videos + other content) directory visible at a glance.
pub fn non_video_by_ext(root: &Path, screens_dir: &str, exts: &VideoExts) -> Skipped {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut no_ext_paths: BTreeSet<PathBuf> = BTreeSet::new();
    for entry in WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| {
            if e.file_type().is_dir() && e.file_name() == std::ffi::OsStr::new(screens_dir) {
                return false;
            }
            true
        })
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        if exts.is_video(p) {
            continue;
        }
        match p.extension().and_then(|e| e.to_str()) {
            Some(e) => *counts.entry(e.to_ascii_lowercase()).or_insert(0) += 1,
            None => {
                *counts.entry(String::new()).or_insert(0) += 1;
                if let Ok(rel) = p.strip_prefix(root) {
                    no_ext_paths.insert(rel.to_path_buf());
                }
            }
        }
    }
    Skipped {
        by_ext: counts.into_iter().collect(),
        no_ext_paths: no_ext_paths.into_iter().collect(),
    }
}

/// Format an extension/count pair from [`non_video_by_ext`] for display.
/// The empty-string extension (no extension) renders as `"(no extension)"`.
pub fn fmt_ext(ext: &str) -> String {
    if ext.is_empty() {
        "(no extension)".to_string()
    } else {
        format!(".{ext}")
    }
}

/// Compute the mirrored output sheet path for a source video:
/// `ROOT/Screens/<rel dir>/<complete-filename>.jpg` — the source's full
/// filename (extension included) with `.jpg` appended. Keeping the original
/// extension makes the sheet traceable to the exact source file and avoids
/// collisions between, say, `movie.mp4` and `movie.mkv` mapping to the same
/// `movie.jpg`.
pub fn sheet_path(root: &Path, screens_dir: &str, src: &Path) -> Result<PathBuf> {
    let rel = src
        .strip_prefix(root)
        .context("source video is not under --root")?;
    let parent = rel.parent().unwrap_or_else(|| Path::new(""));
    let filename = src
        .file_name()
        .and_then(|s| s.to_str())
        .context("source video has no file name")?;
    let mut out = root.join(screens_dir);
    out.push(parent);
    // Push the complete filename + ".jpg" as a single component. Using
    // `set_extension` would mangle multi-dotted filenames (e.g.
    // "Movie.2024.1080p.mp4" -> it treats "mp4" as the extension and replaces
    // it), so a flat push is correct.
    out.push(format!("{filename}.jpg"));
    Ok(out)
}

/// Relative source path under the screens tree: given
/// `ROOT/Screens/a/b/movie.mp4.jpg`, returns `a/b/movie.mp4` (the source's
/// complete filename, extension included, with the trailing `.jpg` stripped).
pub fn rel_source(root: &Path, screens_dir: &str, sheet: &Path) -> Result<PathBuf> {
    let screens_root = root.join(screens_dir);
    let rel = sheet
        .strip_prefix(&screens_root)
        .context("sheet is not under the screens tree")?;
    let p = rel.with_extension("");
    Ok(p)
}

/// Given a sheet's relative source path (complete filename, extension
/// included), look for a matching source video under `root/<rel_source>`.
/// Because the sheet preserves the original extension, the match is exact —
/// no extension enumeration is needed.
pub fn source_exists(root: &Path, rel_source: &Path) -> bool {
    root.join(rel_source).is_file()
}

/// Scan the `<root>/<screens_dir>` tree for `*.jpg` sheets. Returns sorted
/// list of orphan sheets (those whose source no longer exists).
pub fn find_orphans(root: &Path, screens_dir: &str) -> Result<Vec<PathBuf>> {
    let screens_root = root.join(screens_dir);
    if !screens_root.is_dir() {
        log::debug!("no screens tree at {}", screens_root.display());
        return Ok(Vec::new());
    }
    let mut orphans = Vec::new();
    for entry in WalkDir::new(&screens_root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("jpg") {
            continue;
        }
        let stem = rel_source(root, screens_dir, p)?;
        if !source_exists(root, &stem) {
            orphans.push(p.to_path_buf());
        }
    }
    orphans.sort();
    Ok(orphans)
}

/// Remove a set of orphan sheets, then sweep the screens tree bottom-up for
/// newly-empty directories and remove them. Returns the count removed.
pub fn cleanup_orphans(orphans: &[PathBuf], root: &Path, screens_dir: &str) -> Result<usize> {
    let screens_root = root.join(screens_dir);
    let mut removed = 0usize;
    for o in orphans {
        if let Err(e) = std::fs::remove_file(o) {
            if e.kind() != std::io::ErrorKind::NotFound {
                return Err(anyhow::anyhow!(
                    "failed to remove orphan {}: {}",
                    o.display(),
                    e
                ));
            }
        } else {
            log::debug!("removed orphan {}", o.display());
            removed += 1;
        }
    }

    // Bottom-up empty-directory sweep within the screens tree.
    let mut dirs: Vec<PathBuf> = WalkDir::new(&screens_root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_dir())
        .map(|e| e.into_path())
        .collect();
    // Deepest first so we prune leaves before their parents.
    dirs.sort_by_key(|d| std::cmp::Reverse(d.as_os_str().len()));
    for d in dirs {
        if d == screens_root {
            continue;
        }
        if is_empty_dir(&d) {
            let _ = std::fs::remove_dir(&d);
        }
    }
    Ok(removed)
}

fn is_empty_dir(p: &Path) -> bool {
    match std::fs::read_dir(p) {
        Ok(mut it) => it.next().is_none(),
        Err(_) => false,
    }
}
