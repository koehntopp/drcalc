//! Dynamic Range (DR) meter. Port of https://codeberg.org/hausgeist/drmeter
//! with recursive directory scanning, album grouping and tag writing.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use rayon::prelude::*;
use serde::Serialize;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

const BLOCK_SECONDS: f64 = 3.0;
const UPMOST_BLOCKS_RATIO: f64 = 0.2;
const MIN_BLOCKS: usize = 4; // original: MIN_BLOCK_COUNT = 1 // 0.2, which Python evaluates to 4.0
const EXTENSIONS: &[&str] = &["flac", "mp3", "m4a", "mp4", "ogg", "oga", "wav"];
// Tagwright keys (ADR 0027); the spaced foobar keys are no longer read.
const TAG_TRACK: &str = "DYNAMIC_RANGE";
const TAG_ALBUM: &str = "ALBUM_DR";

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum Format {
    Txt,
    Json,
}

#[derive(Copy, Clone, PartialEq, Eq, ValueEnum)]
enum AlbumScope {
    /// Every directory containing audio files is one album; "Disc N"/"CD N" folders
    /// count as part of their parent (as in the discogs calculate_dr.py)
    Folder,
    /// All files below each given path form one album (e.g. multi-disc)
    Overall,
}

/// Dynamic Range (DR) analyzer for audio files.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Files or directories (scanned recursively)
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Report format written to stdout
    #[arg(short, long, value_enum, default_value_t = Format::Txt)]
    format: Format,

    /// How files are grouped for the album DR value
    #[arg(short, long, value_enum, default_value_t = AlbumScope::Folder)]
    album: AlbumScope,

    /// Write DR tags (DYNAMIC_RANGE, ALBUM_DR) into the files
    #[arg(short, long)]
    write_tags: bool,

    /// Do not print a report (useful with --write-tags)
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Serialize)]
struct Track {
    filename: PathBuf,
    dr: i64,
    overall_dr_score: f64,
    overall_peak_db: Option<f64>,
    overall_rms_db: Option<f64>,
    dr_score: Vec<f64>,
    peak_db: Vec<Option<f64>>,
    rms_db: Vec<Option<f64>>,
    duration_seconds: f64,
    samplerate: u32,
    channels: usize,
    bitdepth: Option<u32>,
    codec: String,
    #[serde(skip)]
    peak: f64,
    #[serde(skip)]
    rms: f64,
}

#[derive(Serialize)]
struct Album {
    path: PathBuf,
    dr: i64,
    overall_dr_score: f64,
    overall_peak_db: Option<f64>,
    overall_rms_db: Option<f64>,
    tracks: Vec<Track>,
}

fn round_ties_even(x: f64) -> i64 {
    x.round_ties_even() as i64 // Python's round() and "%.0f" semantics
}

fn db(x: f64) -> f64 {
    20.0 * x.log10()
}

/// None for -inf, matching the original's JSON `null`; 2 decimals.
fn json_db(x: f64) -> Option<f64> {
    let v = db(x);
    v.is_finite().then(|| (v * 100.0).round() / 100.0)
}

fn r2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

struct Blocks {
    channels: usize,
    block_len: usize,
    filled: usize,
    sumsq: Vec<f64>,
    peak: Vec<f64>,
    rms: Vec<Vec<f64>>,   // per channel
    peaks: Vec<Vec<f64>>, // per channel
}

impl Blocks {
    fn new(channels: usize, samplerate: u32) -> Self {
        Self {
            channels,
            block_len: (BLOCK_SECONDS * samplerate as f64).round() as usize,
            filled: 0,
            sumsq: vec![0.0; channels],
            peak: vec![0.0; channels],
            rms: vec![Vec::new(); channels],
            peaks: vec![Vec::new(); channels],
        }
    }

    fn push(&mut self, interleaved: &[f32]) {
        for frame in interleaved.chunks_exact(self.channels) {
            for (c, &s) in frame.iter().enumerate() {
                let s = s as f64;
                self.sumsq[c] += s * s;
                self.peak[c] = self.peak[c].max(s.abs());
            }
            self.filled += 1;
            if self.filled == self.block_len {
                self.flush();
            }
        }
    }

