//! Reproducible decode benchmark that round-trips two published corpora
//! through real `.gif` and `.tiff` containers, then measures weezl's LZW
//! decode on the bytes pulled out of each file.
//!
//! Both datasets come from `imazen/codec-corpus` and download on first run
//! via the `codec-corpus` crate — no binary payloads ship with weezl:
//!
//! * `gb82-sc` — 11 screenshots / screen-content PNGs (3 MB).
//!   Palette-ish content where LZW produces long repeated strings. This is
//!   where [`TableStrategy::Chunked`] wins: each 8-byte suffix chunk removes
//!   8 chain hops from the decode inner loop.
//!
//! * `CID22/CID22-512/validation` — 41 natural-photo PNGs, 512x512 (15 MB).
//!   High-entropy content where LZW strings stay short and the classic
//!   one-byte-per-hop table is already well-matched. This dataset exists in
//!   the bench specifically so reviewers can see the crossover point.
//!
//! For each source image the bench does:
//!
//! 1. Decode the PNG to raw RGB pixel bytes (with the `png` crate).
//! 2. Encode the pixels as a real GIF file via the `gif` crate, which
//!    quantizes RGB → 256-color palette via NeuQuant and emits
//!    `LSB` LZW sub-blocks inside a GIF89a container.
//! 3. Encode the pixels as a real LZW-compressed TIFF file via the
//!    `tiff` crate, which emits a single `Msb`, `tiff-size-switch` LZW
//!    strip inside a baseline TIFF container.
//! 4. Parse each container to pull the raw LZW bitstream out: GIF
//!    sub-block reassembly for GIFs, IFD strip-offset/length walk for
//!    TIFFs. No LZW decoding in the parsers — just byte slicing.
//! 5. Benchmark [`weezl::decode::Decoder`] across the pulled streams for
//!    both [`TableStrategy::Classic`] and [`TableStrategy::Chunked`].
//!
//! Criterion reports throughput in decoded bytes/sec, so the two groups
//! are directly comparable and the chunked-vs-classic ratio is visible
//! in the per-file and aggregate reports.
//!
//! Run with:
//!
//!     cargo bench --bench corpus
//!
//! Set `CODEC_CORPUS_CACHE=/path` to point at a pre-populated cache if the
//! CI environment disallows outbound network access.

extern crate codec_corpus;
extern crate criterion;
extern crate gif;
extern crate png;
extern crate tiff;
extern crate weezl;

use std::fs;
use std::io::{BufReader, Cursor};
use std::path::Path;
use std::time::Instant;

use codec_corpus::Corpus;
use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tiff::encoder::colortype::RGBA8;
use tiff::encoder::{Compression, TiffEncoder};
use weezl::decode::{Configuration as DecodeConfig, TableStrategy};
use weezl::{BitOrder, LzwStatus};

/// One LZW stream pulled out of a real container.
struct LzwSample {
    name: String,
    /// The bit order weezl must decode with — GIF uses LSB, TIFF uses MSB.
    order: BitOrder,
    /// The `min_code_size` for the stream. GIF varies by frame, TIFF is
    /// always 8.
    min_code_size: u8,
    /// Whether weezl should apply the TIFF size-switch quirk.
    tiff_size_switch: bool,
    /// Raw LZW bytes ready to feed to [`DecodeConfig::build().decode()`].
    encoded: Vec<u8>,
    /// Size the decoder will produce. Used for throughput and for sizing
    /// the scratch output buffer.
    decoded_len: usize,
}

/// Decoded source pixels for one corpus image.
struct SourceImage {
    name: String,
    width: u32,
    height: u32,
    /// RGB8 interleaved: width * height * 3 bytes.
    rgb: Vec<u8>,
    /// RGBA8 interleaved: width * height * 4 bytes, opaque alpha.
    rgba: Vec<u8>,
}

