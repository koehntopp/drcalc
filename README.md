# drcalc

Dynamic Range (DR) meter in Rust. A port of [drmeter](https://codeberg.org/hausgeist/drmeter) that scans directories recursively, prints txt or JSON reports, and can write the DR values into the files as tags.

Results match drmeter: on a test set of 287 FLAC tracks from 14 albums, every track DR and album DR was identical, and the unrounded scores agreed to two decimals. It runs about 4x faster than drmeter on a small album, and analyses files in parallel.

## Install

Prebuilt binaries for Linux (x86_64, arm64; static), macOS (arm64, x86_64) and Windows (x86_64) are on the [releases page](https://github.com/koehntopp/drcalc/releases). On macOS, downloaded binaries are quarantined; clear that with `xattr -d com.apple.quarantine drcalc`.

From source:

```sh
cargo install --git https://github.com/koehntopp/drcalc
```

## Usage

```sh
drcalc ~/Music/Album                         # txt report, one per album
drcalc -f json ~/Music/Album                 # JSON report
drcalc -w -q ~/Music                         # write tags only, no report
drcalc -w -a overall ~/Music/MultiDiscAlbum  # one album value for everything under the path
```

| Option | Meaning |
|---|---|
| `-f, --format txt\|json` | Report format on stdout (default `txt`) |
| `-a, --album folder\|overall` | How files are grouped into albums (default `folder`) |
| `-w, --write-tags` | Write `DYNAMIC_RANGE` and `ALBUM_DR` into the files |
| `-q, --quiet` | No report |

Supported files: FLAC, MP3, M4A/MP4 (AAC, ALAC), Ogg Vorbis, WAV. Opus and AIFF are not supported.

The exit code is 1 if any file could not be analysed or tagged. Files shorter than 12 seconds cannot be measured.

## Albums

- `folder` (default): every directory with audio files is one album. A `Disc N` or `CD N` folder counts as part of its parent directory, so a multi-disc album in `Album/Disc 1`, `Album/Disc 2` gets one album value.
- `overall`: all files below each path given on the command line form one album.

The album DR is the mean of the rounded track DRs, rounded to the nearest whole number (ties to even, as Python's `round`). Tracks with DR 0 (for example silence) are left out.

## Tags

| Format | Track | Album |
|---|---|---|
| FLAC, Ogg | Vorbis comment `DYNAMIC_RANGE` | `ALBUM_DR` |
| MP3, WAV | ID3v2 `TXXX:DYNAMIC_RANGE` | `TXXX:ALBUM_DR` |
| M4A | `----:com.apple.iTunes:DYNAMIC_RANGE` | `----:com.apple.iTunes:ALBUM_DR` |

Values are two-digit, zero-padded (`09`, `12`). A value of 0 is never written. Other tags, ReplayGain values and pictures are kept. Files whose tags already hold the right values are not rewritten. ID3v2.3 tags stay at v2.3.

The spaced foobar2000 keys `DYNAMIC RANGE` and `ALBUM DYNAMIC RANGE` are neither read nor written.

## JSON output

A list of albums, each with its tracks:

```json
[
  {
    "path": "/music/Album",
    "dr": 11,
    "overall_dr_score": 11.25,
    "overall_peak_db": 0.0,
    "overall_rms_db": -17.66,
    "tracks": [
      {
        "filename": "/music/Album/Disc 1/01.flac",
        "dr": 12,
        "overall_dr_score": 11.72,
        "overall_peak_db": -1.34,
        "overall_rms_db": -16.06,
        "dr_score": [12.02, 11.41],
        "peak_db": [-1.34, -2.35],
        "rms_db": [-16.07, -16.05],
        "duration_seconds": 234.68,
        "samplerate": 44100,
        "channels": 2,
        "bitdepth": 24,
        "codec": "flac"
      }
    ]
  }
]
```

Per-channel values are in `dr_score`, `peak_db` and `rms_db`. Silent channels have `null` for peak and RMS. The album's `overall_dr_score` is the unrounded mean of the track scores. It is for information only; `dr` is the value that gets tagged.

## Algorithm

This is the same as drmeter:

1. Split each channel into 3-second blocks (the last block may be shorter).
2. For each block, take the RMS (scaled by √2) and the peak.
3. Take the RMS of the loudest 20% of blocks, and the second-highest block peak.
4. The channel DR is the ratio of that peak to that RMS, in dB. The track DR is the mean over the channels.

## License

MIT. Based on drmeter by Jan Willhaus.