    fn flush(&mut self) {
        if self.filled == 0 {
            return;
        }
        for c in 0..self.channels {
            self.rms[c].push((2.0 * self.sumsq[c] / self.filled as f64).sqrt());
            self.peaks[c].push(self.peak[c]);
            self.sumsq[c] = 0.0;
            self.peak[c] = 0.0;
        }
        self.filled = 0;
    }
}

/// Per channel: (dr, peak pressure, rms pressure)
fn dynamic_range(mut b: Blocks) -> Result<Vec<(f64, f64, f64)>> {
    b.flush();
    let total = b.rms[0].len();
    if total < MIN_BLOCKS {
        bail!("file shorter than {} seconds", MIN_BLOCKS as f64 * BLOCK_SECONDS);
    }
    let upmost = round_ties_even(total as f64 * UPMOST_BLOCKS_RATIO) as usize;
    Ok((0..b.channels)
        .map(|c| {
            let rms = &mut b.rms[c];
            let peaks = &mut b.peaks[c];
            rms.sort_by(f64::total_cmp);
            peaks.sort_by(f64::total_cmp);
            let rms_pressure = (rms.iter().map(|r| r * r).sum::<f64>() / total as f64).sqrt();
            let top = (rms[total - upmost..].iter().map(|r| r * r).sum::<f64>() / upmost as f64).sqrt();
            let dr = if top > 0.0 { db(peaks[total - 2] / top) } else { 0.0 };
            (dr, peaks[total - 1], rms_pressure)
        })
        .collect())
}

fn analyze(path: &Path) -> Result<Track> {
    let mss = MediaSourceStream::new(Box::new(File::open(path)?), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let fmt_opts = FormatOptions { enable_gapless: true, ..Default::default() };
    let mut format = symphonia::default::get_probe()
        .format(&hint, mss, &fmt_opts, &MetadataOptions::default())?
        .format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow!("no audio track"))?;
    let track_id = track.id;
    let params = track.codec_params.clone();
    let mut decoder = symphonia::default::get_codecs().make(&params, &DecoderOptions::default())?;
    let codec = symphonia::default::get_codecs()
        .get_codec(params.codec)
        .map_or("unknown", |d| d.short_name)
        .to_string();

    let mut blocks: Option<Blocks> = None;
    let mut buf: Option<SampleBuffer<f32>> = None;
    let (mut samplerate, mut frames) = (0u32, 0u64);
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(SymError::DecodeError(_)) => continue, // skip corrupt packet, like other decoders
            Err(e) => return Err(e.into()),
        };
        let spec = *decoded.spec();
        if buf.as_ref().is_none_or(|b| b.capacity() < decoded.capacity()) {
            buf = Some(SampleBuffer::new(decoded.capacity() as u64, spec));
        }
        let b = buf.as_mut().unwrap();
        b.copy_interleaved_ref(decoded);
        let blk = blocks.get_or_insert_with(|| {
            samplerate = spec.rate;
            Blocks::new(spec.channels.count(), spec.rate)
        });
        frames += (b.samples().len() / blk.channels) as u64;
        blk.push(b.samples());
    }
    let blk = blocks.ok_or_else(|| anyhow!("no audio decoded"))?;
    let channels = blk.channels;
    let per_ch = dynamic_range(blk)?;

    let n = channels as f64;
    let overall = per_ch.iter().map(|c| c.0).sum::<f64>() / n;
    let peak = per_ch.iter().map(|c| c.1).fold(0.0, f64::max);
    let rms = per_ch.iter().map(|c| c.2).sum::<f64>() / n;
    Ok(Track {
        filename: path.to_path_buf(),
        dr: round_ties_even(overall),
        overall_dr_score: r2(overall),
        overall_peak_db: json_db(peak),
        overall_rms_db: json_db(rms),
        dr_score: per_ch.iter().map(|c| r2(c.0)).collect(),
        peak_db: per_ch.iter().map(|c| json_db(c.1)).collect(),
        rms_db: per_ch.iter().map(|c| json_db(c.2)).collect(),
        duration_seconds: r2(frames as f64 / samplerate as f64),
        samplerate,
        channels,
        bitdepth: params.bits_per_sample,
        codec,
        peak,
        rms,
    })
}