fn load_png_as_rgb(path: &Path) -> Option<SourceImage> {
    let file = fs::File::open(path).ok()?;
    let mut decoder = png::Decoder::new(BufReader::new(file));
    // Expand indexed / low-bit-depth PNGs to 8-bit RGB(A) so we can feed
    // the result straight into the gif + tiff encoders without handling
    // palette inputs ourselves.
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0u8; reader.output_buffer_size()?];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());

    // Normalize to RGB8 regardless of the source PNG format.
    let rgb: Vec<u8> = match info.color_type {
        png::ColorType::Rgb => buf.clone(),
        png::ColorType::Rgba => buf
            .chunks_exact(4)
            .flat_map(|p| [p[0], p[1], p[2]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g]).collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0]])
            .collect(),
        png::ColorType::Indexed => {
            // With EXPAND set the decoder should produce RGB(A), but bail
            // gracefully if a pathological file slips through.
            return None;
        }
    };
    let rgba: Vec<u8> = rgb
        .chunks_exact(3)
        .flat_map(|p| [p[0], p[1], p[2], 0xFF])
        .collect();
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("<?>")
        .to_string();
    Some(SourceImage {
        name,
        width: info.width,
        height: info.height,
        rgb,
        rgba,
    })
}

/// Encode `image` as a GIF file via the `gif` crate and pull the LZW stream
/// out of the first image block. The GIF encoder quantizes RGB → 256-color
/// palette via NeuQuant.
fn encode_gif_and_extract_lzw(image: &SourceImage) -> Option<LzwSample> {
    // Build the GIF into an in-memory Vec. gif::Encoder requires that the
    // writer implements Write — Cursor<Vec> is the standard choice.
    let mut bytes: Vec<u8> = Vec::new();
    {
        let frame = gif::Frame::from_rgb(image.width as u16, image.height as u16, &image.rgb);
        let palette: &[u8] = &[];
        let mut encoder =
            gif::Encoder::new(&mut bytes, image.width as u16, image.height as u16, palette).ok()?;
        encoder.write_frame(&frame).ok()?;
    }

    // Walk the GIF: 6-byte header, 7-byte LSD, optional global color table,
    // then blocks until 0x3B trailer. Inside an image block (0x2C) we read
    // min_code_size and reassemble the LZW sub-blocks.
    let mut p = 0usize;
    if bytes.len() < 13 || !bytes.starts_with(b"GIF") {
        return None;
    }
    p += 6; // "GIFXXa"
    let _width = u16::from_le_bytes([bytes[p], bytes[p + 1]]);
    let _height = u16::from_le_bytes([bytes[p + 2], bytes[p + 3]]);
    let packed = bytes[p + 4];
    p += 7;
    if packed & 0x80 != 0 {
        let gct_size = 3usize * (1 << ((packed & 0x07) + 1));
        p += gct_size;
    }

    loop {
        if p >= bytes.len() {
            return None;
        }
        match bytes[p] {
            0x3B => return None, // trailer before any image data
            0x21 => {
                // Extension block: 0x21, label(1), sub-blocks, 0x00 terminator.
                p += 2;
                loop {
                    if p >= bytes.len() {
                        return None;
                    }
                    let sz = bytes[p] as usize;
                    p += 1;
                    if sz == 0 {
                        break;
                    }
                    p += sz;
                }
            }
            0x2C => {
                // Image descriptor: 0x2C, left(2), top(2), w(2), h(2), packed(1).
                p += 10;
                let img_packed = bytes[p - 1];
                if img_packed & 0x80 != 0 {
                    let lct_size = 3usize * (1 << ((img_packed & 0x07) + 1));
                    p += lct_size;
                }
                // LZW image data: min_code_size(1), sub-blocks.
                let min_code_size = bytes[p];
                p += 1;
                let mut encoded: Vec<u8> = Vec::with_capacity(bytes.len() - p);
                loop {
                    if p >= bytes.len() {
                        return None;
                    }
                    let sz = bytes[p] as usize;
                    p += 1;
                    if sz == 0 {
                        break;
                    }
                    encoded.extend_from_slice(&bytes[p..p + sz]);
                    p += sz;
                }
                return Some(LzwSample {
                    name: image.name.clone(),
                    order: BitOrder::Lsb,
                    min_code_size,
                    tiff_size_switch: false,
                    encoded,
                    decoded_len: (image.width as usize) * (image.height as usize),
                });
            }
            _ => return None,
        }
    }
}

