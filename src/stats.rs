//! End-of-run library statistics: group and count every video under root by
//! extension, resolution, video codec, audio codec, duration, and bitrate,
//! then render the counts as plain-text columnar tables.
//!
//! Covers *every* video under root, including those skipped because their
//! sheet already existed — each such video is still ffprobed once (purely
//! for stats) so the report reflects the whole library, not just the sheets
//! written this run. That single probe result is shared with sheet
//! generation via [`crate::probe::ProbeCache`], so ffprobe runs at most once
//! per video across the whole run.
//!
//! Sentinels: a missing audio stream groups under `"(none)"`; any value
//! ffprobe could not report (unknown codec, zero resolution, no bitrate)
//! groups under `"(unknown)"`. These read clearly in a table and never
//! collide with a real codec or container name.
//!
//! Ordering: the discrete-key tables (extension, resolution, video/audio
//! codec) are sorted by count descending then key ascending — the natural
//! "biggest bucket first" scan. The duration and bitrate tables use
//! **fixed magnitude (size) ordering** instead: their buckets are defined in
//! ascending order and rendered that way, so a reader scans short→long and
//! low→high bitrate top-to-bottom. Empty buckets are omitted (consistent
//! with the discrete tables, which only ever hold present keys).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::probe::{ProbeCache, ProbeMeta};

/// Sentinel label for an absent value (e.g. no audio stream).
const NONE: &str = "(none)";
/// Sentinel label for a value ffprobe could not report (unknown codec,
/// zero resolution, no bitrate).
const UNKNOWN: &str = "(unknown)";

/// A magnitude bucket for a continuous value (duration in minutes, or
/// bitrate in Mbps). `upper` is the exclusive upper bound, in the same unit
/// the caller passes to [`bucket_for`] (minutes for duration, Mbps for
/// bitrate); the last bucket uses [`f64::INFINITY`] as a catch-all. Buckets
/// are defined in ascending `upper` order so iterating them yields ascending
/// magnitude — the order the duration/bitrate tables are rendered in.
struct Bucket {
    label: &'static str,
    upper: f64,
}

/// Duration buckets, in minutes. Power-of-two boundaries:
/// `<1`, `1-2`, `2-4`, `4-8`, `8-16`, `16-32`, `32-64`, `64-128`,
/// `128-256`, `256-512`, `512-1024`, `1024+`. Boundaries double each step,
/// so the buckets stay roughly logarithmic and each spans twice the range of
/// the one below; empty buckets are omitted at render time, so the table only
/// shows the magnitudes that actually occur.
const DURATION_BUCKETS: [Bucket; 12] = [
    Bucket {
        label: "<1 min",
        upper: 1.0,
    },
    Bucket {
        label: "1-2 min",
        upper: 2.0,
    },
    Bucket {
        label: "2-4 min",
        upper: 4.0,
    },
    Bucket {
        label: "4-8 min",
        upper: 8.0,
    },
    Bucket {
        label: "8-16 min",
        upper: 16.0,
    },
    Bucket {
        label: "16-32 min",
        upper: 32.0,
    },
    Bucket {
        label: "32-64 min",
        upper: 64.0,
    },
    Bucket {
        label: "64-128 min",
        upper: 128.0,
    },
    Bucket {
        label: "128-256 min",
        upper: 256.0,
    },
    Bucket {
        label: "256-512 min",
        upper: 512.0,
    },
    Bucket {
        label: "512-1024 min",
        upper: 1024.0,
    },
    Bucket {
        label: "1024+ min",
        upper: f64::INFINITY,
    },
];

/// Bitrate buckets, in Mbps. Power-of-two boundaries, mirroring
/// [`DURATION_BUCKETS`]: `<1`, `1-2`, `2-4`, `4-8`, `8-16`, `16-32`,
/// `32-64`, `64-128`, `128-256`, `256-512`, `512-1024`, `1024+`. Boundaries
/// double each step, so the buckets stay roughly logarithmic and each spans
/// twice the range of the one below; empty buckets are omitted at render
/// time, so the table only shows the magnitudes that actually occur (a real
/// library rarely climbs past the low hundreds of Mbps). Bitrate uses the
/// same resolution as the sheet header — container `format.bit_rate`,
/// falling back to the video stream's `bit_rate`; expressed in decimal Mbps
/// (`/1_000_000`) to match the sheet's `kb/s` (`/1000`), not binary. Videos
/// reporting no bitrate at all count under a trailing `"(unknown)"` bucket.
const BITRATE_BUCKETS: [Bucket; 12] = [
    Bucket {
        label: "<1 Mbps",
        upper: 1.0,
    },
    Bucket {
        label: "1-2 Mbps",
        upper: 2.0,
    },
    Bucket {
        label: "2-4 Mbps",
        upper: 4.0,
    },
    Bucket {
        label: "4-8 Mbps",
        upper: 8.0,
    },
    Bucket {
        label: "8-16 Mbps",
        upper: 16.0,
    },
    Bucket {
        label: "16-32 Mbps",
        upper: 32.0,
    },
    Bucket {
        label: "32-64 Mbps",
        upper: 64.0,
    },
    Bucket {
        label: "64-128 Mbps",
        upper: 128.0,
    },
    Bucket {
        label: "128-256 Mbps",
        upper: 256.0,
    },
    Bucket {
        label: "256-512 Mbps",
        upper: 512.0,
    },
    Bucket {
        label: "512-1024 Mbps",
        upper: 1024.0,
    },
    Bucket {
        label: "1024+ Mbps",
        upper: f64::INFINITY,
    },
];

