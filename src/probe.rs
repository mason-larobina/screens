//! ffprobe invocation + metadata struct.
//!
//! A single `ffprobe -of json` call per video fetches format + stream
//! metadata. We pick the first video stream for resolution/aspect/fps and the
//! first audio stream for the audio line.

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

#[derive(Debug, Deserialize)]
struct FfprobeOutput {
    format: FfprobeFormat,
    streams: Vec<FfprobeStream>,
}

#[derive(Debug, Deserialize)]
struct FfprobeFormat {
    duration: Option<String>,
    size: Option<String>,
    bit_rate: Option<String>,
    #[allow(dead_code)]
    format_name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct FfprobeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    bit_rate: Option<String>,
    channels: Option<u32>,
    sample_rate: Option<String>,
}

/// Normalized metadata for a video, derived from a single ffprobe call.
#[derive(Debug, Clone)]
pub struct ProbeMeta {
    /// Duration in seconds.
    pub duration: f64,
    /// File size in bytes (format.size, parsed).
    pub size_bytes: u64,
    /// Container-level average bitrate, bits/s (format.bit_rate).
    pub bit_rate: Option<u64>,
    /// First video stream.
    pub video: VideoMeta,
    /// First audio stream, if any.
    pub audio: Option<AudioMeta>,
}

#[derive(Debug, Clone)]
pub struct VideoMeta {
    pub codec: Option<String>,
    pub width: u32,
    pub height: u32,
    /// Average frame rate as a decimal (e.g. 23.976).
    pub fps: Option<f64>,
    /// Stream-level bitrate, bits/s.
    pub bit_rate: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct AudioMeta {
    pub codec: Option<String>,
    pub channels: u32,
    pub sample_rate: u32,
}

/// Invoke `ffprobe -of json` on `path` and parse the result.
pub fn probe(path: &Path) -> Result<ProbeMeta> {
    log::debug!("ffprobe {}", path.display());
    let out = Command::new("ffprobe")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-of")
        .arg("json")
        .arg("-show_format")
        .arg("-show_streams")
        .arg(path)
        .output()
        .context("failed to spawn ffprobe")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(anyhow!("ffprobe failed: {}", stderr.trim()));
    }

    let parsed: FfprobeOutput =
        serde_json::from_slice(&out.stdout).context("failed to parse ffprobe JSON")?;

    let duration = parsed
        .format
        .duration
        .as_deref()
        .and_then(|s| s.parse::<f64>().ok())
        .ok_or_else(|| anyhow!("missing/invalid format.duration"))?;

    let size_bytes = parsed
        .format
        .size
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);

    let bit_rate = parsed
        .format
        .bit_rate
        .as_deref()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&b| b > 0);

    let video = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("video"))
        .ok_or_else(|| anyhow!("no video stream"))?;

    let fps = video
        .r_frame_rate
        .as_deref()
        .or(video.avg_frame_rate.as_deref())
        .and_then(parse_rational);

    let video = VideoMeta {
        codec: video.codec_name.clone(),
        width: video.width.unwrap_or(0),
        height: video.height.unwrap_or(0),
        fps,
        bit_rate: video
            .bit_rate
            .as_deref()
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&b| b > 0),
    };

    let audio = parsed
        .streams
        .iter()
        .find(|s| s.codec_type.as_deref() == Some("audio"))
        .map(|a| AudioMeta {
            codec: a.codec_name.clone(),
            channels: a.channels.unwrap_or(0),
            sample_rate: a
                .sample_rate
                .as_deref()
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(0),
        });

    Ok(ProbeMeta {
        duration,
        size_bytes,
        bit_rate,
        video,
        audio,
    })
}

/// A per-process cache of ffprobe results so each video is probed at most
/// once across the whole run. Sheet generation and the end-of-run statistics
/// both read from it, so a skipped (already-sheeted) video — probed only for
/// stats — and a freshly-processed video share the same single ffprobe call.
///
/// The cache is keyed by source path and stores cloned [`ProbeMeta`] (small:
/// a few strings + numbers). `get_or_probe` probes on miss and inserts the
/// result; callers receive a clone. Probing happens *outside* the lock so
/// workers probe in parallel, and each source appears at most once in the
/// work set, so no two workers ever probe the same path.
#[derive(Clone)]
pub struct ProbeCache {
    inner: Arc<Mutex<HashMap<PathBuf, ProbeMeta>>>,
}

impl ProbeCache {
    /// Build an empty cache.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(HashMap::new().into()),
        }
    }

    /// Return the cached metadata for `src`, probing (once) and inserting on
    /// a miss. A probe error is propagated so the caller's fail-fast
    /// (`collect::<Result<_>>`) contract aborts the run on the first corrupt
    /// video.
    pub fn get_or_probe(&self, src: &Path) -> Result<ProbeMeta> {
        if let Some(m) = self.inner.lock().unwrap().get(src).cloned() {
            return Ok(m);
        }
        let m = probe(src)?;
        self.inner
            .lock()
            .unwrap()
            .insert(src.to_path_buf(), m.clone());
        Ok(m)
    }

    /// Return the cached metadata for `src` if present, without probing.
    /// Used by the stats pass, which runs only after a fully-successful run
    /// has populated the cache for every video.
    pub fn get(&self, src: &Path) -> Option<ProbeMeta> {
        self.inner.lock().unwrap().get(src).cloned()
    }
}

impl Default for ProbeCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse an ffmpeg rational like "24000/1001" into a decimal f64.
fn parse_rational(s: &str) -> Option<f64> {
    let (num, den) = s.split_once('/')?;
    let num: f64 = num.parse().ok()?;
    let den: f64 = den.parse().ok()?;
    if den == 0.0 {
        return None;
    }
    Some(num / den)
}