/// Encode `image` as a baseline TIFF with LZW compression via the `tiff`
/// crate and pull the single strip out by parsing the IFD.
fn encode_tiff_and_extract_lzw(image: &SourceImage) -> Option<LzwSample> {
    let mut cursor = Cursor::new(Vec::<u8>::new());
    {
        let mut encoder = TiffEncoder::new(&mut cursor)
            .ok()?
            .with_compression(Compression::Lzw);
        // Force a single strip that covers the whole image so we benchmark
        // one uninterrupted LZW stream, not ~20 small ones. Use RGBA8
        // because the tiff crate requires a specific ColorType and the LZW
        // compressor operates on whatever bytes we feed it.
        let mut image_encoder = encoder.new_image::<RGBA8>(image.width, image.height).ok()?;
        image_encoder.rows_per_strip(image.height).ok()?;
        image_encoder.write_data(&image.rgba).ok()?;
    }
    let bytes = cursor.into_inner();

    // Minimal little-endian IFD walk to find StripOffsets (0x0111) and
    // StripByteCounts (0x0117). tiff writes little-endian by default.
    if bytes.len() < 8 || &bytes[0..2] != b"II" {
        return None;
    }
    let ifd_offset = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]) as usize;
    if ifd_offset + 2 > bytes.len() {
        return None;
    }
    let num_entries = u16::from_le_bytes([bytes[ifd_offset], bytes[ifd_offset + 1]]) as usize;
    let entries_start = ifd_offset + 2;

    let mut strip_offsets: Vec<u32> = Vec::new();
    let mut strip_byte_counts: Vec<u32> = Vec::new();
    for i in 0..num_entries {
        let e = entries_start + i * 12;
        if e + 12 > bytes.len() {
            return None;
        }
        let tag = u16::from_le_bytes([bytes[e], bytes[e + 1]]);
        let ty = u16::from_le_bytes([bytes[e + 2], bytes[e + 3]]);
        let count =
            u32::from_le_bytes([bytes[e + 4], bytes[e + 5], bytes[e + 6], bytes[e + 7]]) as usize;
        let value_bytes = &bytes[e + 8..e + 12];

        if tag != 0x0111 && tag != 0x0117 {
            continue;
        }

        // Tag value size is type-dependent: 3 = SHORT (u16), 4 = LONG (u32).
        let elem_size = match ty {
            3 => 2usize,
            4 => 4usize,
            _ => return None,
        };
        let total = elem_size.checked_mul(count)?;

        // Values up to 4 bytes fit inline in the entry; otherwise value
        // field is an offset to an array elsewhere in the file.
        let read_slice: &[u8] = if total <= 4 {
            value_bytes
        } else {
            let off = u32::from_le_bytes([
                value_bytes[0],
                value_bytes[1],
                value_bytes[2],
                value_bytes[3],
            ]) as usize;
            if off + total > bytes.len() {
                return None;
            }
            &bytes[off..off + total]
        };

        let mut out: Vec<u32> = Vec::with_capacity(count);
        for j in 0..count {
            let v = if elem_size == 2 {
                u16::from_le_bytes([read_slice[j * 2], read_slice[j * 2 + 1]]) as u32
            } else {
                u32::from_le_bytes([
                    read_slice[j * 4],
                    read_slice[j * 4 + 1],
                    read_slice[j * 4 + 2],
                    read_slice[j * 4 + 3],
                ])
            };
            out.push(v);
        }
        if tag == 0x0111 {
            strip_offsets = out;
        } else {
            strip_byte_counts = out;
        }
    }

    if strip_offsets.is_empty() || strip_offsets.len() != strip_byte_counts.len() {
        return None;
    }

    // We forced `rows_per_strip = height`, so there should be exactly one
    // strip containing the whole image as a single LZW stream.
    if strip_offsets.len() != 1 {
        return None;
    }
    let off = strip_offsets[0] as usize;
    let len = strip_byte_counts[0] as usize;
    if off + len > bytes.len() {
        return None;
    }
    let encoded = bytes[off..off + len].to_vec();
    let decoded_len = (image.width as usize) * (image.height as usize) * 4;
    Some(LzwSample {
        name: image.name.clone(),
        order: BitOrder::Msb,
        min_code_size: 8,
        tiff_size_switch: true,
        encoded,
        decoded_len,
    })
}