/// One video's contribution to the statistics, after normalizing ffprobe
/// output into the grouping dimensions. The discrete-key fields are the
/// string used as a bucket key in the report; the duration/bitrate fields
/// are pre-resolved bucket indices (with `None` for an unreportable
/// bitrate).
#[derive(Debug, Clone)]
struct VideoStat {
    ext: String,
    resolution: String,
    video_codec: String,
    audio_codec: String,
    duration_bucket: usize,
    bitrate_bucket: Option<usize>,
}

impl VideoStat {
    /// Build a [`VideoStat`] from a source path and its probe metadata,
    /// normalizing each dimension into a bucket key with sentinels for
    /// missing values.
    fn from_probe(src: &Path, meta: &ProbeMeta) -> Self {
        let ext = src
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .filter(|e| !e.is_empty())
            .unwrap_or_else(|| UNKNOWN.to_string());

        let resolution = if meta.video.width > 0 && meta.video.height > 0 {
            format!("{}x{}", meta.video.width, meta.video.height)
        } else {
            UNKNOWN.to_string()
        };

        let video_codec = meta
            .video
            .codec
            .clone()
            .filter(|c| !c.is_empty())
            .unwrap_or_else(|| UNKNOWN.to_string());

        let audio_codec = match &meta.audio {
            Some(a) => a
                .codec
                .clone()
                .filter(|c| !c.is_empty())
                .unwrap_or_else(|| UNKNOWN.to_string()),
            None => NONE.to_string(),
        };

        // Duration is always present (probe errors otherwise); bucket by
        // minutes into the fixed ascending magnitude set.
        let duration_bucket = bucket_for(&DURATION_BUCKETS, meta.duration / 60.0);

        // Bitrate matches the sheet header: container bit_rate preferred,
        // falling back to the video stream's. None/zero → unknown bucket.
        let bps = meta.bit_rate.or(meta.video.bit_rate);
        let bitrate_bucket = bps.map(|b| bucket_for(&BITRATE_BUCKETS, b as f64 / 1_000_000.0));

        VideoStat {
            ext,
            resolution,
            video_codec,
            audio_codec,
            duration_bucket,
            bitrate_bucket,
        }
    }
}

/// Aggregated statistics over all probed videos. Discrete dimensions use
/// `BTreeMap` (key-sorted, then count-sorted at render); the magnitude
/// dimensions use `Vec` counters parallel to their fixed bucket arrays so
/// they render in ascending-size order.
#[derive(Default)]
pub struct Stats {
    by_ext: BTreeMap<String, usize>,
    by_resolution: BTreeMap<String, usize>,
    by_video_codec: BTreeMap<String, usize>,
    by_audio_codec: BTreeMap<String, usize>,
    by_duration: Vec<usize>,
    by_bitrate: Vec<usize>,
    bitrate_unknown: usize,
}

impl Stats {
    fn add(&mut self, s: &VideoStat) {
        *self.by_ext.entry(s.ext.clone()).or_insert(0) += 1;
        *self.by_resolution.entry(s.resolution.clone()).or_insert(0) += 1;
        *self
            .by_video_codec
            .entry(s.video_codec.clone())
            .or_insert(0) += 1;
        *self
            .by_audio_codec
            .entry(s.audio_codec.clone())
            .or_insert(0) += 1;

        if self.by_duration.is_empty() {
            self.by_duration = vec![0; DURATION_BUCKETS.len()];
        }
        self.by_duration[s.duration_bucket] += 1;

        match s.bitrate_bucket {
            Some(i) => {
                if self.by_bitrate.is_empty() {
                    self.by_bitrate = vec![0; BITRATE_BUCKETS.len()];
                }
                self.by_bitrate[i] += 1;
            }
            None => self.bitrate_unknown += 1,
        }
    }

