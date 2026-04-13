//! Synthetic LZW workload generators fitted to real-world data.
//!
//! Earlier versions of these generators used simple byte-stat fitting
//! (entropy, run-length, compression ratio) which matched the marginal
//! distributions of real scans but missed the LZW code-stream structure
//! entirely — width_12 was off by 99%, literal_frac by 50%. The decoder
//! cost model depends on code-level features (literal vs copy fractions,
//! bit-width distribution, KwKwK rate), so byte-stat-only fit gave
//! misleading perf comparisons between strategies. See
//! `docs/code-level-fit-analysis.md` for the full post-mortem.
//!
//! The current generators use three structural models that reproduce
//! both byte-level AND code-level features of real LZW streams:
//!
//! 1. **Document generator** (`GenParams` + `generate`): Markov chain
//!    BG/FG/noise model with pattern library and row-repeat template
//!    tiling. Produces scanned-document-like byte streams fitted to
//!    RVL-CDIP and Brown v. Board real corpus data. Pattern library
//!    simulates recurring character glyphs; row-repeat simulates
//!    scanline-aligned text structure. Code-level fit within 5% on
//!    literal/short_copy/long_copy/width_12 for scanned documents.
//!
//! 2. **Photo generator** (`PhotoParams` + `generate_photo`): Random walk
//!    with flat-hold regions and film grain noise. Produces continuous-tone
//!    byte streams fitted to NASA Apollo film scans, Cleveland Museum of
//!    Art CC0 paintings, and CLIC 2025 validation images. Code-level fit
//!    within 5% for grayscale photos, within 2% for color.
//!
//! 3. **Flat-UI generator** (`generate_flat_ui`): Palette-indexed block
//!    structure simulating GIF screenshots and flat-design graphics.
//!
//! Python reference implementations live in `tools/fit_generator.py`.
//! Both produce byte-for-byte identical output from the same seed.

// ---------------------------------------------------------------------------
// xorshift32 PRNG
// ---------------------------------------------------------------------------

/// xorshift32 PRNG — deterministic, no deps. Must match tools/fit_generator.py.
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u32) -> Self {
        Self(seed | 1)
    }
    pub fn next(&mut self) -> u32 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        self.0
    }
}

// ---------------------------------------------------------------------------
// Document generator (Markov chain + pattern library + row-repeat)
// ---------------------------------------------------------------------------

/// Parameters for the scanned-document Markov generator.
///
/// See `tools/fit_generator.py` for the Python reference implementation —
/// the two share the same xorshift32 sequence and branching structure,
/// so Rust output matches what the fitter measured.
pub struct GenParams {
    /// Probability, per BG pixel, of transitioning BG → FG.
    pub p_bg_to_fg: f64,
    /// Probability, per FG pixel, of transitioning FG → BG.
    pub p_fg_to_bg: f64,
    /// Probability, per clean-BG pixel, of starting a noise burst
    /// (transitioning BG → BG_NOISE).
    pub bg_burst_p: f64,
    /// Probability, per BG_NOISE pixel, of ending the burst.
    /// `1.0` = i.i.d. Bernoulli noise. Smaller values cluster noise
    /// spatially (geometric burst length).
    pub bg_burst_end_p: f64,
    /// FG base byte picked once per FG stretch, uniform in [fg_lo, fg_hi].
    pub fg_lo: u8,
    pub fg_hi: u8,
    /// FG pixels: `fg_base + uniform(-jitter, +jitter)`, clamped to 0..=255.
    pub fg_jitter: u8,
    /// Number of fixed byte patterns in the glyph library. 0 = disabled.
    pub n_patterns: u32,
    /// Length of each pattern in bytes.
    pub pattern_len: u32,
    /// Probability of emitting a pattern instead of jitter when entering FG.
    pub pattern_frac: f64,
    /// Row width for scanline-repeat mode. 0 = flat stream.
    pub row_width: u32,
    /// Number of distinct template rows in scanline-repeat mode.
    pub n_template_rows: u32,
    /// Per-pixel noise on ink (non-255) pixels during row tiling.
    pub row_noise: u8,
}

