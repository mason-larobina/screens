# `screens` — Video Screenlist Sheet Generator

`screens` scans a directory tree of video files and produces, for each video, a single JPEG "screenlist" (contact sheet) image: several sampled frames arranged in a grid, topped with a multi-line text header. Black canvas, white text. The input directory structure is mirrored into a `Screens/` sibling subtree, and orphaned sheets left over from deleted sources can be removed automatically.

For every video:

```
ROOT/a/b/video.mkv   ->   ROOT/Screens/a/b/video.mkv.jpg
```

## What it does

- **One JPEG per input video**, sampled across the duration (skipping the start/end, which are typically black frames).
- **Frame count is duration-driven** — longer videos get more thumbnails, laid out in a grid shaped per video to stay roughly square.
- **Thumbnails are aspect-preserving** and target a fixed megapixel (pixel area) budget, so a portrait clip's sheet can't out-size a landscape clip's.
- **4-line header** on every sheet: filename, size + duration, video codec/resolution/bitrate/fps, and audio codec/channels/sample rate. The audio line is always rendered (`Audio: —` when there is no audio stream).
- **Mirrors the input tree** under `<ROOT>/<screens-dir>/`, preserving the full source filename (extension included) plus `.jpg` so sheets trace back to their exact source and never collide across formats.
- **Orphan cleanup** removes sheets whose source video no longer exists, scoped strictly to the screens tree (source files are never touched).
- **Skips already-generated sheets** by default — re-runs only sheet new or umped sources; pass `--force` to regenerate everything. See [Skip existing sheets](#skip-existing-sheets).
- **Fail-fast on corrupt input**: a single irrecoverable error aborts the run so you notice and can fix/re-run.
- **End-of-run statistics**: prints a plain-text report on stdout, grouping and counting every video under root by extension, resolution, video codec, audio codec, duration, and bitrate.

## Who it's for

Anyone with a library of video files who wants quick visual previews (contact sheets) of their contents — for browsing, cataloguing, or verifying that files are intact.

## Requirements

- **`ffmpeg`** and **`ffprobe`** on `PATH`.
- A **system-installed monospace font** for the header. The default is `Noto Sans Mono` (e.g. the `fonts-noto-mono` / `google-noto-sans-mono-fonts` package, depending on distro).
- **`fc-match`** (fontconfig) on `PATH`, only required when `--font` is a font family name rather than a direct file path.
- A Rust toolchain (to build from source).

## Install

Install the latest published version from crates.io:

```
cargo install screens
```

Or build and install from a local checkout:

```
cargo install --path=.
```

## Usage

```
Generate JPEG screenlist (contact sheet) images for a directory tree of video files

Usage: screens [OPTIONS] <ROOT>

Arguments:
  <ROOT>  Input directory tree to scan (required, must be a directory)

Options:
      --screens-dir <SCREENS_DIR>  Name of the mirrored output subtree, created directly under ROOT [default: Screens]
      --thumb-mp <THUMB_MP>        Target thumbnail area in megapixels (1 MP = 1_000_000 px). The thumbnail L×H is derived per video from the source aspect ratio so the area is constant across orientations [default: 0.3]
      --font-size <FONT_SIZE>      Header font size in pixels [default: 22]
      --font <FONT>                Font for the header text. Either a path to a font file (TTF/OTF/TTC) or a fontconfig family name resolved at runtime via `fc-match`. The font is **not** bundled with the binary, so it must be installed on the system; the default `"Noto Sans Mono"` requires that family to be present (e.g. the `fonts-noto-mono` / `google-noto-sans-mono-fonts` package, depending on distro). The header layout assumes a monospace font; a proportional font will still render but the column width used for word-wrapping is computed from a single glyph [default: "Noto Sans Mono"]
      --header-pad <HEADER_PAD>    Header padding in pixels [default: 8]
      --gap <GAP>                  Thin black gap between thumbnails, in pixels [default: 4]
      --margin <MARGIN>            Outer margin in pixels [default: 0]
      --quality <QUALITY>          JPEG quality (1..100) [default: 85]
      --jobs <JOBS>                Worker parallelism (default = number of CPUs)
      --no-orphan-cleanup          Disable automatic orphan removal; orphans are still reported on stderr
      --force                      Regenerate sheets that already exist. By default a video whose sheet already exists on disk is skipped (and counted), so re-runs only do work for new or changed sources. Pass `--force` to overwrite existing sheets unconditionally
      --video-exts <VIDEO_EXTS>    Video extensions (case-insensitive, no leading dot) eligible for sheet generation. Comma-separated. Files whose extension is **not** in this set are skipped and reported (grouped by extension) so a missed video type can be spotted and added here [default: mp4,m4v,mkv,mov,avi,wmv,flv,f4v,webm,mpg,mpeg,ts,m2ts,vob,3gp,ogv]
  -h, --help                       Print help
  -V, --version                    Print version
```

`<ROOT>` is the input directory tree to scan. Sheets are written under `<ROOT>/<screens-dir>/...`, mirroring the source file and subdirectory structure, and orphaned sheets from previous runs are removed (see [Orphan cleanup](#orphan-cleanup)).

Example:

```
screens ~/Videos
```

### Skip existing sheets

By default a video whose sheet already exists on disk is **skipped** — probed for nothing, regenerated not at all — so re-running `screens` over a tree only does work for new (or never-sheeted) sources. Skipped videos are counted and reported on a final `skipped N video(s) with existing sheet(s)` line; per-video skips also log the source and the existing sheet path.

Pass `--force` to overwrite existing sheets unconditionally:

```
screens --force ~/Videos
```

Skip checks the mirrored sheet path (`<ROOT>/<screens-dir>/.../<file>.jpg`) for existence only — it does not compare timestamps or contents, so a changed source is not detected. To refresh a single video's sheet, delete its sheet (or run with `--force`) and re-run.

### Statistics

At the end of a run, `screens` prints a plain-text report on stdout, grouping and counting **every video under root** by extension, resolution, video codec, and audio codec.

```
Statistics for 4 video(s)

By extension:
  extension  count
  mp4            2
  mkv            1
  webm           1

By resolution:
  resolution  count
  1280x720        1
  1920x1080       1
  320x240         1
  640x480         1

By video codec:
  video codec  count
  h264             2
  mpeg4            1
  vp8              1

By audio codec:
  audio codec  count
  aac              3
  vorbis           1

By duration:
  duration    count
  <1 min          2
  4-8 min         1
  64-128 min      1

By bitrate:
  bitrate   count
  <1 Mbps       2
  1-2 Mbps      2
```

### Orphan cleanup

On by default, scoped to the screens tree only. After generation, any sheet whose source video no longer exists is reported and deleted (along with any now-empty subdirectories it leaves behind). Pass `--no-orphan-cleanup` to report without deleting. Source files are never touched.

### Logging

`RUST_LOG` controls verbosity (defaults to `info`, so progress is visible without extra flags). Use `RUST_LOG=debug` for per-video detail or `RUST_LOG=warn` for a quieter run.

## Exit codes

- `0` — Every video produced a sheet and the orphan sweep completed cleanly.
- `1` — A failure during the run — a corrupt video (ffprobe fails, a frame extraction fails, an image write fails) or an orphan-cleanup error. Printed as `error: <path>: <reason>`; remaining queued videos are not processed.
- `2` — A hard pre-work error: missing/invalid `<ROOT>`, `<ROOT>` not a directory, `ffmpeg`/`ffprobe` not on `PATH`, invalid flag values, or an unresolvable font.

## Project layout

```
src/
  main.rs     clap CLI, flag validation, font resolution, exit codes, logging
  probe.rs    single ffprobe -of json call; normalized metadata struct; per-video probe cache shared by sheet generation and stats
  frames.rs   frame-count formula, grid layout, offset sampling, ffmpeg extraction
  sheet.rs    header rendering, grid compositing, JPEG write, formatting helpers
  text.rs     font loading + word-wrap
  paths.rs    video collection, skipped-file reporting, path mirroring, orphan cleanup
  stats.rs    end-of-run library statistics: group/count videos by extension, resolution, video/audio codec, duration, and bitrate; plain-text columnar report on stdout
  worker.rs   per-video pipeline + parallel worker pool
```