    /// Build the report from already-cached probe results — ffprobe is
    /// called once per video during the run (see `worker::run_all`), so this
    /// never probes itself. Every video in `videos` is looked up in `cache`;
    /// a missing entry (which should not happen after a successful run) is
    /// logged at `WARN` and omitted from the stats, keeping the tables
    /// internally consistent with the `Statistics for N video(s)` header.
    pub fn from_cache(videos: &[PathBuf], cache: &ProbeCache) -> Self {
        let mut stats = Stats::default();
        for src in videos {
            match cache.get(src) {
                Some(meta) => stats.add(&VideoStat::from_probe(src, &meta)),
                None => log::warn!(
                    "stats: no probe result for {} (skipping from report)",
                    src.display()
                ),
            }
        }
        stats
    }

    /// Render the report as a plain-text block. Each dimension is a two-column table
    /// (`<value>   count`) with a header row and a fixed gap between the
    /// columns (no separator line). Discrete-key tables are sorted by count
    /// descending then key ascending; the duration and bitrate tables are
    /// rendered in ascending magnitude (size) order. The total video count
    /// appears only in the `Statistics for N video(s)` header line — there is
    /// no per-table total row or column.
    pub fn render(&self) -> String {
        let total: usize = self.by_ext.values().sum();
        let mut out = String::new();
        out.push_str(&format!("Statistics for {total} video(s)\n\n"));

        render_table(&mut out, "extension", &rows_count_desc(&self.by_ext));
        out.push('\n');
        render_table(
            &mut out,
            "resolution",
            &rows_count_desc(&self.by_resolution),
        );
        out.push('\n');
        render_table(
            &mut out,
            "video codec",
            &rows_count_desc(&self.by_video_codec),
        );
        out.push('\n');
        render_table(
            &mut out,
            "audio codec",
            &rows_count_desc(&self.by_audio_codec),
        );
        out.push('\n');
        render_table(
            &mut out,
            "duration",
            &rows_magnitude(&DURATION_BUCKETS, &self.by_duration),
        );
        out.push('\n');

        let mut br_rows = rows_magnitude(&BITRATE_BUCKETS, &self.by_bitrate);
        if self.bitrate_unknown > 0 {
            br_rows.push((UNKNOWN.to_string(), self.bitrate_unknown));
        }
        render_table(&mut out, "bitrate", &br_rows);

        out
    }
}

/// Build display rows for a discrete-key table: sorted by count descending,
/// then key ascending so ties have a stable, alphabetical order.
fn rows_count_desc(m: &BTreeMap<String, usize>) -> Vec<(String, usize)> {
    let mut rows: Vec<(String, usize)> = m.iter().map(|(k, v)| (k.clone(), *v)).collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows
}

/// Build display rows for a magnitude table: the buckets in their fixed
/// ascending order, with empty (zero-count) buckets omitted. Iterating the
/// bucket array preserves the definition order, so the result is in
/// ascending-size order with no explicit sort. `counts` may be shorter than
/// `buckets` (when no video has been added yet); missing entries read as 0
/// and are therefore omitted.
fn rows_magnitude(buckets: &[Bucket], counts: &[usize]) -> Vec<(String, usize)> {
    buckets
        .iter()
        .enumerate()
        .filter_map(|(i, b)| {
            let c = counts.get(i).copied().unwrap_or(0);
            (c > 0).then_some((b.label.to_string(), c))
        })
        .collect()
}

/// Find the index of the bucket whose exclusive upper bound first exceeds
/// `v`, scanning in definition (ascending) order. The final bucket's
/// `upper` is [`f64::INFINITY`], so it always matches.
fn bucket_for(buckets: &[Bucket], v: f64) -> usize {
    for (i, b) in buckets.iter().enumerate() {
        if v < b.upper {
            return i;
        }
    }
    buckets.len() - 1
}

/// Append one two-column table (`<value>   count`) to `out`. The value
/// column is left-aligned, the count column right-aligned, with a header row
/// and a fixed two-space gap between the columns (no separator line). `rows`
/// are already in display order.
fn render_table(out: &mut String, label: &str, rows: &[(String, usize)]) {
    let header_value = label;
    let header_count = "count";

    // Column widths span the header and all rows so everything lines up.
    let vw = header_value
        .len()
        .max(rows.iter().map(|(v, _)| v.len()).max().unwrap_or(0));
    let cw = header_count.len().max(
        rows.iter()
            .map(|(_, c)| c.to_string().len())
            .max()
            .unwrap_or(0),
    );
    const GAP: usize = 2;

    out.push_str(&format!("By {label}:\n"));
    out.push_str(&format!(
        "  {:<vw$}{gap:>GAP$}{:>cw$}\n",
        header_value,
        header_count,
        gap = "",
        vw = vw,
        cw = cw,
        GAP = GAP
    ));
    for (v, c) in rows {
        out.push_str(&format!(
            "  {:<vw$}{gap:>GAP$}{:>cw$}\n",
            v,
            c,
            gap = "",
            vw = vw,
            cw = cw,
            GAP = GAP
        ));
    }
}