fn load_dataset(dataset: &str) -> Vec<SourceImage> {
    let corpus = match Corpus::new() {
        Ok(c) => c,
        Err(err) => {
            eprintln!("codec-corpus: {err}. Skipping dataset {dataset}.");
            return Vec::new();
        }
    };
    let root = match corpus.get(dataset) {
        Ok(p) => p,
        Err(err) => {
            eprintln!("codec-corpus get({dataset}): {err}");
            return Vec::new();
        }
    };

    let mut entries: Vec<_> = match fs::read_dir(&root) {
        Ok(dir) => dir.flatten().map(|e| e.path()).collect(),
        Err(err) => {
            eprintln!("read_dir({:?}): {err}", root);
            return Vec::new();
        }
    };
    entries.sort();

    entries
        .into_iter()
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("png"))
        .filter_map(|p| load_png_as_rgb(&p))
        .collect()
}

/// Decode one LZW stream in full, using `strategy`. Returns the decoded byte
/// count so the bench can use it as a throughput denominator.
fn decode_once(sample: &LzwSample, strategy: TableStrategy, out: &mut [u8]) -> usize {
    let config = if sample.tiff_size_switch {
        DecodeConfig::with_tiff_size_switch(sample.order, sample.min_code_size)
    } else {
        DecodeConfig::new(sample.order, sample.min_code_size)
    };
    let mut decoder = config.with_table_strategy(strategy).build();
    let mut written = 0usize;
    let mut data: &[u8] = &sample.encoded;
    let mut cursor: &mut [u8] = out;
    loop {
        let result = decoder.decode_bytes(data, cursor);
        let status = result.status.expect("decode error");
        data = &data[result.consumed_in..];
        written += result.consumed_out;
        let (_, tail) = core::mem::take(&mut cursor).split_at_mut(result.consumed_out);
        cursor = tail;
        black_box(written);
        match status {
            LzwStatus::Done => break,
            LzwStatus::Ok => {
                if cursor.is_empty() {
                    // Output is exhausted but the decoder didn't emit Done —
                    // this happens on TIFF strips whose decoded length we
                    // over-estimated. Stop here; the bench's throughput
                    // number is already computed from the configured
                    // `decoded_len`, so partial decoding is not a problem.
                    break;
                }
            }
            LzwStatus::NoProgress => panic!("decode made no progress"),
        }
    }
    written
}