fn album(path: PathBuf, mut tracks: Vec<Track>) -> Album {
    tracks.sort_by(|a, b| a.filename.cmp(&b.filename));
    let n = tracks.len() as f64;
    // As calculate_dr.py: mean of the rounded track values, tracks with DR 0 left out.
    let rated: Vec<i64> = tracks.iter().map(|t| t.dr).filter(|&d| d > 0).collect();
    let dr = if rated.is_empty() { 0.0 } else { rated.iter().sum::<i64>() as f64 / rated.len() as f64 };
    let peak = tracks.iter().map(|t| t.peak).fold(0.0, f64::max);
    let rms = tracks.iter().map(|t| t.rms).sum::<f64>() / n;
    Album {
        path,
        dr: round_ties_even(dr),
        overall_dr_score: r2(dr),
        overall_peak_db: json_db(peak),
        overall_rms_db: json_db(rms),
        tracks,
    }
}

/// Album directory of a file's folder, looking through a "Disc N" / "CD N" folder.
fn album_root(dir: &Path) -> PathBuf {
    let is_disc = dir.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        let l = n.to_ascii_lowercase();
        let rest = l.strip_prefix("disc").or_else(|| l.strip_prefix("cd")).unwrap_or("").trim_start();
        !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())
    });
    match dir.parent() {
        Some(parent) if is_disc => parent.to_path_buf(),
        _ => dir.to_path_buf(),
    }
}