pub fn generate(params: &GenParams, len: usize, seed: u32) -> Vec<u8> {
    // Row-repeat mode: generate N template rows, tile with ink noise.
    if params.row_width > 0 {
        let rw = params.row_width as usize;
        let n_tpl = (params.n_template_rows as usize).max(1);
        let inner = GenParams {
            row_width: 0,
            n_template_rows: 0,
            row_noise: 0,
            ..*params
        };
        let templates: Vec<Vec<u8>> = (0..n_tpl)
            .map(|t| generate(&inner, rw, seed.wrapping_add(t as u32)))
            .collect();
        let noise = params.row_noise as i16;
        let noise_span = (2 * noise + 1).max(1) as u32;
        let mut rng = Rng::new(seed);
        let mut out = Vec::with_capacity(len);
        for i in 0..len {
            let row = i / rw;
            let v = templates[row % n_tpl][i % rw];
            let v = if noise > 0 && v != 255 {
                let delta = (rng.next() % noise_span) as i16 - noise;
                (v as i16 + delta).clamp(0, 255) as u8
            } else {
                v
            };
            out.push(v);
        }
        return out;
    }

    let mut rng = Rng::new(seed);
    let u = u32::MAX as f64;
    let fg_lo = params.fg_lo as i16;
    let fg_hi = params.fg_hi as i16;
    let fg_span = (fg_hi - fg_lo + 1).max(1) as u32;
    let jitter = params.fg_jitter as i16;
    let jitter_span = (2 * jitter + 1).max(1) as u32;

    // Build pattern library from the PRNG stream.
    let patterns: Vec<Vec<u8>> = if params.n_patterns > 0 && params.pattern_frac > 0.0 {
        (0..params.n_patterns)
            .map(|_| {
                let base = fg_lo + (rng.next() % fg_span) as i16;
                (0..params.pattern_len)
                    .map(|_| {
                        let delta = (rng.next() % jitter_span) as i16 - jitter;
                        (base + delta).clamp(0, 255) as u8
                    })
                    .collect()
            })
            .collect()
    } else {
        Vec::new()
    };

    let mut out = Vec::with_capacity(len);
    let mut state = 0u8; // 0=BG, 1=FG, 2=BG_NOISE, 3=PATTERN
    let mut fg_base: i16 = 0;
    let mut pat_idx: usize = 0;
    let mut pat_pos: u32 = 0;

    while out.len() < len {
        match state {
            0 => {
                out.push(255);
                if (rng.next() as f64 / u) < params.p_bg_to_fg {
                    if !patterns.is_empty() && (rng.next() as f64 / u) < params.pattern_frac {
                        state = 3;
                        pat_idx = (rng.next() % params.n_patterns) as usize;
                        pat_pos = 0;
                    } else {
                        state = 1;
                        fg_base = fg_lo + (rng.next() % fg_span) as i16;
                    }
                } else if (rng.next() as f64 / u) < params.bg_burst_p {
                    state = 2;
                }
            }
            1 => {
                let delta = (rng.next() % jitter_span) as i16 - jitter;
                let v = (fg_base + delta).clamp(0, 255) as u8;
                out.push(v);
                if (rng.next() as f64 / u) < params.p_fg_to_bg {
                    state = 0;
                }
            }
            2 => {
                out.push((rng.next() % 255) as u8);
                if (rng.next() as f64 / u) < params.bg_burst_end_p {
                    state = 0;
                }
            }
            _ => {
                out.push(patterns[pat_idx][pat_pos as usize]);
                pat_pos += 1;
                if pat_pos >= params.pattern_len {
                    state = 0;
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Photo generator (random walk + flat-hold regions)
// ---------------------------------------------------------------------------

/// Parameters for the continuous-tone photo generator.
pub struct PhotoParams {
    /// Max step per pixel: prev + uniform(-delta, +delta).
    pub walk_delta: u8,
    /// Probability of jumping to a random value (edge).
    pub edge_p: f64,
    /// Number of channels (1 = grayscale, 3 = RGB interleaved).
    pub channels: u8,
    /// Probability per WALK pixel of entering FLAT mode.
    pub flat_p: f64,
    /// Probability per FLAT pixel of returning to WALK mode.
    pub flat_end_p: f64,
    /// FLAT mode center value range.
    pub flat_val_lo: u8,
    pub flat_val_hi: u8,
    /// Per-pixel noise in FLAT mode (film grain).
    pub flat_noise: u8,
}

pub fn generate_photo(params: &PhotoParams, len: usize, seed: u32) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let u = u32::MAX as f64;
    let delta = params.walk_delta as i16;
    let delta_span = (2 * delta + 1).max(1) as u32;
    let channels = params.channels.max(1) as usize;
    let flat_val_span = (params.flat_val_hi as u32).saturating_sub(params.flat_val_lo as u32) + 1;
    let flat_noise = params.flat_noise as i16;
    let noise_span = (2 * flat_noise + 1).max(1) as u32;

    let mut out = Vec::with_capacity(len);
    let mut val = [128i16; 3];
    let mut flat = [false; 3];
    let mut flat_hold = [0i16; 3];

    while out.len() < len {
        for ch in 0..channels {
            if out.len() >= len {
                break;
            }
            if flat[ch] {
                let v = if flat_noise > 0 {
                    let n = (rng.next() % noise_span) as i16 - flat_noise;
                    (flat_hold[ch] + n).clamp(0, 255)
                } else {
                    flat_hold[ch]
                };
                out.push(v as u8);
                if (rng.next() as f64 / u) < params.flat_end_p {
                    flat[ch] = false;
                    val[ch] = flat_hold[ch];
                }
            } else {
                if (rng.next() as f64 / u) < params.edge_p {
                    val[ch] = (rng.next() % 256) as i16;
                }
                let step = (rng.next() % delta_span) as i16 - delta;
                val[ch] = (val[ch] + step).clamp(0, 255);
                out.push(val[ch] as u8);
                if (rng.next() as f64 / u) < params.flat_p {
                    flat[ch] = true;
                    flat_hold[ch] = (params.flat_val_lo as u32 + rng.next() % flat_val_span) as i16;
                }
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Flat-UI GIF generator (palette-indexed block structure)
// ---------------------------------------------------------------------------

/// Generate a flat-UI-style byte stream: palette-indexed values (0-31),
/// rectangular blocks of solid color with sharp edges.
pub fn generate_flat_ui(len: usize, seed: u32) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    let width = 640;
    let palette_size = 32u32;
    let mut out = Vec::with_capacity(len);

    let mut row = vec![0u8; width];
    let mut col = 0;
    while col < width {
        let color = (rng.next() % palette_size) as u8;
        let block_w = 20 + (rng.next() % 120) as usize;
        let end = (col + block_w).min(width);
        for c in col..end {
            row[c] = color;
        }
        col = end;
    }

    let mut rows_until_change = 10 + (rng.next() % 40) as usize;
    while out.len() < len {
        for &b in row.iter() {
            if out.len() >= len {
                break;
            }
            out.push(b);
        }
        rows_until_change -= 1;
        if rows_until_change == 0 {
            let n_changes = 1 + (rng.next() % 4) as usize;
            for _ in 0..n_changes {
                let start = (rng.next() % width as u32) as usize;
                let bw = 20 + (rng.next() % 120) as usize;
                let color = (rng.next() % palette_size) as u8;
                let end = (start + bw).min(width);
                for c in start..end {
                    row[c] = color;
                }
            }
            rows_until_change = 10 + (rng.next() % 40) as usize;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Fitted archetypes
// ---------------------------------------------------------------------------

/// Email class median (n=50 RVL-CDIP). Row-repeat with 4 templates.
pub const SCANNED_EMAIL: GenParams = GenParams {
    p_bg_to_fg: 0.007000,
    p_fg_to_bg: 0.25,
    bg_burst_p: 0.000500,
    bg_burst_end_p: 1.0,
    fg_lo: 0,
    fg_hi: 150,
    fg_jitter: 5,
    n_patterns: 40,
    pattern_len: 4,
    pattern_frac: 0.3,
    row_width: 512,
    n_template_rows: 4,
    row_noise: 12,
};

/// Blank form — high compression tail (RVL-CDIP form class, ratio 50-80).
pub const SCANNED_BLANK: GenParams = GenParams {
    p_bg_to_fg: 0.001700,
    p_fg_to_bg: 0.25,
    bg_burst_p: 0.000075,
    bg_burst_end_p: 0.075,
    fg_lo: 0,
    fg_hi: 120,
    fg_jitter: 10,
    n_patterns: 0,
    pattern_len: 0,
    pattern_frac: 0.0,
    row_width: 0,
    n_template_rows: 0,
    row_noise: 0,
};

/// Dense text document (RVL-CDIP email low-ratio tail).
pub const SCANNED_DENSE: GenParams = GenParams {
    p_bg_to_fg: 0.025000,
    p_fg_to_bg: 0.25,
    bg_burst_p: 0.0,
    bg_burst_end_p: 1.0,
    fg_lo: 0,
    fg_hi: 150,
    fg_jitter: 5,
    n_patterns: 0,
    pattern_len: 0,
    pattern_frac: 0.0,
    row_width: 0,
    n_template_rows: 0,
    row_noise: 0,
};

/// Old monochrome document scan (Brown v. Board, 79 pages).
pub const SCANNED_MONOCHROME_OLD: GenParams = GenParams {
    p_bg_to_fg: 0.050000,
    p_fg_to_bg: 0.25,
    bg_burst_p: 0.0,
    bg_burst_end_p: 0.2,
    fg_lo: 0,
    fg_hi: 200,
    fg_jitter: 3,
    n_patterns: 40,
    pattern_len: 6,
    pattern_frac: 0.5,
    row_width: 0,
    n_template_rows: 0,
    row_noise: 0,
};

/// Grayscale photo (Apollo Full Earth film scan).
pub const PHOTO_GRAY: PhotoParams = PhotoParams {
    walk_delta: 6,
    edge_p: 0.02,
    channels: 1,
    flat_p: 0.05,
    flat_end_p: 0.005,
    flat_val_lo: 0,
    flat_val_hi: 3,
    flat_noise: 3,
};

/// Vivid color photo (CMA "July" painting, LZW worst case).
pub const PHOTO_COLOR: PhotoParams = PhotoParams {
    walk_delta: 1,
    edge_p: 0.03,
    channels: 3,
    flat_p: 0.03,
    flat_end_p: 0.10,
    flat_val_lo: 0,
    flat_val_hi: 50,
    flat_noise: 6,
};