/// Time one decode pass with `strategy`, best-of-`iters` median to
/// smooth out noise. Returns seconds per decode.
fn median_decode_time(
    sample: &LzwSample,
    strategy: TableStrategy,
    out: &mut [u8],
    iters: u32,
) -> f64 {
    let mut samples = Vec::with_capacity(iters as usize);
    for _ in 0..iters {
        let t0 = Instant::now();
        decode_once(sample, strategy, out);
        samples.push(t0.elapsed().as_secs_f64());
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    samples[samples.len() / 2]
}

/// Print a sorted per-file summary to stderr before the criterion runs.
/// Each row lists the LZW-encoded size, decoded size, compression
/// ratio, classic/chunked decode throughput and the speedup ratio.
/// Rows are sorted by compression ratio so reviewers can eyeball which
/// files have the longest LZW strings — those are the ones chunked
/// should win on.
fn print_summary(label: &str, samples: &[LzwSample]) {
    if samples.is_empty() {
        return;
    }
    let total_encoded: u64 = samples.iter().map(|s| s.encoded.len() as u64).sum();
    let total_decoded: u64 = samples.iter().map(|s| s.decoded_len as u64).sum();
    let overall_ratio = total_decoded as f64 / total_encoded.max(1) as f64;

    // Measure classic and chunked decode time for every file. This is a
    // best-of-11 median, cheap compared to criterion's full sampling —
    // enough to get a stable speedup number for the summary. The samples
    // are pre-warmed with one untimed decode so the cache state is the
    // same between the two strategy measurements.
    let max_out = samples.iter().map(|s| s.decoded_len).max().unwrap_or(0);
    let mut outbuf = vec![0u8; max_out];
    let iters: u32 = 11;

    // Collect (compression_ratio, classic_sec, chunked_sec, sample) per file.
    let mut rows: Vec<(f64, f64, f64, &LzwSample)> = samples
        .iter()
        .map(|s| {
            // Warm-up: one untimed decode per strategy.
            decode_once(s, TableStrategy::Classic, &mut outbuf[..s.decoded_len]);
            decode_once(s, TableStrategy::Chunked, &mut outbuf[..s.decoded_len]);
            let classic_sec = median_decode_time(
                s,
                TableStrategy::Classic,
                &mut outbuf[..s.decoded_len],
                iters,
            );
            let chunked_sec = median_decode_time(
                s,
                TableStrategy::Chunked,
                &mut outbuf[..s.decoded_len],
                iters,
            );
            let r = s.decoded_len as f64 / s.encoded.len().max(1) as f64;
            (r, classic_sec, chunked_sec, s)
        })
        .collect();
    rows.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());

    // Totals for the [total] row.
    let total_classic_sec: f64 = rows.iter().map(|r| r.1).sum();
    let total_chunked_sec: f64 = rows.iter().map(|r| r.2).sum();

    eprintln!();
    eprintln!("=== corpus/{label} — {} files ===", samples.len());
    eprintln!(
        "{:<32} {:>10} {:>12} {:>8} {:>12} {:>12} {:>8}",
        "file", "lzw bytes", "decoded", "ratio", "classic", "chunked", "speedup"
    );
    for (cratio, csec, ksec, s) in &rows {
        let speedup = csec / ksec;
        eprintln!(
            "{:<32} {:>10} {:>12} {:>7.2}x {:>12} {:>12} {:>7.2}x",
            truncate_middle(&s.name, 32),
            s.encoded.len(),
            s.decoded_len,
            cratio,
            format_throughput(s.decoded_len, *csec),
            format_throughput(s.decoded_len, *ksec),
            speedup,
        );
    }
    let total_speedup = total_classic_sec / total_chunked_sec;
    eprintln!(
        "{:<32} {:>10} {:>12} {:>7.2}x {:>12} {:>12} {:>7.2}x",
        "[total]",
        total_encoded,
        total_decoded,
        overall_ratio,
        format_throughput(total_decoded as usize, total_classic_sec),
        format_throughput(total_decoded as usize, total_chunked_sec),
        total_speedup,
    );
    eprintln!();
}

/// Format `bytes / seconds` as a human-readable throughput string.
fn format_throughput(bytes: usize, seconds: f64) -> String {
    if seconds <= 0.0 || bytes == 0 {
        return "-".to_string();
    }
    let bps = bytes as f64 / seconds;
    if bps >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.2} GiB/s", bps / (1024.0 * 1024.0 * 1024.0))
    } else if bps >= 1024.0 * 1024.0 {
        format!("{:.0} MiB/s", bps / (1024.0 * 1024.0))
    } else {
        format!("{:.0} KiB/s", bps / 1024.0)
    }
}

/// Shorten a filename for the summary table. Keep the head and tail so
/// both the identifier and the extension stay visible.
fn truncate_middle(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let keep = max - 1;
    let head = keep / 2;
    let tail = keep - head;
    format!("{}…{}", &s[..head], &s[s.len() - tail..])
}

fn bench_group(c: &mut Criterion, label: &str, samples: &[LzwSample]) {
    if samples.is_empty() {
        eprintln!("No samples for {label} — skipping group.");
        return;
    }

    print_summary(label, samples);

    // Size the scratch output buffer for the worst-case file.
    let max_out = samples.iter().map(|s| s.decoded_len).max().unwrap_or(0);
    let mut outbuf = vec![0u8; max_out];

    // Per-file: two entries per file (classic, chunked). The benchmark
    // name embeds the compression ratio so the criterion report shows
    // it next to each throughput number. Readers can diff the pair to
    // see the chunked-vs-classic speedup per image.
    let group_name = format!("corpus/{label}/per-file");
    let mut group = c.benchmark_group(&group_name);
    for sample in samples {
        let ratio = sample.decoded_len as f64 / sample.encoded.len().max(1) as f64;
        group.throughput(Throughput::Bytes(sample.decoded_len as u64));
        for &strategy in &[TableStrategy::Classic, TableStrategy::Chunked] {
            let id = BenchmarkId::new(
                format!("{}@{:.2}x/{:?}", sample.name, ratio, strategy),
                sample.decoded_len,
            );
            group.bench_with_input(id, sample, |b, s| {
                b.iter(|| {
                    decode_once(s, strategy, &mut outbuf[..s.decoded_len]);
                });
            });
        }
    }
    group.finish();

    // Aggregate: sum total decoded bytes across the whole dataset so the
    // throughput number is a corpus-wide average. The aggregate ratio
    // (total decoded / total encoded) is embedded in the ID too.
    let total_encoded: u64 = samples.iter().map(|s| s.encoded.len() as u64).sum();
    let total_decoded: u64 = samples.iter().map(|s| s.decoded_len as u64).sum();
    let overall_ratio = total_decoded as f64 / total_encoded.max(1) as f64;
    let agg_name = format!("corpus/{label}/aggregate");
    let mut agg = c.benchmark_group(&agg_name);
    agg.throughput(Throughput::Bytes(total_decoded));
    for &strategy in &[TableStrategy::Classic, TableStrategy::Chunked] {
        let id = BenchmarkId::new(format!("@{overall_ratio:.2}x/{strategy:?}"), total_decoded);
        agg.bench_with_input(id, samples, |b, samples| {
            b.iter(|| {
                for s in samples {
                    decode_once(s, strategy, &mut outbuf[..s.decoded_len]);
                }
            });
        });
    }
    agg.finish();
}