fn is_audio(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Returns (album key, file) pairs according to the grouping scope.
fn collect(paths: &[PathBuf], scope: AlbumScope) -> Vec<(PathBuf, PathBuf)> {
    let mut out = Vec::new();
    for root in paths {
        if root.is_file() {
            out.push((album_root(root.parent().unwrap_or(Path::new("."))), root.clone()));
            continue;
        }
        for entry in walkdir::WalkDir::new(root).sort_by_file_name().into_iter().filter_map(|e| {
            e.map_err(|err| eprintln!("warning: {err}")).ok()
        }) {
            let p = entry.path();
            if entry.file_type().is_file() && is_audio(p) && !p.file_name().unwrap().to_string_lossy().starts_with("._") {
                let key = match scope {
                    AlbumScope::Folder => album_root(p.parent().unwrap()),
                    AlbumScope::Overall => root.clone(),
                };
                out.push((key, p.to_path_buf()));
            }
        }
    }
    out
}

fn fmt_duration(secs: f64) -> String {
    let s = secs.round() as u64;
    let full = format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
    full.trim_start_matches(['0', ':']).to_string()
}

fn summarize<T: ToString>(values: impl Iterator<Item = T>, unit: &str) -> String {
    let mut counts: Vec<(String, usize)> = Vec::new();
    for v in values.map(|v| v.to_string()) {
        match counts.iter_mut().find(|(k, _)| *k == v) {
            Some((_, n)) => *n += 1,
            None => counts.push((v, 1)),
        }
    }
    let unit = if unit.is_empty() { String::new() } else { format!(" {unit}") };
    if counts.len() == 1 {
        return format!("{}{unit}", counts[0].0);
    }
    counts.iter().map(|(v, n)| format!("{v}{unit} ({n}x)")).collect::<Vec<_>>().join(", ")
}

fn fmt_db(x: Option<f64>) -> String {
    x.map_or("-inf".into(), |v| format!("{v:.2}"))
}

fn to_txt(a: &Album) -> String {
    let rows: Vec<String> = a
        .tracks
        .iter()
        .map(|t| {
            let name = t.filename.file_name().unwrap().to_string_lossy();
            format!(
                "DR{:<3} {:>9} dBFS {:>9} dBFS {:>9}  {name}",
                t.dr,
                fmt_db(t.overall_peak_db),
                fmt_db(t.overall_rms_db),
                fmt_duration(t.duration_seconds)
            )
        })
        .collect();
    let div = "-".repeat(rows.iter().map(|r| r.chars().count()).max().unwrap_or(0).min(120));
    let source = std::fs::canonicalize(&a.path).unwrap_or(a.path.clone());
    let source = source.file_name().map_or(source.to_string_lossy(), |n| n.to_string_lossy());
    let ts = a.tracks.iter();
    format!(
        "drcalc {ver}\n\n{div}\nAnalyzed: {source}\n{div}\n\n\
         DR         Peak            RMS       Duration  Track\n{div}\n{rows}\n{div}\n\n\
         Number of tracks:  {n}\nOfficial DR value: DR{dr}\n\n\
         Samplerate:        {sr}\nChannels:          {ch}\nBits per sample:   {bits}\nCodec:             {codec}\n{eq}\n",
        ver = env!("CARGO_PKG_VERSION"),
        rows = rows.join("\n"),
        n = a.tracks.len(),
        dr = a.dr,
        sr = summarize(ts.clone().map(|t| t.samplerate), "Hz"),
        ch = summarize(ts.clone().map(|t| t.channels), ""),
        bits = summarize(ts.clone().map(|t| t.bitdepth.map_or("n/a".into(), |b| b.to_string())), ""),
        codec = summarize(ts.clone().map(|t| t.codec.clone()), ""),
        eq = "=".repeat(div.len()),
    )
}

fn write_tags(path: &Path, track_dr: i64, album_dr: i64) -> Result<()> {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile;
    use lofty::id3::v2::{Id3v2Tag, Id3v2Version};
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst};
    use lofty::ogg::tag::VorbisComments;

    // Zero-padded like calculate_dr.py ("09"); a 0 value is never written there either.
    let vals: Vec<(&str, String)> = [(TAG_TRACK, track_dr), (TAG_ALBUM, album_dr)]
        .into_iter()
        .filter(|&(_, v)| v > 0)
        .map(|(k, v)| (k, format!("{v:02}")))
        .collect();
    // Each setter returns whether anything changed; unchanged files are not rewritten
    // (as calculate_dr.py, which saves only on a new value).
    let set_vc = |t: &mut VorbisComments| {
        let mut changed = false;
        for (k, v) in &vals {
            // Duplicate keys (e.g. a stale second ALBUM_DR) count as a change too.
            if !t.get_all(k).eq([v.as_str()]) {
                t.insert(k.to_string(), v.clone());
                changed = true;
            }
        }
        changed
    };
    let set_id3 = |t: &mut Id3v2Tag| {
        let mut changed = false;
        for (k, v) in &vals {
            if t.get_user_text(k) != Some(v.as_str()) {
                t.insert_user_text(k.to_string(), v.clone());
                changed = true;
            }
        }
        changed
    };
    let set_ilst = |t: &mut Ilst| {
        let mut changed = false;
        for (k, v) in &vals {
            let ident = AtomIdent::Freeform { mean: "com.apple.iTunes".into(), name: k.to_string().into() };
            let data = AtomData::UTF8(v.clone());
            if t.get(&ident).and_then(|a| a.data().next()) != Some(&data) {
                t.remove(&ident).for_each(drop);
                t.insert(Atom::new(ident, data));
                changed = true;
            }
        }
        changed
    };

    let mut f = OpenOptions::new().read(true).write(true).open(path)?;
    let po = ParseOptions::new();
    let mut wo = WriteOptions::default();
    // Keep ID3v2.3 files at v2.3 instead of silently upgrading them to v2.4.
    let mut keep_v23 = |t: &Id3v2Tag| {
        if t.original_version() == Id3v2Version::V3 {
            wo = WriteOptions::default().use_id3v23(true);
        }
    };

    macro_rules! opt_tag {
        ($file:ty, $get:ident, $set:ident, $tag:ty, $apply:expr) => {{
            let mut file = <$file>::read_from(&mut f, po)?;
            if file.$get().is_none() {
                file.$set(<$tag>::default());
            }
            let changed = $apply(file.$get().unwrap());
            (file, changed)
        }};
    }
    macro_rules! save {
        ($file:expr, $changed:expr) => {
            if $changed {
                f.rewind()?;
                $file.save_to(&mut f, wo)?;
            }
        };
    }

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "flac" => {
            let (file, changed) = opt_tag!(lofty::flac::FlacFile, vorbis_comments_mut, set_vorbis_comments, VorbisComments, set_vc);
            save!(file, changed);
        }
        "mp3" => {
            let (file, changed) = opt_tag!(lofty::mpeg::MpegFile, id3v2_mut, set_id3v2, Id3v2Tag, |t: &mut Id3v2Tag| {
                keep_v23(t);
                set_id3(t)
            });
            save!(file, changed);
        }
        "wav" => {
            let (file, changed) = opt_tag!(lofty::iff::wav::WavFile, id3v2_mut, set_id3v2, Id3v2Tag, |t: &mut Id3v2Tag| {
                keep_v23(t);
                set_id3(t)
            });
            save!(file, changed);
        }
        "m4a" | "mp4" => {
            let (file, changed) = opt_tag!(lofty::mp4::Mp4File, ilst_mut, set_ilst, Ilst, set_ilst);
            save!(file, changed);
        }
        "ogg" | "oga" => {
            let mut file = lofty::ogg::VorbisFile::read_from(&mut f, po)?;
            let changed = set_vc(file.vorbis_comments_mut());
            save!(file, changed);
        }
        _ => bail!("tag writing not supported for .{ext}"),
    }
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let files = collect(&cli.paths, cli.album);
    if files.is_empty() {
        bail!("no supported audio files found ({})", EXTENSIONS.join(", "));
    }

    let results: Vec<(PathBuf, Result<Track>)> = files
        .into_par_iter()
        .map(|(key, file)| {
            let r = analyze(&file).with_context(|| file.display().to_string());
            (key, r)
        })
        .collect();

    let mut groups: BTreeMap<PathBuf, Vec<Track>> = BTreeMap::new();
    let mut failed = false;
    for (key, r) in results {
        match r {
            Ok(t) => groups.entry(key).or_default().push(t),
            Err(e) => {
                failed = true;
                eprintln!("error: {e:#}");
            }
        }
    }
    let albums: Vec<Album> = groups.into_iter().map(|(k, v)| album(k, v)).collect();

    if cli.write_tags {
        let jobs: Vec<(&Path, i64, i64)> =
            albums.iter().flat_map(|a| a.tracks.iter().map(move |t| (t.filename.as_path(), t.dr, a.dr))).collect();
        let errs: Vec<String> = jobs
            .par_iter()
            .filter_map(|(p, t, a)| write_tags(p, *t, *a).err().map(|e| format!("{}: {e:#}", p.display())))
            .collect();
        for e in &errs {
            eprintln!("error writing tags: {e}");
        }
        failed |= !errs.is_empty();
    }

    if !cli.quiet {
        let mut out = std::io::stdout().lock();
        match cli.format {
            Format::Json => writeln!(out, "{}", serde_json::to_string_pretty(&albums)?)?,
            Format::Txt => {
                let reports: Vec<String> = albums.iter().map(to_txt).collect();
                write!(out, "{}", reports.join("\n"))?;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disc_folders_belong_to_parent() {
        for d in ["A/Disc 1", "A/disc2", "A/CD 10", "A/cd3"] {
            assert_eq!(album_root(Path::new(d)), Path::new("A"), "{d}");
        }
        for d in ["A/Discography", "A/CD", "A/Disc 1a", "A/B"] {
            assert_eq!(album_root(Path::new(d)), Path::new(d), "{d}");
        }
    }
}
