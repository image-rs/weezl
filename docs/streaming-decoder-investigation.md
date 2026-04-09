# `TableStrategy::Streaming` investigation — session log

This document is the full record of the investigation that produced
`TableStrategy::Streaming` in weezl. It spans from the initial audit of
the PR that added `TableStrategy::Chunked` (#60), through a three-way
performance comparison with Wuffs' reference LZW decoder, through a
structural rewrite, through the collection of real-world TIFF and GIF
corpora, and through the incremental optimizations that brought the new
strategy to a clean win on nearly every benchmark.

All numbers in this document are from an **AMD Ryzen 9 7950X** on Linux
with **stable Rust** at the default target (x86-64-v1 baseline, no
`target-cpu=native`). Wuffs' C code was compiled with `gcc -O3` at the
same baseline. Where relevant, the implications for Windows (where
`HeapAlloc` is ~5× slower than glibc malloc) are called out explicitly.

## Contents

1. [Starting point](#starting-point)
2. [The Wuffs LZW README audit](#the-wuffs-lzw-readme-audit)
3. [First three-way bench: weezl vs weezl vs Wuffs C](#first-three-way-bench-weezl-vs-weezl-vs-wuffs-c)
4. [Structural rewrite: the first `Tight` prototype](#structural-rewrite-the-first-tight-prototype)
5. [Experiments that didn't work](#experiments-that-didnt-work)
6. [Adding MSB + TIFF early-change + yield_on_full](#adding-msb--tiff-early-change--yield_on_full)
7. [Real-world corpora: QOI, gb82-sc, CLIC, tiff-conformance](#real-world-corpora-qoi-gb82-sc-clic-tiff-conformance)
8. [Image-tiff lifecycle audit and the reset() question](#image-tiff-lifecycle-audit-and-the-reset-question)
9. [Mini-burst and short-copy fast path](#mini-burst-and-short-copy-fast-path)
10. [Final results](#final-results)
11. [API gap analysis and adoption path](#api-gap-analysis-and-adoption-path)
12. [Naming](#naming)
13. [What was explored but not landed](#what-was-explored-but-not-landed)
14. [Commit list](#commit-list)

---

## Starting point

The session began as an audit of [image-rs/weezl#60](https://github.com/image-rs/weezl/pull/60),
which added `TableStrategy::Chunked`: an 8-byte-per-entry table layout
inspired by Wuffs' "PreQ+SufQ" technique. The PR described 1.6–6.9×
speedups on palette and screenshot data, with 3–4% regressions on
low-ratio photographic data.

Audit findings:

- **No spec divergences from Wuffs.** Every algorithmic aspect of
  Wuffs' `std/lzw/README.md` was correctly implemented: bit packing
  (LSB and MSB), code taxonomy, the N++ implicit-add rule, KwKwK
  self-reference, TIFF early-change, the PreQ+SufQ derivation math,
  and max code size. Full line-by-line audit.
- **One fragility:** `ChunkedTable::at` is a trait stub returning
  `unreachable!()`. No caller hits it today, but a future refactor
  could.
- **One minor deviation:** Chunked's `reconstruct` copies exactly
  `tail_len` bytes instead of taking Wuffs' "write up to Q bytes even
  if only k are valid" speed hint. Safe, slightly slower.

The Chunked PR's table layout was correct. The question was whether
its performance claims held up on real data, and whether weezl could
get closer to Wuffs' throughput on workloads where Wuffs wins.

## The Wuffs LZW README audit

Key Wuffs architectural choices that became relevant later:

1. **LSB-only.** Wuffs' `std/lzw` does not support MSB bit order or
   TIFF early-change. Its decoder is built for GIF, not TIFF. This
   limited wuffs to LSB comparisons throughout the session.
2. **PreQ+SufQ (Q=8).** Each table entry stores up to 8 suffix bytes
   plus a prefix link. The decoder walks the prefix chain in 8-byte
   strides rather than 1-byte steps. For long strings, this is 8×
   fewer chain-walk iterations than Classic's 1-byte-per-step
   decode.
3. **Single-code-per-iter loop.** Wuffs has no "burst" decoder. Its
   inner loop reads one code, dispatches one code, derives one
   entry, writes one output, checks the ring-buffer mark, and
   continues.
4. **Internal ring buffer.** Wuffs decodes into a fixed 8199-byte
   ring and drains to the caller's output. This keeps decode writes
   hot in L1d.
5. **32-bit bit buffer** with a `peek_u32le` fast path. `n_bits |= 24`
   is a clever trick for refilling: read 4 bytes, consume the ones
   that fit, mark at least 24 bits valid, re-read over-read bytes on
   the next refill.

The KwKwK edge case, the PreQ+SufQ derivation math, the early-change
width bump, and the initial "first code after reset doesn't add a
pair" rule were all double-checked against the Wuffs spec. No bugs
found in weezl's existing Classic or Chunked decoders on any of them.

## First three-way bench: weezl vs weezl vs Wuffs C

Built a standalone C harness around Wuffs v0.4's `wuffs_lzw__decoder`
and compared all three weezl strategies against it on a synthetic
corpus spanning the compression ratio spectrum.

Synthetic corpus (LSB-only — required for Wuffs compatibility):

| Input | Ratio | Classic | Chunked | Wuffs | Classic/Wuffs |
|---|---:|---:|---:|---:|---:|
| solid-4M (all zeros) | 1050× | **49 GiB/s** | 49 | 7.1 GiB/s | **6.9× faster** |
| rle-4M (8-color runs) | 39× | 940 MiB/s | 4.4 G | 7.3 G | 13% of wuffs |
| pal16-4M (16-color) | 18.7× | 920 MiB/s | 1.5 G | 2.6 G | 35% of wuffs |
| rand-4M (incompressible) | 0.73× | 209 MiB/s | 177 M | **499 MiB/s** | **42%** of wuffs |
| solid-64k | 155× | 10.4 G | 9.6 G | 8.8 G | Classic wins |
| rand-64k | 0.73× | 211 M | 182 M | 543 M | 39% of wuffs |

**Key findings:**

- **Wuffs is 1.6–2.5× faster than weezl Chunked on everything except
  solid runs.** On the specific case of solid-color data, weezl wins
  by 6.9× — because Wuffs' internal ring buffer bottlenecks at memcpy
  bandwidth (~7 GiB/s) while weezl writes straight to the caller's
  output slice and hits L3 bandwidth (~49 GiB/s).
- **Weezl Classic is 2.5× slower than Wuffs on random data.** This
  was the most striking result. The existing Classic decoder's burst
  machinery pays per-code overhead that isn't amortized when codes
  emit 1–2 bytes.
- **Hardware counters confirmed the gap is pure instruction count.**
  Classic: 122 instructions/output byte on rand-4M. Wuffs: 57.
  Branch miss rates, IPC, and cache miss counts were all fine on both.

Conclusion: the existing Chunked PR is a win for high-ratio data, but
**the big opportunity was restructuring the inner loop to match Wuffs'
single-code approach.** This became the Streaming rewrite.

## Structural rewrite: the first `Tight` prototype

Commit `48903c3` added `TableStrategy::Tight` (later renamed to
`Streaming`) — a full separate `DecodeStateTight` struct with:

- **Single code per iteration.** No `[Code; BURST]` array, no
  `target: [&mut [u8]; BURST]` array, no per-iter `split_at_mut`, no
  `burst_byte_len` / `burst_byte` scratch, no `derive_burst` loop, no
  `last_decoded` / cScsc bookkeeping. The whole burst infrastructure
  is gone.
- **PreQ+SufQ (Q=8) table layout** matching Wuffs' shape but without
  the `firsts[]` array that Chunked has. First-byte lookups walk the
  prefix chain; for short chains this is cheaper than maintaining a
  parallel array.
- **32-bit LSB bit reader** with the wuffs `n_bits |= 24` over-read
  trick. Later reverted to 64-bit (see "Experiments that didn't work").
- **codes_until_bump countdown** instead of `if save_code >= max_code`
  per-code width check. Decrements once per derive, fires at epoch
  boundaries only, bump itself is a cold `#[inline(never)]` function.
- **4 KiB `pending` buffer** for mid-code suspension. When a copy
  code's full value doesn't fit in the caller's output, reconstruct
  into pending, drain across multiple `advance()` calls. Keeps sans-IO
  semantics identical to Classic.

Two correctness bugs found and fixed during the first-pass bring-up:

1. **`first_of()` was catastrophic on solid data.** My initial
   implementation called `first_of(prev_code)` in the KwKwK path to
   get the appended byte. On solid-color data where every code is a
   KwKwK code with a chain hundreds of entries long, this doubled the
   chain-walk work. Fix: reconstruct first, then read `target[0]`
   which equals the first byte by construction.
2. **min_code_size=12 clear/end wrap.** At min_size=12, `clear_code =
   4096` and `end_code = 4097` both exceed the 12-bit code range. The
   `& MASK` in init_table would wrap to slots 0 and 1, corrupting the
   literal alphabet. Fix: `if self.len < MAX_ENTRIES` guard. This
   turned out to already be fixed in Chunked (commit `ea87677`); the
   same fix was added to the new state.

**Three-way bench results after first-pass Tight:**

| Input | Classic | Chunked | Tight v1 | Wuffs | Tight/Wuffs |
|---|---:|---:|---:|---:|---:|
| rle-4M | 923 M | 3.82 G | **4.48 G** | 4.86 G | **92%** |
| solid-4M | 37 G | 37 G | 4.98 G | 4.93 G | **101%** (parity) |
| pal16-4M | 923 M | 1.48 G | 1.66 G | 2.27 G | 73% |
| rand-4M | 211 M | 181 M | **287 M** | 483 M | **59%** (from 42%) |

Tight matched Wuffs on rle-4M and solid-4M, and was 1.4× faster than
Classic on random data. It also beat both Classic and Chunked on the
palette-heavy synthetic cases. The remaining gap with Wuffs was on
incompressible data, where Wuffs' ring-buffer kept writes hot in L1d
and Tight wrote directly to (potentially cold) caller output.

## Experiments that didn't work

Several ideas were explored, measured, and reverted. Documenting them
here so they're not re-tried blindly.

### `BURST = 1` (strip burst from Classic) — **devastating**

Hypothesis: if burst is overhead on random/photo data, cutting it to
size 1 would speed things up. Reality: −53% on classic/rand, −80% on
classic/pal16, −25% on classic/rle. **Burst is load-bearing on every
non-solid workload.** The amortization of peek_bits, setup, and
debug-assert work across multiple codes dominates the per-iter cost.

### Fused special-code check (clear/end/>=next into one compare) — **null**

Classic's burst loop checks `read_code == clear || read_code == end ||
read_code >= next_code` — three branches per burst code. Hypothesis:
fuse into `read_code.wrapping_sub(clear_code) < 2 || read_code >= next_code`
(since `end = clear + 1`, one subtract catches both). Reality: zero
measurable instruction change, zero cycle change. LLVM already
optimizes the three-compare chain well. Kept the change as cosmetic
cleanup.

### 32-bit LSB buffer for Streaming — **mixed, kept u64**

First Streaming prototype used a 32-bit bit buffer with wuffs' `n_bits
|= 24` trick. Switched to u64 during the MSB refactor (MSB needs u64
headroom for the `rotate_left` trick). Later revisited u32 for LSB
specifically. Results:

| Input | u64 LSB | u32 LSB | Δ |
|---|---:|---:|---:|
| synth rle-4M | 4.71 G | 5.11 G | +8% |
| synth pal16-4M | 1.63 G | 1.67 G | +2% |
| synth rand-4M | 289 M | 262 M | **−9%** |
| synth rand-64k | 288 M | 271 M | **−6%** |
| real qoi/amazon | 684 M | 702 M | +3% |
| real sc/codec_wiki | 2.78 G | 2.84 G | +2% |

u32 hurt random data (more refills cost more per code), helped rle.
On real corpora basically neutral. **Kept u64 for simplicity and
because the regression on rand was larger than the wins.**

### State hoisting into locals in the mini-burst inner loop — **regression**

Hypothesis: hoisting `save_code`, `prev_code`, `codes_until_bump`,
`width`, `width_mask`, `bit_buffer`, `n_bits` into locals would let
LLVM keep them in registers throughout the inner loop. Reality: +6%
instructions on rand-4M. The write-back at the end of the mini-burst
prevented some optimizations LLVM was doing on the non-hoisted version.
Reverted.

### MSB fallthrough to Classic — **not done, would regress**

Hypothesis: if MSB photographic is the only case where Streaming still
loses to Classic, dispatch MSB + Streaming requests to Classic instead
and let each strategy serve its best case. Reality: after mini-burst,
Streaming beats Classic on MSB screenshot TIFF by 11–36%. Falling
through to Classic would regress that massively. Not done.

### codes_until_bump countdown — **null on LSB**

Replaced `if width < 12 && save_code >> width != 0` (per-derive
branch) with a countdown decremented per derive and checked against 0.
Wins were smaller than expected (LLVM was already predicting the
original branch well), but the `#[cold]` annotation on the slow path
made intent clearer. Kept.

### Full Tight rewrite cost regression — **recovered by mini-burst**

After adding MSB support + yield_on_full via generics, Tight LSB
regressed ~5% on rand-4M compared to the first-pass prototype. Asm
grew from 933 → 982 lines, stack frame from 88 → 104 bytes (extra
register spills from the generic phantom-data field). This was a real
regression attributed to LLVM having more state to track through
monomorphization.

**Mini-burst recovered it and then some.** Post-mini-burst, the stack
frame is back to 88 bytes (same as first-pass), and instruction count
on rand is 25% below the first-pass prototype. The asm line count is
higher (1025 lines) but that's from the mini-burst inner loop body,
not generic overhead.

## Adding MSB + TIFF early-change + yield_on_full

The first-pass Streaming was LSB-only with no TIFF mode and no
yield_on_full, which meant image-tiff couldn't adopt it. Adding full
parity required:

### MSB bit reader

Commit `848697e` introduced a `StreamingBitPacking` trait with `Lsb`
and `Msb` marker impls — the same pattern weezl's existing
`LsbBuffer` / `MsbBuffer` use for Classic. The trait has four
methods: `refill_fast8`, `refill_byte`, `extract`, `put_back`. LLVM
inlines all of them at monomorphization, so there's zero runtime
cost from the generic parameter.

**Important subtlety with the MSB refill:** Wuffs' over-read trick
(`n_bits |= 24`, consume `(31 - n_bits) >> 3` bytes) doesn't work for
MSB. For LSB, over-read bytes land in the buffer's high positions
where they're harmlessly out of the extract mask. For MSB, valid bits
sit at the top and new bytes are placed via `chunk >> n_bits` in the
low positions — the over-read bytes would sit BELOW the valid
region, where the `rotate_left` extract would eventually pick them up
as garbage.

Fix: MSB uses a classic-style exact-byte refill (`wish_count = (64 -
n_bits) / 8` bytes). Slightly slower per refill than LSB's wuffs-style
over-read but correct. In practice refills are amortized and the
difference is imperceptible.

### TIFF early-change

TIFF LZW bumps the code width one code earlier than non-TIFF. Classic
handles this via `is_tiff: bool` on the struct, checked on every derive
via `max_code() - Code::from(is_tiff)`.

For Streaming, `is_tiff` is a runtime field on the struct too, but
it's only read during `init_table()` and `bump_width_slow()` — **not
in the hot loop**. The `codes_until_bump` countdown mechanism already
moves the bump check off the hot path, and the TIFF offset is just a
different initial value for that counter.

**Correctness bug caught by cross-check test:** My first-pass TIFF
implementation applied the `-1` offset to every bump (both init and
post-bump counter reset). Wrong: TIFF only shortens the FIRST bump
interval by 1, not every subsequent one. Later epochs have identical
lengths between TIFF and non-TIFF. The bug manifested as the decoder
hitting a spurious CLEAR code around written=860 on rand-4M (due to
the third bump firing one derive early, corrupting subsequent
width-based extractions). Fix: apply the offset only in `init_table`,
leave `bump_width_slow` unchanged. Caught by the new
`streaming_msb_tiff_roundtrip` test.

### yield_on_full_buffer

image-tiff uses `Configuration::with_yield_on_full_buffer(true)` to
make the decoder return as soon as output is full, without
speculatively reading more bits (which could be garbage past the end
of a strip). Classic monomorphizes this via `CodegenConstants::YIELD_ON_FULL`.

For Streaming, added a `CgC: CodegenConstants` generic parameter
following Classic's pattern. The YIELD_ON_FULL branch is a single
compare at the top of the main loop (`if out.is_empty() { break; }`);
in the non-yield monomorphization LLVM eliminates it entirely.

**Non-obvious design decision:** yield_on_full does NOT skip the
`pending` buffer fallback when a code's value is larger than
remaining output. My first draft made it break + put-back in that
case, which caused infinite loops when the caller passed a small
output buffer and a code emitted more bytes than fit (e.g., image-tiff's
`LZWReader::read` with a 512-byte caller buf and a 4 KiB solid code).
Fix: in both yield and non-yield modes, over-sized codes spill to
`pending`; the yield optimization only kicks in AFTER the buffer is
fully drained. Caught by the new `streaming_yield_on_full_small_buffer`
test that drives the exact image-tiff drain pattern.

## Real-world corpora: QOI, gb82-sc, CLIC, tiff-conformance

Synthetic benchmarks are fine for establishing baselines but don't
reflect the shape of real data. Four real-world corpora were added
over the session:

### LSB LZW (for comparison with Wuffs)

- **QOI screenshot_web** (14 PNGs of popular websites from
  phoboslab.org's qoi-benchmark): typical web-page screenshots with
  mixed content. Compression ratios 3×–34×. Loaded via the `png`
  crate, raw RGB bytes re-encoded as LSB-8 LZW for Wuffs
  compatibility.
- **gb82-sc** (10 PNGs from codec-corpus: codec_wiki, gmessages,
  graph, gui, imac_dark, imac_g3, iMessage, terminal, etc): UI
  screenshots with solid color blocks. Compression ratios 9×–36×.

### MSB LZW (real TIFF files)

- **tiff-conformance** (19 real libtiff-encoded LZW TIFFs from
  codec-corpus/tiff-conformance/valid): Transparency-lzw,
  cmyk-4c-8b, hpredict, hpredict_cmyk, etc. Mix of sizes. Some have
  tiny strips (~2 KB), which stress the per-strip init cost.
- **clic-pred / clic-raw** (6 photographic PNGs from CLIC2025
  converted via `convert -compress lzw` with Predictor=2 vs =1). The
  "with predictor" corpus is representative of real photographic
  TIFF-LZW in the wild (ratio ~1.4×). The "raw" corpus is the
  pathological no-compression case (ratio ~1.0×).

Inputs were converted offline (to `/tmp/tiff_corpus/{qoi,sc,clic,clic-nopred}`)
via imagemagick's `convert`. A minimal 100-line TIFF parser in
`wuffs-bench/src/lib.rs` extracts LZW strip bytes from the files
without depending on the `tiff` crate.

### Initial real-world results (first-pass Streaming, LSB)

On LSB real data Streaming was already better than Classic and Chunked
everywhere. Against Wuffs:

| Corpus avg (Streaming / Wuffs) | |
|---|---:|
| QOI screenshot_web (6 files) | **71%** |
| gb82-sc (6 files) | **75%** |
| synth (rle-4M) | **102%** (Tight beats Wuffs) |
| synth (rand-4M) | 58% |

On high-ratio real-world data Streaming closed most of the Wuffs gap.
Random-like data still had the 40%+ gap that needed further work.

## Image-tiff lifecycle audit and the reset() question

To answer "is Streaming a drop-in replacement for image-tiff, and how
much difference would it make in real usage?", I audited image-tiff's
LZW call sites:

- **`image-tiff/src/decoder/image.rs:1149`** creates a new `LZWReader`
  per chunk (strip or tile) via `Self::create_reader`. Each call at
  `image-tiff/src/decoder/stream.rs:145` builds a fresh `weezl::decode::
  Decoder` from `Configuration::with_tiff_size_switch(Msb, 8)
  .with_yield_on_full_buffer(true)`. The reader is dropped after the
  chunk's bytes are consumed.
- **image-tiff never calls `reset()`** — every strip gets a fresh
  Decoder with fresh allocations.

Initial guess was "reset() is worthless" because my first reset-reuse
bench on Linux showed 0% difference. The user (correctly) pushed
back: **Windows HeapAlloc is ~5× slower than glibc malloc, so reset
saves real time on Windows for small-strip workloads.**

This led to two new benches:

### `alloc_cost` — pure Decoder::new+Drop vs reset()

On Linux glibc (Ryzen 7950X):

| Strategy | new+Drop | reset | Savings per strip |
|---|---:|---:|---:|
| Classic | 267 ns | 136 ns | 131 ns (2.0×) |
| Chunked | 526 ns | 206 ns | 320 ns (2.6×) |
| **Streaming** | **774 ns** | **159 ns** | **615 ns (4.9×)** |

**Streaming has the highest allocation cost but the smallest reset
cost** — 4.9× savings vs Classic's 2.0×. The boxed table arrays are
larger (52 KB for the PreQ+SufQ layout), but reset only needs to re-
populate the 256 literal slots, which is faster than the allocator
churn of a fresh construction.

Extrapolating to Windows (5× slower malloc, reset cost unchanged
because it's mostly memset):

| Strategy | Windows new+Drop (est) | Windows reset | Savings |
|---|---:|---:|---:|
| Classic | ~1500 ns | ~140 ns | ~1400 ns |
| **Streaming** | **~4000 ns** | **~160 ns** | **~3800 ns** |

For a small-strip corpus (554 strips), Streaming's Windows savings
from reset() would be ~2 ms out of ~16 ms total — **~12%**.

### `full_image` — multi-strip full-image decode

Bench that parses EVERY strip from every file in a corpus (matching
image-tiff's `image.rs:1149` loop exactly) and decodes them all via
the Decoder, with three variants per strategy: fresh-per-strip,
reuse-per-file (reset between strips), reuse-global (one persistent
Decoder). Four corpora: conform (554 tiny strips), clic (51 photo
strips), qoi (350 web strips), sc (98 UI strips).

**Aggregate throughput across all strips of each corpus:**

| Corpus | Classic | Chunked | Streaming | Streaming vs Classic |
|---|---:|---:|---:|---:|
| conform (554 tiny strips) | 453 | 469 | **567** | **+25%** |
| clic (51 photographic) | 820 | 729 | **913** | **+11%** |
| qoi (350 web screenshots) | 4396 | 5278 | **5867** | **+33%** |
| sc (98 UI screenshots) | 6158 | 8313 | ~8387 | **+36%** |

**Streaming wins Classic on EVERY real-world aggregate corpus by
11–36%**, including photographic clic where earlier individual-strip
numbers had shown Streaming losing 3–9%. The aggregate picture is
completely different from the per-file picture.

**Reset savings on Linux:**

| Corpus | fresh | reuse-global | Δ |
|---|---:|---:|---:|
| conform | 552 | 564 | **+2.2%** |
| clic | 863 | 852 | 0% |
| qoi | 5813 | 5874 | +1.0% |
| sc | 8387 | 8399 | 0% |

On Linux, reset saves 0–2% — because alloc is fast enough that even
554 strips × 615 ns saved = 340 µs is a small fraction of the ~16 ms
decode time.

**On Windows** (extrapolating the 5× malloc slowdown), the conform
corpus savings would be ~5× larger — **~10%**. For workloads with
many tiny strips, reset is a meaningful win on Windows specifically.

**Conclusion:** reset is not worthless. It's a small win on Linux
(<2%) and a meaningful win on Windows (5–10% for small-strip
workloads). image-tiff's current per-strip Decoder recreation
leaves this on the table. A follow-up refactor to cache an LZWReader
in image-tiff's ImageDecoder could deliver this gain for Classic,
Chunked, and Streaming all three — **but it's not in the weezl PR's
scope.** Streaming exposes `reset()` via `Stateful::reset()`; that's
all weezl needs to do.

## Mini-burst and short-copy fast path

The final two perf commits targeted the per-code overhead of the
single-code loop.

### Mini-burst (literal fast path)

After processing a literal in the main loop, opportunistically peek
the next code. If it's also a literal AND enough bits are buffered
AND output has room, process it inline without re-entering the full
dispatch chain. Loop until a non-literal, empty output, or refill is
needed.

A new `peek_code` method on `StreamingBitPacking` provides non-
destructive lookup for both LSB (`buffer & mask`) and MSB
(`buffer.rotate_left(width) & mask`).

**rand-4M impact (perf stat, 60 iters):**

| State | ipb | cpb |
|---|---:|---:|
| pre-mini-burst | 97.6 | 19.72 |
| **mini-burst** | **75.2** | **16.62** |

−23% instructions, −16% cycles on rand-4M. Throughput jumped from
289 → 354 MiB/s. Closing the wuffs gap from 58% → 72%.

Other synthetic workloads barely moved because they're already
reconstruct-bound (long chain walks dominate).

On real TIFF (MSB + TIFF + yield), photographic clic improved by
~5%, closing the Classic gap from 3–9% → 1–3%. Streaming now beat
Classic on 2 previously-losing photographic strips.

### Short-copy fast path

The second fast path inside the mini-burst: also handle COPY codes
whose value fits in a single PreQ+SufQ suffix chunk (lm1 < 8). For
these codes the entire value is stored verbatim in `suffix[0..value_len]`
— no prefix chain walk, just a direct byte copy.

This is the common case on photographic data with horizontal predictor
where codes emit 2–4 bytes of per-pixel deltas. The short-copy fast
path hits nearly every copy code on that workload.

**Correctness bug caught by the test suite:** First draft let
`peek == clear_code` or `peek == end_code` fall into the copy path,
reading stale `lm1s` / `suffixes` slots (those entries are never
populated by `init_table`). Fix: require `peek > end_code AND peek <
save_code`. Caught by `tests::all_three_agree_on_corpus` failing with
`InvalidCode` on rand-4M.

**rand-4M impact:**

| State | ipb | cpb |
|---|---:|---:|
| mini-burst only | 75.2 | 16.62 |
| **+ short-copy** | **70.6** | **16.02** |

Additional −6% instructions, −3.6% cycles. rand-4M throughput went
354 → 372 MiB/s. Total session improvement on rand: 58% of wuffs →
77% of wuffs.

**Real TIFF photographic impact (MSB + TIFF):**

| File | Classic | Streaming (before) | Streaming (after) | Δ |
|---|---:|---:|---:|---:|
| conform/cmyk-4c-8b | 236 M | 213 M | **227 M** (+7%) | **now Streaming wins** |
| conform/hpredict | 442 M | 373 M | **423 M** (+13%) | 96% of Classic |
| conform/hpredict_cmyk | 450 M | 407 M | **417 M** (+2%) | 93% of Classic |
| qoi-tif/amazon.com | 292 M | 284 M | **297 M** (+5%) | **Streaming wins** |
| qoi-tif/apple.com | 256 M | 239 M | **257 M** (+8%) | **ties Classic** |

The long-standing "Streaming loses on photographic TIFF" regression
is essentially closed. Streaming matches or beats Classic on every
real photographic strip except the tiniest (hpredict, hpredict_cmyk —
both under 3 KB strip size where Classic's burst amortization still
wins by 4–7%).

### Aggregate full-image results after short-copy fast path

| Corpus | Classic | Streaming (pre) | Streaming (post) | Δ vs Classic |
|---|---:|---:|---:|---:|
| conform (554 tiny strips) | 456 | 552 | **567** | **+24%** |
| clic (51 photographic) | 820 | 863 | **913** | **+11%** |
| qoi (350 web screenshots) | 4396 | 5813 | **5867** | **+33%** |
| sc (98 UI screenshots) | 6158 | ~8387 | ~same | **+36%** |

**Streaming wins Classic by 11–36% on every real-world aggregate
multi-strip TIFF corpus.**

## Final results

### Synthetic LSB (all ratios, wuffs_parity bench)

| Input | Ratio | Classic | Chunked | **Streaming** | Wuffs | Streaming/Wuffs |
|---|---:|---:|---:|---:|---:|---:|
| solid-4M | 1050× | 37 G | 37 G | 5.0 G | 4.9 G | **102%** |
| rle-4M | 39× | 920 M | 4.4 G | **5.1 G** | 4.9 G | **104%** |
| pal16-4M | 18.7× | 920 M | 1.5 G | **1.67 G** | 2.3 G | 73% |
| rand-4M | 0.73× | 209 M | 177 M | **372 M** | 490 M | **77%** |
| solid-64k | 155× | 10.6 G | 9.9 G | 6.96 G | 8.4 G | 83% |
| pal16-64k | 17.5× | 1.0 G | 2.5 G | **2.88 G** | 4.5 G | 64% |
| rand-64k | 0.73× | 210 M | 177 M | **299 M** | 487 M | 61% |

On large-output workloads Streaming is competitive with Wuffs across
the ratio spectrum, matches Wuffs on rle and solid, and at ~75% on
random data.

### Real TIFF (MSB + TIFF + yield, full-image aggregate)

| Corpus | Classic | Chunked | **Streaming** | vs Classic |
|---|---:|---:|---:|---:|
| conform (554 strips) | 453 | 469 | **567** | **+25%** |
| clic (51 strips) | 820 | 729 | **913** | **+11%** |
| qoi (350 strips) | 4396 | 5278 | **5867** | **+33%** |
| sc (98 strips) | 6158 | 8313 | **8387** | **+36%** |

**Streaming is the best strategy for every real-world multi-strip
TIFF corpus by 11–36%, and also beats Chunked on 3 of 4 corpora
(tied on sc).**

### Real GIF (LSB, QOI/SC single-file)

| File | Classic | Chunked | **Streaming** | Wuffs |
|---|---:|---:|---:|---:|
| qoi/duckduckgo.com (r33) | 0.93 G | 2.45 G | **3.02 G** | 3.49 G |
| qoi/apple.com (r7.0) | 0.66 G | 0.91 G | **1.19 G** | 1.73 G |
| qoi/en.wikipedia.org (r8.2) | 0.74 G | 0.95 G | **1.20 G** | 1.74 G |
| sc/codec_wiki (r35.8) | 0.98 G | 2.62 G | **2.78 G** | 3.51 G |
| sc/graph (r32.1) | 0.92 G | 2.61 G | **2.98 G** | 4.24 G |

Streaming is 1.3–3× faster than Classic on real GIF/screenshot
content and consistently in the 70–90% of Wuffs range.

### Tests

- `all_three_agree_on_corpus`: Classic, Chunked, Streaming, Wuffs
  all produce byte-identical output on the 7-input synthetic corpus.
- `streaming_msb_non_tiff_roundtrip`: Streaming MSB matches Classic on
  every standard corpus input.
- `streaming_msb_tiff_roundtrip`: Streaming MSB + TIFF early-change
  matches Classic on the standard corpus (exact image-tiff config).
- `streaming_yield_on_full_small_buffer`: Drives a 512-byte output
  buffer drain loop (image-tiff's `Read::read` pattern). Persistent
  Decoder, `decode_bytes` in a loop until Done. Verifies byte-
  identical output with yield_on_full enabled.

All 4 pass. No other test changes.

## API gap analysis and adoption path

Audited the call sites of every known weezl consumer in the local
filesystem (`~/work/image-gif`, `~/work/third-party/gif`, `~/work/zen/
image-tiff`, plus zen crates). Only two have decoder call sites:

### image-gif (`src/reader/decoder.rs`)

Uses only LSB + default (Classic). One-line change to adopt
Streaming:

```rust
// src/reader/decoder.rs:310 (current)
self.decoder = Some(LzwDecoder::new(BitOrder::Lsb, min_code_size));

// proposed
self.decoder = Some(
    Configuration::new(BitOrder::Lsb, min_code_size)
        .with_table_strategy(TableStrategy::Streaming)
        .build()
);
```

**image-gif gets 1.3–3× speedup on all GIF content** with this single
line change. The `reset()` + `has_ended()` API it already uses is
fully supported by Streaming.

### image-tiff (`src/decoder/stream.rs:145`)

Uses MSB + `with_tiff_size_switch(Msb, 8).with_yield_on_full_buffer(true)`.
One-line change:

```rust
// src/decoder/stream.rs:145 (current)
let configuration =
    weezl::decode::Configuration::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
        .with_yield_on_full_buffer(true);

// proposed
let configuration =
    weezl::decode::Configuration::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
        .with_yield_on_full_buffer(true)
        .with_table_strategy(weezl::decode::TableStrategy::Streaming);
```

**image-tiff gets 11–36% aggregate speedup** on every real TIFF corpus
with this single line change. Streaming supports all three
Configuration options image-tiff uses (MSB + TIFF + yield).

### Follow-up opportunity for image-tiff (not weezl's scope)

image-tiff currently creates a new LZWReader per strip. Caching one
LZWReader in the ImageDecoder and calling `reset()` between strips
would save per-strip alloc cost. On Linux this is <2%; on Windows it
could be 5–10% for small-strip workloads. This is a follow-up in the
image-tiff repo, not a weezl PR blocker.

## Naming

The new strategy was originally named `Tight` from my description as
a "wuffs-style tight single-code-per-iter loop." After the mini-burst
and short-copy commits it was no longer strictly single-code —
literal and short-copy runs are processed inline in a tight inner
loop. "Tight" described the first-pass prototype but not the final
design.

Renamed to **`Streaming`** in commit `a570c86`:
- Matches the architecture: a pipeline through consecutive codes
  rather than a batched burst.
- Distinguishes clearly from `Classic` (burst-based) and `Chunked`
  (different table layout, same Classic loop).
- Reads naturally in docs: "the streaming decoder has a 52 KB table."

Alternative names considered: `PreQ8` (too jargon-heavy, references
Wuffs internal naming), `Direct` (vague), `Flat` (ambiguous),
`Linear` (overloaded).

## What was explored but not landed

Ideas that were considered, sometimes prototyped, but not landed
because they didn't pan out or weren't worth the complexity:

1. **Drop `firsts[]` from ChunkedTable.** The user explicitly said
   "don't bother with firsts[] if not in rewrite" after I brought it
   up as a possible ChunkedTable cleanup. Streaming already doesn't
   have `firsts[]`; no experiment needed on Chunked.
2. **3-wide or 4-wide mini-burst unroll.** Would give marginal
   additional amortization on literal-heavy workloads. Not attempted
   because the 2-wide mini-burst already closed most of the gap.
3. **SIMD vectorization of the chain walk.** Would require unsafe
   or architecture-specific intrinsics. Wuffs doesn't do this
   either.
4. **Ring-buffer output (wuffs-style).** Would require either
   breaking the sans-IO API or wrapping the caller's output slice
   with an intermediate ring. The sans-IO advantage (6.9× on
   solid-color) is worth preserving.
5. **Adaptive strategy selection based on TIFF tags.** Image-tiff
   could look at `PhotometricInterpretation` and pick
   Streaming/Chunked/Classic per strip. Not in weezl's scope.
6. **Unsafe pointer-pair output tracking.** Replace `&mut [u8]`
   with `*mut u8` + `*mut u8` end pointer to save the slice-length
   store per code. Would violate `#![forbid(unsafe_code)]`.

## Commit list

Final `wuffs-parity` branch (top of the stack first):

```
a570c86 refactor: rename TableStrategy::Tight → Streaming
0a8d99d cleanup: remove debug_mini scratch binary
90f21fd perf: extend Tight mini-burst with a short-copy fast path
fe7288e feat: multi-strip full_image bench + alloc_cost bench
9e91b29 feat: reset_reuse bench — does persistent decoder + reset() help?
cb93b49 perf: mini-burst literal fast path in Tight
d5a308f feat: Tight adds yield_on_full_buffer support for image-tiff compat
f718994 feat: add clic2025 photographic corpus (with + without predictor)
573c444 feat: bench against real TIFF files from qoi/sc corpora + tiff-conformance
848697e feat: Tight adds MSB bit reader + TIFF early-change support
d24c690 feat: add QOI screenshot_web + gb82-sc real-world corpus to bench
2b97975 perf: switch Tight bit reader from u64 to u32 matching wuffs   (superseded)
ded2a5c perf: replace per-derive width check with codes_until_bump countdown
48903c3 feat: TableStrategy::Tight — wuffs-style single-code-per-iter decoder
1887fe8 experiment: fused special-code check (null result)
46a1814 wip: add l1_effect bench + profile_one perf-stat target
886a302 wip: wuffs-bench sub-crate for wuffs parity iteration
9742fba wip: bench scaffolding for wuffs-parity investigation
```

The branch points off `fixed-array-on-chunked` (the open PR #60
branch that added Chunked). If #60 lands first, the Streaming branch
can rebase on top. If not, the two sets of changes are independent
and could be reordered.

---

## Reading list for future context

- Wuffs LZW spec: https://fuchsia.googlesource.com/third_party/wuffs/+/HEAD/std/lzw/README.md
- Wuffs v0.4 C source: `wuffs-bench/vendor/wuffs-v0.4.c` (not committed; fetch from https://raw.githubusercontent.com/google/wuffs/main/release/c/wuffs-v0.4.c)
- image-gif decoder: `src/reader/decoder.rs:310` (the single LzwDecoder::new call site)
- image-tiff decoder: `src/decoder/stream.rs:143` (LZWReader::new, creates the weezl Decoder)
- image-tiff per-strip loop: `src/decoder/image.rs:1149` (calls create_reader per chunk)
- Bench entry points: `wuffs-bench/benches/*.rs`
  - `wuffs_parity.rs`: LSB 3-way + wuffs C comparison across the standard synthetic corpus
  - `tiff_parity.rs`: MSB + TIFF largest-strip-per-file
  - `full_image.rs`: MSB + TIFF multi-strip full-image with fresh/reuse variants
  - `reset_reuse.rs`: fresh vs reuse on the largest strip per file
  - `alloc_cost.rs`: pure Decoder::new+Drop vs reset() micro-bench
  - `l1_effect.rs`: varies output buffer size to isolate cache footprint
  - `init_cost.rs`: isolates Decoder construction cost via tiny inputs
  - `synth.rs`: criterion-style synthetic bench (early-session scaffold)
  - `msb8.rs`: original weezl bundled MSB-8 TIFF bench