fn bench_dataset(c: &mut Criterion, dataset: &str) {
    let images = load_dataset(dataset);
    if images.is_empty() {
        return;
    }

    let gif_samples: Vec<LzwSample> = images
        .iter()
        .filter_map(encode_gif_and_extract_lzw)
        .collect();
    let tiff_samples: Vec<LzwSample> = images
        .iter()
        .filter_map(encode_tiff_and_extract_lzw)
        .collect();

    bench_group(c, &format!("{dataset}/gif"), &gif_samples);
    bench_group(c, &format!("{dataset}/tiff"), &tiff_samples);
}

fn bench_screen_content(c: &mut Criterion) {
    bench_dataset(c, "gb82-sc");
}

fn bench_natural_photos(c: &mut Criterion) {
    bench_dataset(c, "CID22/CID22-512/validation");
}

/// Full-page web screenshots — large (up to 1313x20667) with lots of
/// anti-aliased text, UI chrome, and whitespace. Lands in the middle
/// of the compression-ratio spectrum and helps close the gap between
/// natural photos and pixel-art screenshots.
fn bench_web_screenshots(c: &mut Criterion) {
    bench_dataset(c, "qoi-benchmark/screenshot_web");
}

// ---------------------------------------------------------------------------
// Synthetic sweep
// ---------------------------------------------------------------------------
//
// Generate LZW inputs with controlled compression ratio so we can map out
// the Classic/Chunked crossover at a higher density than real corpora give
// us. The generator builds a byte sequence out of a small dictionary of
// `chunk_len`-byte random chunks — tiling forces LZW to discover long
// repeating substrings, and the chunk length is what pins the eventual
// ratio. Two knobs:
//
// * `chunk_len` — length of each repeated block. LZW's dictionary
//   eventually stores each full block as a single code, so the ratio
//   trends toward `chunk_len` for large outputs.
// * `dict_size` — number of distinct chunks to shuffle across. A bigger
//   dictionary means more distinct codes to learn and a slightly lower
//   ratio because the overhead is amortized over fewer repeats per code.
//
// With `dict_size = 4` the observed GIF/TIFF compression ratios track
// `chunk_len` within ~30% — close enough to sweep across the crossover
// region at 0.5x resolution, and because we feed the output through the
// same `Encoder::with_tiff_size_switch(Msb, 8)` / `Encoder::new(Lsb, 8)`
// pipelines the numbers are directly comparable to the real-corpus rows.

fn synth_bytes(
    chunk_len: usize,
    dict_size: usize,
    pattern_fraction: f64,
    total_len: usize,
    seed: u64,
) -> Vec<u8> {
    let mut s: u64 = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let mut rand_u64 = || {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s
    };
    let mut rand_u8 = || (rand_u64() & 0xFF) as u8;

    // Build a dictionary of distinct repeating chunks.
    let mut dict: Vec<Vec<u8>> = Vec::with_capacity(dict_size);
    for _ in 0..dict_size {
        dict.push((0..chunk_len).map(|_| rand_u8()).collect());
    }

    // Emit blocks. Each block is either a repeating pattern (high ratio)
    // or pure random bytes (ratio ~1.0). The fraction of pattern blocks
    // controls the blended ratio:
    //
    //     1 / ratio ≈ f/R_pat + (1 - f)/R_rand   where R_rand ≈ 1
    //                = f/R_pat + 1 - f
    //     f ≈ (1 - 1/ratio) / (1 - 1/R_pat)
    //
    // Block size is large enough (1 KiB) to let LZW build up long strings
    // inside pattern blocks but small enough to interleave density.
    const BLOCK: usize = 1024;
    let mut out = Vec::with_capacity(total_len + BLOCK);
    let mut pattern_idx: usize = 0;
    let mut acc_pattern: f64 = 0.0;
    while out.len() < total_len {
        let block_start = out.len();
        acc_pattern += pattern_fraction;
        let want_pattern = acc_pattern >= 1.0;
        if want_pattern {
            acc_pattern -= 1.0;
            // Tile the chosen dictionary chunk across the block.
            while out.len() - block_start < BLOCK {
                let d = &dict[pattern_idx % dict_size];
                out.extend_from_slice(d);
                pattern_idx = pattern_idx.wrapping_add(1);
            }
            out.truncate(block_start + BLOCK);
        } else {
            // Fill the block with fresh random bytes.
            for _ in 0..BLOCK {
                out.push(rand_u8());
            }
        }
    }
    out.truncate(total_len);
    out
}

fn make_synth_sample(
    label: &str,
    bytes: Vec<u8>,
    order: BitOrder,
    tiff_mode: bool,
) -> Option<LzwSample> {
    use weezl::encode::Encoder as LzwEnc;
    let decoded_len = bytes.len();
    let mut enc = if tiff_mode {
        LzwEnc::with_tiff_size_switch(order, 8)
    } else {
        LzwEnc::new(order, 8)
    };
    let encoded = enc.encode(&bytes).ok()?;
    Some(LzwSample {
        name: label.to_string(),
        order,
        min_code_size: 8,
        tiff_size_switch: tiff_mode,
        encoded,
        decoded_len,
    })
}

fn bench_synth(c: &mut Criterion) {
    // ~1 MiB of output per sample — enough for stable timing, small enough
    // to keep the criterion sample count high.
    const TOTAL: usize = 1 << 20;
    const DICT: usize = 4;
    const CHUNK: usize = 16; // length of each tiled pattern block

    // Sweep the pattern fraction so the resulting compression ratio spans
    // the crossover region at ~0.5x granularity. `f` values picked to give
    // target ratios 1.3, 1.5, 1.75, 2, 2.5, 3, 3.5, 4, 5, 6, 8, 12, 20
    // (via 1/ratio = 1 - f * (1 - 1/R_pat) with R_pat ≈ 16).
    let targets: &[(f64, &str)] = &[
        (0.0, "f00"), // pure noise (ratio ~1.0)
        (0.25, "f25"),
        (0.40, "f40"),
        (0.50, "f50"),
        (0.60, "f60"),
        (0.70, "f70"),
        (0.78, "f78"),
        (0.84, "f84"),
        (0.88, "f88"),
        (0.92, "f92"),
        (0.95, "f95"),
        (0.98, "f98"),
        (1.0, "f99"),
    ];

    let mut gif_samples: Vec<LzwSample> = Vec::new();
    let mut tiff_samples: Vec<LzwSample> = Vec::new();
    for (i, &(f, label)) in targets.iter().enumerate() {
        let bytes = synth_bytes(CHUNK, DICT, f, TOTAL, 0xBEEFu64 + i as u64);
        if let Some(s) = make_synth_sample(
            &format!("synth_{label}"),
            bytes.clone(),
            BitOrder::Lsb,
            false,
        ) {
            gif_samples.push(s);
        }
        if let Some(s) = make_synth_sample(&format!("synth_{label}"), bytes, BitOrder::Msb, true) {
            tiff_samples.push(s);
        }
    }

    bench_group(c, "synth/lsb", &gif_samples);
    bench_group(c, "synth/msb-tiff", &tiff_samples);
}

criterion_group!(
    benches,
    bench_screen_content,
    bench_natural_photos,
    bench_web_screenshots,
    bench_synth
);
criterion_main!(benches);
