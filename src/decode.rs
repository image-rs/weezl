//! A module for all decoding needs.
#[cfg(feature = "std")]
use crate::error::StreamResult;
use crate::error::{BufferResult, LzwError, LzwStatus, VectorResult};
use crate::{BitOrder, Code, StreamBuf, MAX_CODESIZE, MAX_ENTRIES, STREAM_BUF_SIZE};

use crate::alloc::{boxed::Box, vec, vec::Vec};
#[cfg(feature = "std")]
use std::io::{self, BufRead, Write};

/// The state for decoding data with an LZW algorithm.
///
/// The same structure can be utilized with streams as well as your own buffers and driver logic.
/// It may even be possible to mix them if you are sufficiently careful not to lose or skip any
/// already decode data in the process.
///
/// This is a sans-IO implementation, meaning that it only contains the state of the decoder and
/// the caller will provide buffers for input and output data when calling the basic
/// [`decode_bytes`] method. Nevertheless, a number of _adapters_ are provided in the `into_*`
/// methods for decoding with a particular style of common IO.
///
/// * [`decode`] for decoding once without any IO-loop.
/// * [`into_async`] for decoding with the `futures` traits for asynchronous IO.
/// * [`into_stream`] for decoding with the standard `io` traits.
/// * [`into_vec`] for in-memory decoding.
///
/// [`decode_bytes`]: #method.decode_bytes
/// [`decode`]: #method.decode
/// [`into_async`]: #method.into_async
/// [`into_stream`]: #method.into_stream
/// [`into_vec`]: #method.into_vec
pub struct Decoder {
    state: Box<dyn Stateful + Send + 'static>,
}

/// A decoding stream sink.
///
/// See [`Decoder::into_stream`] on how to create this type.
///
/// [`Decoder::into_stream`]: struct.Decoder.html#method.into_stream
#[cfg_attr(
    not(feature = "std"),
    deprecated = "This type is only useful with the `std` feature."
)]
#[cfg_attr(not(feature = "std"), allow(dead_code))]
pub struct IntoStream<'d, W> {
    decoder: &'d mut Decoder,
    writer: W,
    buffer: Option<StreamBuf<'d>>,
    default_size: usize,
}

/// An async decoding sink.
///
/// See [`Decoder::into_async`] on how to create this type.
///
/// [`Decoder::into_async`]: struct.Decoder.html#method.into_async
#[cfg(feature = "async")]
pub struct IntoAsync<'d, W> {
    decoder: &'d mut Decoder,
    writer: W,
    buffer: Option<StreamBuf<'d>>,
    default_size: usize,
}

/// A decoding sink into a vector.
///
/// See [`Decoder::into_vec`] on how to create this type.
///
/// [`Decoder::into_vec`]: struct.Decoder.html#method.into_vec
pub struct IntoVec<'d> {
    decoder: &'d mut Decoder,
    vector: &'d mut Vec<u8>,
}

trait Stateful {
    fn advance(&mut self, inp: &[u8], out: &mut [u8]) -> BufferResult;
    fn has_ended(&self) -> bool;
    /// Ignore an end code and continue decoding (no implied reset).
    fn restart(&mut self);
    /// Reset the decoder to the beginning, dropping all buffers etc.
    fn reset(&mut self);
}

#[derive(Clone, Copy, Default)]
struct Link {
    prev: Code,
    first: u8,
}

#[derive(Clone)]
struct DerivationBase {
    code: Code,
    first: u8,
}

#[derive(Default)]
struct MsbBuffer {
    /// A buffer of individual bits. The oldest code is kept in the high-order bits.
    bit_buffer: u64,
    /// A precomputed mask for this code.
    code_mask: u16,
    /// The current code size.
    code_size: u8,
    /// The number of bits in the buffer.
    bits: u8,
}

#[derive(Default)]
struct LsbBuffer {
    /// A buffer of individual bits. The oldest code is kept in the high-order bits.
    bit_buffer: u64,
    /// A precomputed mask for this code.
    code_mask: u16,
    /// The current code size.
    code_size: u8,
    /// The number of bits in the buffer.
    bits: u8,
}

trait CodeBuffer {
    fn new(min_size: u8) -> Self;
    fn reset(&mut self, min_size: u8);
    fn bump_code_size(&mut self);

    /// Retrieve the next symbol, refilling if necessary.
    fn next_symbol(&mut self, inp: &mut &[u8]) -> Option<Code>;
    /// Refill the internal buffer.
    fn refill_bits(&mut self, inp: &mut &[u8]);

    fn peek_bits(&self, code: &mut [Code; BURST]) -> usize;
    fn consume_bits(&mut self, code_cnt: u8);

    fn max_code(&self) -> Code;
    fn code_size(&self) -> u8;
}

pub(crate) trait CodegenConstants {
    const YIELD_ON_FULL: bool;
}

pub(crate) struct NoYield;
pub(crate) struct YieldOnFull;

impl CodegenConstants for NoYield {
    const YIELD_ON_FULL: bool = false;
}

impl CodegenConstants for YieldOnFull {
    const YIELD_ON_FULL: bool = true;
}

struct DecodeState<CodeBuffer, Constants: CodegenConstants> {
    /// The original minimum code size.
    min_size: u8,
    /// The table of decoded codes.
    table: Table,
    /// The buffer of decoded data.
    buffer: Buffer,
    /// The link which we are still decoding and its original code.
    last: Option<DerivationBase>,
    /// The next code entry.
    next_code: Code,
    /// Code to reset all tables.
    clear_code: Code,
    /// Code to signal the end of the stream.
    end_code: Code,
    /// A stored flag if the end code has already appeared.
    has_ended: bool,
    /// If tiff then bumps are a single code sooner.
    is_tiff: bool,
    /// Do we allow stream to start without an explicit reset code?
    implicit_reset: bool,
    /// The buffer for decoded words.
    code_buffer: CodeBuffer,
    #[allow(dead_code)]
    constants: core::marker::PhantomData<Constants>,
}

// We have a buffer of 64 bits. So at max size at most 5 units can be read at once without
// refilling the buffer. At smaller code sizes there are more. We tune for 6 here, by slight
// experimentation. This may be an architecture dependent constant.
const BURST: usize = 6;

struct Buffer {
    bytes: Box<[u8]>,
    read_mark: usize,
    write_mark: usize,
}

struct Table {
    suffixes: Box<[[u8; STREAMING_Q]; MAX_ENTRIES]>,
    chain: Box<[Link; MAX_ENTRIES]>,
    depths: Box<[u16; MAX_ENTRIES]>,
    len: usize,
}

const MASK: usize = MAX_ENTRIES - 1;

/// Strategy for the LZW decode table.
#[derive(Clone, Copy, Debug, Default)]
pub enum TableStrategy {
    /// Classic 6-wide burst decoder with compact 4-byte-per-entry table
    /// (24 KB). This is the default, matching weezl's existing behavior.
    #[default]
    Classic,
    /// Streaming single-code-per-iteration decoder with a PreQ+SufQ(Q=8)
    /// table and a mini-burst fast path for consecutive literals or
    /// short copies (value length ≤ 8). Inspired by the wuffs_lzw
    /// reference decoder's loop structure.
    ///
    /// Supports all Configuration options: LSB and MSB bit order, TIFF
    /// early-change, and `yield_on_full_buffer` — i.e., it is a drop-in
    /// replacement for the Classic strategy in every configuration.
    ///
    /// Recommended for all workloads. Beats Classic on every tested
    /// workload except solid single-byte data (pure KwKwK):
    ///
    ///   * Palette / GIF / screenshot data: 2–3× faster than Classic,
    ///     75–85% of wuffs_lzw throughput.
    ///   * Random / incompressible data: ~2× faster than Classic,
    ///     ~82% of wuffs_lzw throughput.
    ///   * Solid single-byte data: ~78% slower than Classic (Classic's
    ///     burst decoder does a single memcpy per max-length code).
    ///
    /// Table size: ~52 KB (PreQ+SufQ layout with Q=8).
    ///
    /// Per-decoder allocation cost is higher than Classic (~3× on Linux
    /// glibc) but the `reset()` path is fast (~160 ns), so callers that
    /// reuse a single decoder across many strips or frames pay the
    /// allocation cost only once. This matters most on Windows where
    /// `HeapAlloc` is ~5× slower than glibc malloc.
    Streaming,
}

/// Describes the static parameters for creating a decoder.
#[derive(Clone, Debug)]
pub struct Configuration {
    order: BitOrder,
    size: u8,
    tiff: bool,
    yield_on_full: bool,
    strategy: TableStrategy,
}

impl Configuration {
    /// Create a configuration to decode with the specified bit order and symbol size.
    pub fn new(order: BitOrder, size: u8) -> Self {
        super::assert_decode_size(size);
        Configuration {
            order,
            size,
            tiff: false,
            yield_on_full: false,
            strategy: TableStrategy::Classic,
        }
    }

    /// Create a configuration for a TIFF compatible decoder.
    pub fn with_tiff_size_switch(order: BitOrder, size: u8) -> Self {
        super::assert_decode_size(size);
        Configuration {
            order,
            size,
            tiff: true,
            yield_on_full: false,
            strategy: TableStrategy::Classic,
        }
    }

    /// Immediately yield to the caller when the decoder buffer is full.
    ///
    /// This can be used for `libtiff` compatibility. It will use a "relaxed" stream interpretation
    /// that need not contain an explicit EOF. Instead, the decoder is expected to stop fetching
    /// symbols when some out-of-band specified length of the decoded text has been reached. The
    /// caller indicates this maximum length through the available output buffer space.
    ///
    /// Symbols afterwards must not be expected to be valid. On filling the output buffer space
    /// completely, the decoder will return immediately to the caller instead of potentially
    /// interpreting the following bit-stream (and returning an error on doing so).
    ///
    /// Default: `false`.
    pub fn with_yield_on_full_buffer(self, do_yield: bool) -> Self {
        Configuration {
            yield_on_full: do_yield,
            ..self
        }
    }

    /// Select the decode table strategy.
    ///
    /// - [`TableStrategy::Classic`] (default): compact 4-byte-per-entry
    ///   table with a 6-wide burst decoder. Matches existing weezl
    ///   behavior.
    ///
    /// - [`TableStrategy::Streaming`]: single-code-per-iteration decoder
    ///   with a mini-burst literal/short-copy fast path. Beats Classic
    ///   on real-world TIFF and GIF aggregates by 3–35% across
    ///   photographic, screenshot, and palette data. Drop-in
    ///   replacement; supports every Configuration option.
    ///
    /// Default: [`TableStrategy::Classic`]. For new code,
    /// [`TableStrategy::Streaming`] is the recommended choice.
    pub fn with_table_strategy(self, strategy: TableStrategy) -> Self {
        Configuration { strategy, ..self }
    }

    /// Create a new decoder with the define configuration.
    pub fn build(self) -> Decoder {
        Decoder {
            state: Decoder::from_configuration(&self),
        }
    }
}

impl Decoder {
    /// Create a new decoder with the specified bit order and symbol size.
    ///
    /// The algorithm for dynamically increasing the code symbol bit width is compatible with the
    /// original specification. In particular you will need to specify an `Lsb` bit oder to decode
    /// the data portion of a compressed `gif` image.
    ///
    /// # Panics
    ///
    /// The `size` needs to be in the interval `0..=12`.
    pub fn new(order: BitOrder, size: u8) -> Self {
        Configuration::new(order, size).build()
    }

    /// Create a TIFF compatible decoder with the specified bit order and symbol size.
    ///
    /// The algorithm for dynamically increasing the code symbol bit width is compatible with the
    /// TIFF specification, which is a misinterpretation of the original algorithm for increasing
    /// the code size. It switches one symbol sooner.
    ///
    /// # Panics
    ///
    /// The `size` needs to be in the interval `0..=12`.
    pub fn with_tiff_size_switch(order: BitOrder, size: u8) -> Self {
        Configuration::with_tiff_size_switch(order, size).build()
    }

    fn from_configuration(configuration: &Configuration) -> Box<dyn Stateful + Send + 'static> {
        macro_rules! make_state {
            ($buf:ty, $cgc:ty) => {{
                let mut state = Box::new(DecodeState::<$buf, $cgc>::new(configuration.size));
                state.is_tiff = configuration.tiff;
                state as Box<dyn Stateful + Send + 'static>
            }};
        }

        match (
            configuration.order,
            configuration.yield_on_full,
            configuration.strategy,
        ) {
            (BitOrder::Lsb, false, TableStrategy::Classic) => {
                make_state!(LsbBuffer, NoYield)
            }
            (BitOrder::Lsb, true, TableStrategy::Classic) => {
                make_state!(LsbBuffer, YieldOnFull)
            }
            (BitOrder::Msb, false, TableStrategy::Classic) => {
                make_state!(MsbBuffer, NoYield)
            }
            (BitOrder::Msb, true, TableStrategy::Classic) => {
                make_state!(MsbBuffer, YieldOnFull)
            }
            (BitOrder::Lsb, false, TableStrategy::Streaming) => {
                let mut state = Box::new(DecodeStateStreaming::<StreamingLsb, NoYield>::new(
                    configuration.size,
                ));
                state.is_tiff = configuration.tiff;
                state.init_table();
                state.bump_if_lowbit();
                state as Box<dyn Stateful + Send + 'static>
            }
            (BitOrder::Lsb, true, TableStrategy::Streaming) => {
                let mut state = Box::new(DecodeStateStreaming::<StreamingLsb, YieldOnFull>::new(
                    configuration.size,
                ));
                state.is_tiff = configuration.tiff;
                state.init_table();
                state.bump_if_lowbit();
                state as Box<dyn Stateful + Send + 'static>
            }
            (BitOrder::Msb, false, TableStrategy::Streaming) => {
                let mut state = Box::new(DecodeStateStreaming::<StreamingMsb, NoYield>::new(
                    configuration.size,
                ));
                state.is_tiff = configuration.tiff;
                state.init_table();
                state.bump_if_lowbit();
                state as Box<dyn Stateful + Send + 'static>
            }
            (BitOrder::Msb, true, TableStrategy::Streaming) => {
                let mut state = Box::new(DecodeStateStreaming::<StreamingMsb, YieldOnFull>::new(
                    configuration.size,
                ));
                state.is_tiff = configuration.tiff;
                state.init_table();
                state.bump_if_lowbit();
                state as Box<dyn Stateful + Send + 'static>
            }
        }
    }

    /// Decode some bytes from `inp` and write result to `out`.
    ///
    /// This will consume a prefix of the input buffer and write decoded output into a prefix of
    /// the output buffer. See the respective fields of the return value for the count of consumed
    /// and written bytes. For the next call You should have adjusted the inputs accordingly.
    ///
    /// The call will try to decode and write as many bytes of output as available. It will be
    /// much more optimized (and avoid intermediate buffering) if it is allowed to write a large
    /// contiguous chunk at once.
    ///
    /// See [`into_stream`] for high-level functions (that are only available with the `std`
    /// feature).
    ///
    /// [`into_stream`]: #method.into_stream
    pub fn decode_bytes(&mut self, inp: &[u8], out: &mut [u8]) -> BufferResult {
        self.state.advance(inp, out)
    }

    /// Decode a single chunk of lzw encoded data.
    ///
    /// This method requires the data to contain an end marker, and returns an error otherwise.
    ///
    /// This is a convenience wrapper around [`into_vec`]. Use the `into_vec` adapter to customize
    /// buffer size, to supply an existing vector, to control whether an end marker is required, or
    /// to preserve partial data in the case of a decoding error.
    ///
    /// [`into_vec`]: #into_vec
    ///
    /// # Example
    ///
    /// ```
    /// use weezl::{BitOrder, decode::Decoder};
    ///
    /// // Encoded that was created with an encoder.
    /// let data = b"\x80\x04\x81\x94l\x1b\x06\xf0\xb0 \x1d\xc6\xf1\xc8l\x19 \x10";
    /// let decoded = Decoder::new(BitOrder::Msb, 9)
    ///     .decode(data)
    ///     .unwrap();
    /// assert_eq!(decoded, b"Hello, world");
    /// ```
    pub fn decode(&mut self, data: &[u8]) -> Result<Vec<u8>, LzwError> {
        let mut output = vec![];
        self.into_vec(&mut output).decode_all(data).status?;
        Ok(output)
    }

    /// Construct a decoder into a writer.
    #[cfg(feature = "std")]
    pub fn into_stream<W: Write>(&mut self, writer: W) -> IntoStream<'_, W> {
        IntoStream {
            decoder: self,
            writer,
            buffer: None,
            default_size: STREAM_BUF_SIZE,
        }
    }

    /// Construct a decoder into an async writer.
    #[cfg(feature = "async")]
    pub fn into_async<W: futures::io::AsyncWrite>(&mut self, writer: W) -> IntoAsync<'_, W> {
        IntoAsync {
            decoder: self,
            writer,
            buffer: None,
            default_size: STREAM_BUF_SIZE,
        }
    }

    /// Construct a decoder into a vector.
    ///
    /// All decoded data is appended and the vector is __not__ cleared.
    ///
    /// Compared to `into_stream` this interface allows a high-level access to decoding without
    /// requires the `std`-feature. Also, it can make full use of the extra buffer control that the
    /// special target exposes.
    pub fn into_vec<'lt>(&'lt mut self, vec: &'lt mut Vec<u8>) -> IntoVec<'lt> {
        IntoVec {
            decoder: self,
            vector: vec,
        }
    }

    /// Check if the decoding has finished.
    ///
    /// No more output is produced beyond the end code that marked the finish of the stream. The
    /// decoder may have read additional bytes, including padding bits beyond the last code word
    /// but also excess bytes provided.
    pub fn has_ended(&self) -> bool {
        self.state.has_ended()
    }

    /// Ignore an end code and continue.
    ///
    /// This will _not_ reset any of the inner code tables and not have the effect of a clear code.
    /// It will instead continue as if the end code had not been present. If no end code has
    /// occurred then this is a no-op.
    ///
    /// You can test if an end code has occurred with [`has_ended`](#method.has_ended).
    /// FIXME: clarify how this interacts with padding introduced after end code.
    #[allow(dead_code)]
    pub(crate) fn restart(&mut self) {
        self.state.restart();
    }

    /// Reset all internal state.
    ///
    /// This produce a decoder as if just constructed with `new` but taking slightly less work. In
    /// particular it will not deallocate any internal allocations. It will also avoid some
    /// duplicate setup work.
    pub fn reset(&mut self) {
        self.state.reset();
    }
}

#[cfg(feature = "std")]
impl<'d, W: Write> IntoStream<'d, W> {
    /// Decode data from a reader.
    ///
    /// This will read data until the stream is empty or an end marker is reached.
    pub fn decode(&mut self, read: impl BufRead) -> StreamResult {
        self.decode_part(read, false)
    }

    /// Decode data from a reader, requiring an end marker.
    pub fn decode_all(mut self, read: impl BufRead) -> StreamResult {
        self.decode_part(read, true)
    }

    /// Set the size of the intermediate decode buffer.
    ///
    /// A buffer of this size is allocated to hold one part of the decoded stream when no buffer is
    /// available and any decoding method is called. No buffer is allocated if `set_buffer` has
    /// been called. The buffer is reused.
    ///
    /// # Panics
    /// This method panics if `size` is `0`.
    pub fn set_buffer_size(&mut self, size: usize) {
        assert_ne!(size, 0, "Attempted to set empty buffer");
        self.default_size = size;
    }

    /// Use a particular buffer as an intermediate decode buffer.
    ///
    /// Calling this sets or replaces the buffer. When a buffer has been set then it is used
    /// instead of dynamically allocating a buffer. Note that the size of the buffer is critical
    /// for efficient decoding. Some optimization techniques require the buffer to hold one or more
    /// previous decoded words. There is also additional overhead from `write` calls each time the
    /// buffer has been filled.
    ///
    /// # Panics
    /// This method panics if the `buffer` is empty.
    pub fn set_buffer(&mut self, buffer: &'d mut [u8]) {
        assert_ne!(buffer.len(), 0, "Attempted to set empty buffer");
        self.buffer = Some(StreamBuf::Borrowed(buffer));
    }

    fn decode_part(&mut self, mut read: impl BufRead, must_finish: bool) -> StreamResult {
        let IntoStream {
            decoder,
            writer,
            buffer,
            default_size,
        } = self;

        enum Progress {
            Ok,
            Done,
        }

        let mut bytes_read = 0;
        let mut bytes_written = 0;

        // Converting to mutable refs to move into the `once` closure.
        let read_bytes = &mut bytes_read;
        let write_bytes = &mut bytes_written;

        let outbuf: &mut [u8] =
            match { buffer.get_or_insert_with(|| StreamBuf::Owned(vec![0u8; *default_size])) } {
                StreamBuf::Borrowed(slice) => &mut *slice,
                StreamBuf::Owned(vec) => &mut *vec,
            };
        assert!(!outbuf.is_empty());

        let once = move || {
            // Try to grab one buffer of input data.
            let data = read.fill_buf()?;

            // Decode as much of the buffer as fits.
            let result = decoder.decode_bytes(data, &mut outbuf[..]);
            // Do the bookkeeping and consume the buffer.
            *read_bytes += result.consumed_in;
            *write_bytes += result.consumed_out;
            read.consume(result.consumed_in);

            // Handle the status in the result.
            let done = result.status.map_err(|err| {
                io::Error::new(io::ErrorKind::InvalidData, &*format!("{:?}", err))
            })?;

            // Check if we had any new data at all.
            if let LzwStatus::NoProgress = done {
                debug_assert_eq!(
                    result.consumed_out, 0,
                    "No progress means we have not decoded any data"
                );
                // In particular we did not finish decoding.
                if must_finish {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "No more data but no end marker detected",
                    ));
                } else {
                    return Ok(Progress::Done);
                }
            }

            // And finish by writing our result.
            // TODO: we may lose data on error (also on status error above) which we might want to
            // deterministically handle so that we don't need to restart everything from scratch as
            // the only recovery strategy. Any changes welcome.
            writer.write_all(&outbuf[..result.consumed_out])?;

            Ok(if let LzwStatus::Done = done {
                Progress::Done
            } else {
                Progress::Ok
            })
        };

        // Decode chunks of input data until we're done.
        let status = core::iter::repeat_with(once)
            // scan+fuse can be replaced with map_while
            .scan((), |(), result| match result {
                Ok(Progress::Ok) => Some(Ok(())),
                Err(err) => Some(Err(err)),
                Ok(Progress::Done) => None,
            })
            .fuse()
            .collect();

        StreamResult {
            bytes_read,
            bytes_written,
            status,
        }
    }
}

impl IntoVec<'_> {
    /// Decode data from a slice.
    ///
    /// This will read data until the slice is empty or an end marker is reached.
    pub fn decode(&mut self, read: &[u8]) -> VectorResult {
        self.decode_part(read, false)
    }

    /// Decode data from a slice, requiring an end marker.
    pub fn decode_all(mut self, read: &[u8]) -> VectorResult {
        self.decode_part(read, true)
    }

    fn grab_buffer(&mut self) -> (&mut [u8], &mut Decoder) {
        const CHUNK_SIZE: usize = 1 << 12;
        let decoder = &mut self.decoder;
        let length = self.vector.len();

        // Use the vector to do overflow checks and w/e.
        self.vector.reserve(CHUNK_SIZE);
        // FIXME: decoding into uninit buffer?
        self.vector.resize(length + CHUNK_SIZE, 0u8);

        (&mut self.vector[length..], decoder)
    }

    fn decode_part(&mut self, part: &[u8], must_finish: bool) -> VectorResult {
        let mut result = VectorResult {
            consumed_in: 0,
            consumed_out: 0,
            status: Ok(LzwStatus::Ok),
        };

        enum Progress {
            Ok,
            Done,
        }

        // Converting to mutable refs to move into the `once` closure.
        let read_bytes = &mut result.consumed_in;
        let write_bytes = &mut result.consumed_out;
        let mut data = part;

        // A 64 MB buffer is quite large but should get alloc_zeroed.
        // Note that the decoded size can be up to quadratic in code block.
        let once = move || {
            // Grab a new output buffer.
            let (outbuf, decoder) = self.grab_buffer();

            // Decode as much of the buffer as fits.
            let result = decoder.decode_bytes(data, &mut outbuf[..]);
            // Do the bookkeeping and consume the buffer.
            *read_bytes += result.consumed_in;
            *write_bytes += result.consumed_out;
            data = &data[result.consumed_in..];

            let unfilled = outbuf.len() - result.consumed_out;
            let filled = self.vector.len() - unfilled;
            self.vector.truncate(filled);

            // Handle the status in the result.
            match result.status {
                Err(err) => Err(err),
                Ok(LzwStatus::NoProgress) if must_finish => Err(LzwError::InvalidCode),
                Ok(LzwStatus::NoProgress) | Ok(LzwStatus::Done) => Ok(Progress::Done),
                Ok(LzwStatus::Ok) => Ok(Progress::Ok),
            }
        };

        // Decode chunks of input data until we're done.
        let status: Result<(), _> = core::iter::repeat_with(once)
            // scan+fuse can be replaced with map_while
            .scan((), |(), result| match result {
                Ok(Progress::Ok) => Some(Ok(())),
                Err(err) => Some(Err(err)),
                Ok(Progress::Done) => None,
            })
            .fuse()
            .collect();

        if let Err(err) = status {
            result.status = Err(err);
        }

        result
    }
}

// This is implemented in a separate file, so that 1.34.2 does not parse it. Otherwise, it would
// trip over the usage of await, which is a reserved keyword in that edition/version. It only
// contains an impl block.
#[cfg(feature = "async")]
#[path = "decode_into_async.rs"]
mod impl_decode_into_async;

impl<C: CodeBuffer, CgC: CodegenConstants> DecodeState<C, CgC> {
    fn new(min_size: u8) -> Self {
        DecodeState {
            min_size,
            table: Table::new(),
            buffer: Buffer::new(),
            last: None,
            clear_code: 1 << min_size,
            end_code: (1 << min_size) + 1,
            next_code: (1 << min_size) + 2,
            has_ended: false,
            is_tiff: false,
            implicit_reset: true,
            code_buffer: CodeBuffer::new(min_size),
            constants: core::marker::PhantomData,
        }
    }

    fn init_tables(&mut self) {
        self.code_buffer.reset(self.min_size);
        self.next_code = (1 << self.min_size) + 2;
        self.table.init(self.min_size);
    }

    fn reset_tables(&mut self) {
        self.code_buffer.reset(self.min_size);
        self.next_code = (1 << self.min_size) + 2;
        self.table.clear(self.min_size);
    }
}

impl<C: CodeBuffer, CgC: CodegenConstants> Stateful for DecodeState<C, CgC> {
    fn has_ended(&self) -> bool {
        self.has_ended
    }

    fn restart(&mut self) {
        self.has_ended = false;
    }

    fn reset(&mut self) {
        self.table.init(self.min_size);
        self.next_code = (1 << self.min_size) + 2;
        self.buffer.read_mark = 0;
        self.buffer.write_mark = 0;
        self.last = None;
        self.restart();
        self.code_buffer = CodeBuffer::new(self.min_size);
    }

    fn advance(&mut self, mut inp: &[u8], mut out: &mut [u8]) -> BufferResult {
        // Skip everything if there is nothing to do.
        if self.has_ended {
            return BufferResult {
                consumed_in: 0,
                consumed_out: 0,
                status: Ok(LzwStatus::Done),
            };
        }

        // Rough description:
        // We will fill the output slice as much as possible until either there is no more symbols
        // to decode or an end code has been reached. This requires an internal buffer to hold a
        // potential tail of the word corresponding to the last symbol. This tail will then be
        // decoded first before continuing with the regular decoding. The same buffer is required
        // to persist some symbol state across calls.
        //
        // We store the words corresponding to code symbols in an index chain, bytewise, where we
        // push each decoded symbol. (TODO: wuffs shows some success with 8-byte units). This chain
        // is traversed for each symbol when it is decoded and bytes are placed directly into the
        // output slice. In the special case (new_code == next_code) we use an existing decoded
        // version that is present in either the out bytes of this call or in buffer to copy the
        // repeated prefix slice.
        // TODO: I played with a 'decoding cache' to remember the position of long symbols and
        // avoid traversing the chain, doing a copy of memory instead. It did however not lead to
        // a serious improvement. It's just unlikely to both have a long symbol and have that
        // repeated twice in the same output buffer.
        //
        // You will also find the (to my knowledge novel) concept of a _decoding burst_ which
        // gained some >~10% speedup in tests. This is motivated by wanting to use out-of-order
        // execution as much as possible and for this reason have the least possible stress on
        // branch prediction. Our decoding table already gives us a lookahead on symbol lengths but
        // only for re-used codes, not novel ones. This lookahead also makes the loop termination
        // when restoring each byte of the code word perfectly predictable! So a burst is a chunk
        // of code words which are all independent of each other, have known lengths _and_ are
        // guaranteed to fit into the out slice without requiring a buffer. One burst can be
        // decoded in an extremely tight loop.
        //
        // TODO: since words can be at most (1 << MAX_CODESIZE) = 4096 bytes long we could avoid
        // that intermediate buffer at the expense of not always filling the output buffer
        // completely. Alternatively we might follow its chain of precursor states twice. This may
        // be even cheaper if we store more than one byte per link so it really should be
        // evaluated.
        // TODO: if the caller was required to provide the previous last word we could also avoid
        // the buffer for cases where we need it to restore the next code! This could be built
        // backwards compatible by only doing it after an opt-in call that enables the behaviour.

        // Record initial lengths for the result that is returned.
        let o_in = inp.len();
        let o_out = out.len();

        // The code_link is the previously decoded symbol.
        // It's used to link the new code back to its predecessor.
        let mut code_link = None;
        // The status, which is written to on an invalid code.
        let mut status = Ok(LzwStatus::Ok);

        match self.last.take() {
            // No last state? This is the first code after a reset?
            None => {
                match self.next_symbol(&mut inp) {
                    // Plainly invalid code.
                    Some(code) if code > self.next_code => status = Err(LzwError::InvalidCode),
                    // next_code would require an actual predecessor.
                    Some(code) if code == self.next_code => status = Err(LzwError::InvalidCode),
                    // No more symbols available and nothing decoded yet.
                    // Assume that we didn't make progress, this may get reset to Done if we read
                    // some bytes from the input.
                    None => status = Ok(LzwStatus::NoProgress),
                    // Handle a valid code.
                    Some(init_code) => {
                        if init_code == self.clear_code {
                            self.init_tables();
                        } else if init_code == self.end_code {
                            self.has_ended = true;
                            status = Ok(LzwStatus::Done);
                        } else if self.table.is_empty() {
                            if self.implicit_reset {
                                self.init_tables();

                                self.buffer.fill_reconstruct(&self.table, init_code);
                                code_link = Some(DerivationBase {
                                    code: init_code,
                                    first: self.table.first_of(init_code),
                                });
                            } else {
                                // We require an explicit reset.
                                status = Err(LzwError::InvalidCode);
                            }
                        } else {
                            // Reconstruct the first code in the buffer.
                            self.buffer.fill_reconstruct(&self.table, init_code);
                            code_link = Some(DerivationBase {
                                code: init_code,
                                first: self.table.first_of(init_code),
                            });
                        }
                    }
                }
            }
            // Move the tracking state to the stack.
            Some(tup) => code_link = Some(tup),
        };

        // Track an empty `burst` (see below) means we made no progress.
        let mut have_yet_to_decode_data = false;

        // Restore the previous state, if any.
        if code_link.is_some() {
            let remain = self.buffer.buffer();
            // Check if we can fully finish the buffer.
            if remain.len() > out.len() {
                if out.is_empty() {
                    // This also implies the buffer is _not_ empty and we will not enter any
                    // decoding loop.
                    status = Ok(LzwStatus::NoProgress);
                } else {
                    out.copy_from_slice(&remain[..out.len()]);
                    self.buffer.consume(out.len());
                    out = &mut [];
                }
            } else if remain.is_empty() {
                status = Ok(LzwStatus::NoProgress);
                have_yet_to_decode_data = true;
            } else {
                let consumed = remain.len();
                out[..consumed].copy_from_slice(remain);
                self.buffer.consume(consumed);
                out = &mut out[consumed..];
                have_yet_to_decode_data = false;
            }
        }

        // A special reference to out slice which holds the last decoded symbol.
        let mut last_decoded: Option<&[u8]> = None;

        if self.buffer.buffer().is_empty() {
            // Hot loop that writes data to the output as long as we can do so directly from the
            // input stream. As an invariant of this block we did not need to use the buffer to
            // store a decoded code word. Testing the condition ahead of time avoids a test in the
            // loop body since every code path where the buffer is filled already breaks.
            //
            // In a previous iteration of the code we trusted compiler optimization to work this
            // out but it seems that it does not. Another edit hidden behind some performance work
            // then edited out the check, inadvertently changing the behavior for callers that
            // relied on being able to provide an empty output buffer and still receiving a useful
            // signal about the state of the stream.

            // A burst is a sequence of code words that are independently decoded, i.e. they do not
            // change the state of the decoder in ways that would influence the interpretation of
            // each other. That is: they are not special symbols, they do not make us increase the
            // code size, they are each codes already in the tree before the burst.
            //
            // The tracking state for a burst. These are actually initialized later but compiler
            // wasn't smart enough to fully optimize out the init code so that appears outside the
            // loop.
            let mut burst = [0; BURST];
            let mut burst_byte_len = [0u16; BURST];
            let mut burst_byte = [0u8; BURST];
            let mut target: [&mut [u8]; BURST] = Default::default();

            loop {
                // In particular, we *also* break if the output buffer is still empty. Especially
                // when the output parameter was an empty slice, we must try to fetch at least one
                // code but with YIELD_ON_FULL we do not.
                if CgC::YIELD_ON_FULL && out.is_empty() {
                    break;
                }

                let mut deriv = match code_link.take() {
                    Some(link) => link,
                    None => {
                        // TODO: we do not need to break here. This does not indicate that the buffer
                        // has been filled, rather it indicates we have reset the state. The next code
                        // should be part of the initial alphabet. However the first code is special in
                        // the sense of not creating a new code itself. This is handled correctly in
                        // the initialization prior to the loop; and in particular that handling as
                        // written currently relies on putting it into the buffer; so handling it we
                        // would need to ensure that either the buffer is fully cleared after its use,
                        // or use another implementation of handling that first code.
                        break;
                    }
                };

                // Ensure the code buffer is full, we're about to request some codes.
                // Note that this also ensures at least one code is in the buffer if any input is left.
                self.refill_bits(&mut inp);
                let cnt = self.code_buffer.peek_bits(&mut burst);

                // No code left in the buffer, and no more bytes to refill the buffer.
                if cnt == 0 {
                    if have_yet_to_decode_data {
                        status = Ok(LzwStatus::NoProgress);
                    }

                    code_link = Some(deriv);
                    break;
                }

                debug_assert!(
                    // When the table is full, we have a max code above the size switch.
                    self.table.len() >= MAX_ENTRIES - usize::from(self.is_tiff)
                    // When the code size is 2 we have a bit code: (0, 1, CLS, EOF). Then the
                    // computed next_code is 4 which already exceeds the bit width from the start.
                    // Then we will immediately switch code size after this code.
                    //
                    // TODO: this is the reason for some saturating and non-sharp comparisons in
                    // the code below. Maybe it makes sense to revisit turning this into a compile
                    // time choice?
                    || (self.code_buffer.code_size() == 1 && self.next_code < 4)
                    || (self.code_buffer.code_size() == 2 && self.next_code == 4)
                    || self.code_buffer.max_code() - Code::from(self.is_tiff) >= self.next_code,
                    "Table: {}, code_size: {}, next_code: {}, table_condition: {}",
                    self.table.is_full(),
                    self.code_buffer.code_size(),
                    self.next_code,
                    self.code_buffer.max_code() - Code::from(self.is_tiff),
                );

                let mut burst_size = 0;
                let size_switch_at = self.code_buffer.max_code() - Code::from(self.is_tiff);
                // This is intended to wrap. As by the debug assert above, we keep the next
                // code bounded by the current size's max code where we switch code size.
                // Except in case the table is full then we actually want to allow decoding
                // of an arbitrary count of non-resetting symbols.
                let left_before_size_switch = size_switch_at.wrapping_sub(self.next_code);

                // Hoist loop-invariant fields into locals so the compiler doesn't reload
                // from memory on every iteration of the hot burst loop.
                let clear_code = self.clear_code;
                let next_code = self.next_code;

                // A burst is a sequence of decodes that are completely independent of each other. This
                // is the case if neither is an end code, a clear code, or a next code, i.e. we have
                // all of them in the decoding table and thus known their depths, and additionally if
                // we can decode them directly into the output buffer.
                for b in &burst[..cnt] {
                    // We can commit the previous burst code, and will take a slice from the output
                    // buffer. This also avoids the bounds check in the tight loop later.
                    if burst_size > 0 {
                        let len = burst_byte_len[burst_size - 1];
                        let (into, tail) = out.split_at_mut(usize::from(len));
                        target[burst_size - 1] = into;
                        out = tail;
                    }

                    // Check that we don't overflow the code size with all codes we burst decode.
                    burst_size += 1;

                    if burst_size > usize::from(left_before_size_switch) {
                        break;
                    }

                    let read_code = *b;

                    // A burst code can't be special. Fused check: since
                    // end_code = clear_code + 1, `read_code - clear_code < 2`
                    // catches both. Then one more compare for >= next_code.
                    if read_code.wrapping_sub(clear_code) < 2 || read_code >= next_code {
                        break;
                    }

                    // Read the code length and check that we can decode directly into the out slice.
                    let len = self.table.code_len(read_code);

                    if out.len() < usize::from(len) {
                        break;
                    }

                    // We do exactly one more code (the one being inspected in the current iteration)
                    // after the 'burst'. When we want to break decoding precisely on the supplied
                    // buffer, we check if this is the last code to be decoded into it.
                    if CgC::YIELD_ON_FULL {
                        if out.len() == usize::from(len) {
                            break;
                        }
                    }

                    burst_byte_len[burst_size - 1] = len;
                }

                self.code_buffer.consume_bits(burst_size as u8);
                have_yet_to_decode_data = false;

                // Note that the very last code in the burst buffer doesn't actually belong to the
                // burst itself. TODO: sometimes it could, we just don't differentiate between the
                // breaks and a loop end condition above. That may be a speed advantage?
                let (&new_code, burst) = burst[..burst_size].split_last().unwrap();

                // The very tight loop for restoring the actual burst. These can be reconstructed in
                // parallel since none of them depend on a prior constructed. Only the derivation of
                // new codes is not parallel. There are no size changes here either.
                let burst_targets = &mut target[..burst_size - 1];

                if !self.table.is_full() {
                    self.next_code += burst_targets.len() as u16;
                }

                if let Some(n4) = burst_targets.as_mut_array::<4>() {
                    let burst = *burst.as_array::<4>().unwrap();
                    self.table.reconstruct_4(burst, n4, &mut burst_byte);
                } else if let Some(n5) = burst_targets.as_mut_array::<5>() {
                    let n4 = n5.first_chunk_mut::<4>().unwrap();
                    let c4 = *burst[..4].as_array::<4>().unwrap();
                    self.table.reconstruct_4(c4, n4, &mut burst_byte);
                    burst_byte[4] = self.table.reconstruct(burst[4], n5[4]);
                } else {
                    for ((&burst, target), byte) in
                        burst.iter().zip(&mut *burst_targets).zip(&mut burst_byte)
                    {
                        *byte = self.table.reconstruct(burst, target);
                    }
                }

                self.table.derive_burst(&mut deriv, burst, &burst_byte[..]);

                // Now handle the special codes.
                if new_code == self.clear_code {
                    self.reset_tables();
                    last_decoded = None;
                    // Restarts in the next call to the entry point.
                    break;
                }

                if new_code == self.end_code {
                    self.has_ended = true;
                    status = Ok(LzwStatus::Done);
                    last_decoded = None;
                    break;
                }

                if new_code > self.next_code {
                    status = Err(LzwError::InvalidCode);
                    last_decoded = None;
                    break;
                }

                let required_len = if new_code == self.next_code {
                    self.table.code_len(deriv.code) + 1
                } else {
                    self.table.code_len(new_code)
                };

                // We need the decoded data of the new code if it is the `next_code`. This is the
                // special case of LZW decoding that is demonstrated by `banana` (or form cScSc). In
                // all other cases we only need the first character of the decoded data.
                let have_next_code = new_code == self.next_code;

                // Update the slice holding the last decoded word.
                if have_next_code {
                    // If we did not have any burst code, we still hold that slice in the buffer.
                    if let Some(new_last) = target[..burst_size - 1].last_mut() {
                        let slice = core::mem::replace(new_last, &mut []);
                        last_decoded = Some(&*slice);
                    }
                }

                let cha;
                let is_in_buffer = usize::from(required_len) > out.len();
                // Check if we will need to store our current state into the buffer.
                if is_in_buffer {
                    if have_next_code {
                        // last_decoded will be Some if we have restored any code into the out slice.
                        // Otherwise it will still be present in the buffer.
                        if let Some(last) = last_decoded.take() {
                            self.buffer.bytes[..last.len()].copy_from_slice(last);
                            self.buffer.write_mark = last.len();
                            self.buffer.read_mark = last.len();
                        }

                        cha = self.buffer.fill_cscsc();
                    } else {
                        // Restore the decoded word into the buffer.
                        last_decoded = None;
                        cha = self.buffer.fill_reconstruct(&self.table, new_code);
                    }
                } else {
                    let (target, tail) = out.split_at_mut(usize::from(required_len));
                    out = tail;

                    if have_next_code {
                        // Reconstruct high.
                        let source = match last_decoded.take() {
                            Some(last) => last,
                            None => &self.buffer.bytes[..self.buffer.write_mark],
                        };

                        // We don't *actually* expect the unwrap to happen. Each source is at least 1
                        // byte long. But llvm doesn't know this (too much indirect loads and cases).
                        cha = source.get(0).map(|x| *x).unwrap_or(0);
                        target[..source.len()].copy_from_slice(source);
                        target[source.len()..][0] = cha;
                    } else {
                        cha = self.table.reconstruct(new_code, target);
                    }

                    // A new decoded word.
                    last_decoded = Some(target);
                }

                // Each newly read code creates one new code/link based on the preceding code if we
                // have enough space to put it there.
                if !self.table.is_full() {
                    self.table.derive(&deriv, cha);

                    if self.next_code >= self.code_buffer.max_code() - Code::from(self.is_tiff)
                        && self.code_buffer.code_size() < MAX_CODESIZE
                    {
                        self.bump_code_size();
                    }

                    self.next_code += 1;
                }

                // store the information on the decoded word.
                code_link = Some(DerivationBase {
                    code: new_code,
                    first: cha,
                });

                // Can't make any more progress with decoding.
                //
                // We have more data buffered but not enough space to put it? We want fetch a next
                // symbol if possible as in the case of it being a new symbol we can refer to the
                // buffered output as the source for that symbol's meaning and do a memcpy.
                //
                // Since this test is after decoding at least one code, we can now check for an
                // empty buffer and still guarantee progress when one was passed as a parameter.
                if is_in_buffer || out.is_empty() {
                    break;
                }
            }
        }

        // We need to store the last word into the buffer in case the first code in the next
        // iteration is the next_code.
        if let Some(tail) = last_decoded {
            self.buffer.bytes[..tail.len()].copy_from_slice(tail);
            self.buffer.write_mark = tail.len();
            // Mark the full buffer as having been consumed.
            self.buffer.read_mark = tail.len();
        }

        // Ensure we don't indicate that no progress was made if we read some bytes from the input
        // (which is progress).
        if o_in > inp.len() {
            if let Ok(LzwStatus::NoProgress) = status {
                status = Ok(LzwStatus::Ok);
            }
        }

        // Store the code/link state.
        self.last = code_link;

        BufferResult {
            consumed_in: o_in.wrapping_sub(inp.len()),
            consumed_out: o_out.wrapping_sub(out.len()),
            status,
        }
    }
}

impl<C: CodeBuffer, CgC: CodegenConstants> DecodeState<C, CgC> {
    fn next_symbol(&mut self, inp: &mut &[u8]) -> Option<Code> {
        self.code_buffer.next_symbol(inp)
    }

    fn bump_code_size(&mut self) {
        self.code_buffer.bump_code_size()
    }

    fn refill_bits(&mut self, inp: &mut &[u8]) {
        self.code_buffer.refill_bits(inp)
    }
}

impl CodeBuffer for MsbBuffer {
    fn new(min_size: u8) -> Self {
        MsbBuffer {
            code_size: min_size + 1,
            code_mask: (1u16 << (min_size + 1)) - 1,
            bit_buffer: 0,
            bits: 0,
        }
    }

    fn reset(&mut self, min_size: u8) {
        self.code_size = min_size + 1;
        self.code_mask = (1 << self.code_size) - 1;
    }

    fn next_symbol(&mut self, inp: &mut &[u8]) -> Option<Code> {
        if self.bits < self.code_size {
            self.refill_bits(inp);
        }

        if self.bits < self.code_size {
            return None;
        }

        let mask = u64::from(self.code_mask);
        let rotbuf = self.bit_buffer.rotate_left(self.code_size.into());
        self.bit_buffer = rotbuf & !mask;
        self.bits -= self.code_size;
        Some((rotbuf & mask) as u16)
    }

    fn bump_code_size(&mut self) {
        self.code_size += 1;
        self.code_mask = (self.code_mask << 1) | 1;
    }

    fn refill_bits(&mut self, inp: &mut &[u8]) {
        let wish_count = (64 - self.bits) / 8;
        let mut buffer = [0u8; 8];
        let new_bits = match inp.get(..usize::from(wish_count)) {
            Some(bytes) => {
                buffer[..usize::from(wish_count)].copy_from_slice(bytes);
                *inp = &inp[usize::from(wish_count)..];
                wish_count * 8
            }
            None => {
                let new_bits = inp.len() * 8;
                buffer[..inp.len()].copy_from_slice(inp);
                *inp = &[];
                new_bits as u8
            }
        };
        self.bit_buffer |= u64::from_be_bytes(buffer) >> self.bits;
        self.bits += new_bits;
    }

    fn peek_bits(&self, code: &mut [Code; BURST]) -> usize {
        let mut bit_buffer = self.bit_buffer;
        let mask = u64::from(self.code_mask);
        let mut consumed = 0;
        let mut cnt = 0;

        for b in code {
            let consumed_after = consumed + self.code_size;
            if consumed_after > self.bits {
                break;
            }

            cnt += 1;
            consumed = consumed_after;

            let rotbuf = bit_buffer.rotate_left(self.code_size.into());
            *b = (rotbuf & mask) as u16;
            // The read bits are 'appended' but we never interpret those appended bits.
            bit_buffer = rotbuf;
        }

        cnt
    }

    fn consume_bits(&mut self, code_cnt: u8) {
        let bits = self.code_size * code_cnt;
        debug_assert!(bits <= self.bits);

        if bits >= self.bits {
            self.bit_buffer = 0;
        } else {
            // bits < self.bits so this must be smaller than the number size.
            self.bit_buffer = self.bit_buffer << bits;
        }

        self.bits = self.bits.wrapping_sub(bits);
    }

    fn max_code(&self) -> Code {
        self.code_mask
    }

    fn code_size(&self) -> u8 {
        self.code_size
    }
}

impl CodeBuffer for LsbBuffer {
    fn new(min_size: u8) -> Self {
        LsbBuffer {
            code_size: min_size + 1,
            code_mask: (1u16 << (min_size + 1)) - 1,
            bit_buffer: 0,
            bits: 0,
        }
    }

    fn reset(&mut self, min_size: u8) {
        self.code_size = min_size + 1;
        self.code_mask = (1 << self.code_size) - 1;
    }

    fn next_symbol(&mut self, inp: &mut &[u8]) -> Option<Code> {
        if self.bits < self.code_size {
            self.refill_bits(inp);
        }

        if self.bits < self.code_size {
            return None;
        }

        let mask = u64::from(self.code_mask);
        let code = self.bit_buffer & mask;
        self.bit_buffer >>= self.code_size;
        self.bits -= self.code_size;
        Some(code as u16)
    }

    fn bump_code_size(&mut self) {
        self.code_size += 1;
        self.code_mask = (self.code_mask << 1) | 1;
    }

    fn refill_bits(&mut self, inp: &mut &[u8]) {
        let wish_count = (64 - self.bits) / 8;
        let mut buffer = [0u8; 8];
        let new_bits = match inp.get(..usize::from(wish_count)) {
            Some(bytes) => {
                buffer[..usize::from(wish_count)].copy_from_slice(bytes);
                *inp = &inp[usize::from(wish_count)..];
                wish_count * 8
            }
            None => {
                let new_bits = inp.len() * 8;
                buffer[..inp.len()].copy_from_slice(inp);
                *inp = &[];
                new_bits as u8
            }
        };
        self.bit_buffer |= u64::from_le_bytes(buffer) << self.bits;
        self.bits += new_bits;
    }

    fn peek_bits(&self, code: &mut [Code; BURST]) -> usize {
        let mut bit_buffer = self.bit_buffer;
        let mask = u64::from(self.code_mask);
        let mut consumed = 0;
        let mut cnt = 0;

        for b in code {
            let consumed_after = consumed + self.code_size;
            if consumed_after > self.bits {
                break;
            }

            cnt += 1;
            consumed = consumed_after;

            *b = (bit_buffer & mask) as u16;
            bit_buffer = bit_buffer >> self.code_size;
        }

        cnt
    }

    fn consume_bits(&mut self, code_cnt: u8) {
        let bits = self.code_size * code_cnt;
        debug_assert!(bits <= self.bits);

        if bits >= self.bits {
            self.bit_buffer = 0;
        } else {
            // bits < self.bits so this must be smaller than the number size.
            self.bit_buffer = self.bit_buffer >> bits;
        }

        self.bits = self.bits.wrapping_sub(bits);
    }

    fn max_code(&self) -> Code {
        self.code_mask
    }

    fn code_size(&self) -> u8 {
        self.code_size
    }
}

impl Buffer {
    fn new() -> Self {
        Buffer {
            bytes: vec![0; MAX_ENTRIES].into_boxed_slice(),
            read_mark: 0,
            write_mark: 0,
        }
    }

    /// When encoding a sequence `cScSc` where `c` is any character and `S` is any string
    /// this results in two codes `AB`, `A` encoding `cS` and `B` encoding `cSc`. Supposing
    /// the buffer is already filled with the reconstruction of `A`, we can easily fill it
    /// with the reconstruction of `B`.
    fn fill_cscsc(&mut self) -> u8 {
        self.bytes[self.write_mark] = self.bytes[0];
        self.write_mark += 1;
        self.read_mark = 0;
        self.bytes[0]
    }

    // Fill the buffer by decoding from the table
    fn fill_reconstruct(&mut self, table: &Table, code: Code) -> u8 {
        self.write_mark = 0;
        self.read_mark = 0;
        let depth = table.code_len(code);
        let out = &mut self.bytes[..usize::from(depth)];
        let last = table.reconstruct(code, out);
        self.write_mark = usize::from(depth);
        last
    }

    fn buffer(&self) -> &[u8] {
        &self.bytes[self.read_mark..self.write_mark]
    }

    fn consume(&mut self, amt: usize) {
        self.read_mark += amt;
    }
}

impl Table {
    fn new() -> Self {
        Table {
            suffixes: boxed_arr(),
            chain: boxed_arr(),
            depths: boxed_arr(),
            len: 0,
        }
    }

    fn clear(&mut self, min_size: u8) {
        self.len = usize::from(1u16 << u16::from(min_size)) + 2;
    }

    fn init(&mut self, min_size: u8) {
        self.len = 0;
        for i in 0..(1u16 << u16::from(min_size)) {
            let idx = self.len & MASK;
            self.suffixes[idx] = [i as u8, 0, 0, 0, 0, 0, 0, 0];
            self.chain[idx] = Link::base(i as u8);
            self.depths[idx] = 1;
            self.len += 1;
        }
        // Clear code + End code: skip writing when the masked index would
        // alias an alphabet entry (happens at min_size=12).
        for _ in 0..2 {
            if self.len < MAX_ENTRIES {
                let idx = self.len & MASK;
                self.chain[idx] = Link::base(0);
                self.depths[idx] = 0;
            }
            self.len += 1;
        }
    }

    fn first_of(&self, code: Code) -> u8 {
        self.chain[usize::from(code) & MASK].first
    }

    fn len(&self) -> usize {
        self.len
    }

    fn code_len(&self, code: Code) -> u16 {
        self.depths[usize::from(code) & MASK]
    }

    fn is_empty(&self) -> bool {
        self.len == 0
    }

    fn is_full(&self) -> bool {
        self.len >= MAX_ENTRIES
    }

    fn derive(&mut self, from: &DerivationBase, byte: u8) {
        let idx = self.len & MASK;

        let parent = usize::from(from.code) & MASK;
        let parent_depth = self.depths[parent];
        let pos = parent_depth as usize & (STREAMING_Q - 1);
        let mut link = from.derive();

        if pos > 0 {
            self.suffixes[idx] = self.suffixes[parent];
            self.suffixes[idx][pos] = byte;
            link.prev = self.chain[parent].prev;
        } else {
            self.suffixes[idx] = [0u8; STREAMING_Q];
            self.suffixes[idx][0] = byte;
        }

        self.depths[idx] = parent_depth + 1;
        self.chain[idx] = link;
        self.len += 1;
    }

    fn derive_burst(&mut self, from: &mut DerivationBase, burst: &[Code], first: &[u8]) {
        for (&code, &first_byte) in burst.iter().zip(first.iter()) {
            self.derive(from, first_byte);
            from.code = code;
            from.first = first_byte;
        }
    }

    fn reconstruct(&self, code: Code, out: &mut [u8]) -> u8 {
        let o = out.len();
        let code_index = usize::from(code) & MASK;
        let suffix = self.suffixes[code_index];

        // Short path: whole value fits in one Q-chunk.
        if o <= STREAMING_Q {
            out.copy_from_slice(&suffix[..o]);
            return self.chain[code_index].first;
        }

        // Tail: last incomplete chunk. Note: this is not the same as as_chunks_mut's tail since we
        // have a full chunk when this the cdde depth is aligned.
        let tail_len = ((o - 1) & (STREAMING_Q - 1)) + 1;
        let tail_start = o - tail_len;
        out[tail_start..].copy_from_slice(&suffix[..tail_len]);

        let first = self.chain[code_index].first;
        let mut c = self.chain[code_index].prev;
        // Full 8-byte chunks, walking the prefix chain backward.
        // chunks_exact_mut guarantees each chunk has exactly STREAMING_Q bytes,
        // so LLVM compiles copy_from_slice to a single qword move with no
        // bounds check. The `.rev()` walks from end to start.
        for chunk in out[..tail_start].chunks_exact_mut(STREAMING_Q).rev() {
            let code_index = usize::from(c) & MASK;
            chunk.copy_from_slice(&self.suffixes[code_index]);
            c = self.chain[code_index].prev;
        }

        first
    }

    fn non_memcpy(out: &mut [u8], from: [u8; STREAMING_Q]) {
        match out.len() {
            0 => {},
            1 => out[0] = from[0],
            2 => out[..2].copy_from_slice(&from[..2]),
            3 => out[..3].copy_from_slice(&from[..3]),
            4 => out[..4].copy_from_slice(&from[..4]),
            5 => out[..5].copy_from_slice(&from[..5]),
            6 => out[..6].copy_from_slice(&from[..6]),
            7 => out[..7].copy_from_slice(&from[..7]),
            8 => out[..8].copy_from_slice(&from[..8]),
            _ => unreachable!(),
        }
    }

    fn reconstruct_4(&self, code: [Code; 4], out: &mut [&mut [u8]; 4], byte: &mut [u8; BURST]) {
        let [a, b, c, d] = out;

        if a.len() <= STREAMING_Q && b.len() <= STREAMING_Q && c.len() <= STREAMING_Q && d.len() <= STREAMING_Q {
            let cis = code.map(|x| usize::from(x) & MASK);
            let [sa, sb, sc, sd] = cis.map(|i| self.suffixes[i]);

            Self::non_memcpy(a, sa);
            Self::non_memcpy(b, sb);
            Self::non_memcpy(c, sc);
            Self::non_memcpy(d, sd);

            byte[0] = self.chain[cis[0]].first;
            byte[1] = self.chain[cis[1]].first;
            byte[2] = self.chain[cis[2]].first;
            byte[3] = self.chain[cis[3]].first;

            return;
        }

        byte[0] = self.reconstruct(code[0], a);
        byte[1] = self.reconstruct(code[1], b);
        byte[2] = self.reconstruct(code[2], c);
        byte[3] = self.reconstruct(code[3], d);
    }
}

fn boxed_arr<T: Clone + Default, const N: usize>() -> Box<[T; N]> {
    use core::convert::TryInto;
    vec![T::default(); N]
        .into_boxed_slice()
        .try_into()
        .ok()
        .unwrap()
}

impl Link {
    fn base(byte: u8) -> Self {
        Link {
            prev: 0,
            first: byte,
        }
    }
}

impl DerivationBase {
    // TODO: this has self type to make it clear we might depend on the old in a future
    // optimization. However, that has no practical purpose right now.
    fn derive(&self) -> Link {
        Link {
            prev: self.code,
            first: self.first,
        }
    }
}

// ============================================================================
// DecodeStateStreaming — single-code-per-iteration decoder with mini-burst.
//
// Inspired by wuffs_lzw's inner-loop structure. Eliminates every piece
// of machinery that doesn't exist in the wuffs inner loop:
//
//   * no 6-wide burst peek (no [Code; BURST] array, no peek_bits batch)
//   * no target slice array (no [&mut [u8]; BURST], no per-iter split_at_mut)
//   * no burst reconstruct loop, no derive_burst
//   * no per-iter is_full check — derive is guarded once per code by
//     comparing save_code against MAX_ENTRIES
//   * no last_decoded/cScSc bookkeeping — KwKwK reconstructs inline
//
// Table layout: PreQ+SufQ (Q=8), PreQ+SufQ(Q=8) table layout from the Wuffs LZW README. The
// difference is purely in the loop structure, not the table.
//
// MINI-BURST fast path: after processing a literal, the decoder
// opportunistically inlines additional codes that can be handled
// without re-entering the full dispatch chain. Two cases:
//   * literal (peek < clear_code): 1-byte output
//   * short copy (end_code < peek < save_code, lm1 < 8): value fits
//     in one Q-byte suffix chunk, no prefix chain walk, 1 memcpy
//
// This amortizes the outer-loop overhead (yield check, refill check,
// dispatch chain) across runs of consecutive safe codes, which dominate
// photographic TIFF data (with horizontal predictor: codes emit 2–4
// bytes each) and random data.
//
// Supported options (full parity with Classic):
//   * LSB and MSB bit order via the StreamingBitPacking trait (compile-
//     time monomorphized, zero runtime cost).
//   * TIFF early-change: runtime flag on the struct, only affects
//     codes_until_bump initialization in init_table.
//   * yield_on_full_buffer: compile-time const via CodegenConstants,
//     eliminates a branch in non-yield mode.
//
// Mid-code suspension uses a 4 KiB `pending` buffer: when a copy
// code's full value doesn't fit in the caller's `out` slice, we
// reconstruct the value into `pending`, copy what fits, and stream
// the rest on subsequent advance() calls. This preserves sans-IO
// semantics for the caller.
// ============================================================================

const STREAMING_Q: usize = 8;
const STREAMING_MASK: usize = MAX_ENTRIES - 1;

// ----- Bit packing direction (zero-cost compile-time generic) -----

/// Marker trait for LSB vs MSB bit ordering. Implemented by zero-sized
/// `Lsb` and `Msb` types whose methods get inlined away during
/// monomorphization. Provides refill and extract primitives over a u64
/// bit buffer; the buffer layout differs by direction but the signatures
/// are uniform.
pub(crate) trait StreamingBitPacking {
    /// Refill the bit buffer from `inp` when `*n_bits < width`. On entry,
    /// `inp.len() >= 8` is guaranteed.
    fn refill_fast8(bit_buffer: &mut u64, n_bits: &mut u8, inp: &mut &[u8]);
    /// Refill one byte at a time (slow path, near EOF).
    fn refill_byte(bit_buffer: &mut u64, n_bits: &mut u8, byte: u8);
    /// Extract one code from the buffer, consuming `width` bits.
    fn extract(bit_buffer: &mut u64, n_bits: &mut u8, width: u8, mask: u64) -> Code;
    /// Undo an extract — put `code` back at the head of the buffer.
    fn put_back(bit_buffer: &mut u64, n_bits: &mut u8, width: u8, code: Code);
    /// Peek the next code without consuming it. Used by the mini-burst
    /// fast path to speculatively check for consecutive literals.
    fn peek_code(bit_buffer: u64, width: u8, mask: u64) -> Code;
}

pub(crate) struct StreamingLsb;
pub(crate) struct StreamingMsb;

impl StreamingBitPacking for StreamingLsb {
    #[inline(always)]
    fn refill_fast8(bit_buffer: &mut u64, n_bits: &mut u8, inp: &mut &[u8]) {
        use core::convert::TryInto;
        let bytes: [u8; 8] = inp[..8].try_into().unwrap();
        let chunk = u64::from_le_bytes(bytes);
        // Shift the new chunk into the empty HIGH part of the buffer.
        // Bits above position 64 are truncated and re-read on the next
        // refill (iop doesn't advance past them). Mirrors wuffs' `n_bits
        // |= 24` trick but for 64-bit buffers.
        *bit_buffer |= chunk << *n_bits;
        let consume = ((63 - *n_bits) >> 3) as usize;
        *inp = &inp[consume..];
        *n_bits |= 56;
    }
    #[inline(always)]
    fn refill_byte(bit_buffer: &mut u64, n_bits: &mut u8, byte: u8) {
        *bit_buffer |= u64::from(byte) << *n_bits;
        *n_bits += 8;
    }
    #[inline(always)]
    fn extract(bit_buffer: &mut u64, n_bits: &mut u8, width: u8, mask: u64) -> Code {
        let code = (*bit_buffer & mask) as Code;
        *bit_buffer >>= width;
        *n_bits -= width;
        code
    }
    #[inline(always)]
    fn put_back(bit_buffer: &mut u64, n_bits: &mut u8, width: u8, code: Code) {
        *bit_buffer = (*bit_buffer << width) | u64::from(code);
        *n_bits += width;
    }
    #[inline(always)]
    fn peek_code(bit_buffer: u64, _width: u8, mask: u64) -> Code {
        (bit_buffer & mask) as Code
    }
}

impl StreamingBitPacking for StreamingMsb {
    #[inline(always)]
    fn refill_fast8(bit_buffer: &mut u64, n_bits: &mut u8, inp: &mut &[u8]) {
        // Unlike LSB, we can't use the wuffs over-read trick: for MSB
        // the "extra" bits would land below n_bits in the buffer's low
        // positions, where rotate_left would pick them up as garbage.
        //
        // Instead, read exactly as many bytes as fit in (64 - n_bits):
        // 0..=8 bytes depending on current n_bits. For width <= 12, we
        // enter this branch with n_bits < 12 so wish_count is always 6..=8.
        let wish_count = ((64 - *n_bits) / 8) as usize;
        // Read wish_count bytes into the top of a temporary buffer.
        let mut buf = [0u8; 8];
        buf[..wish_count].copy_from_slice(&inp[..wish_count]);
        let chunk = u64::from_be_bytes(buf);
        // Shift the chunk right by n_bits so the valid new bits land
        // in positions (64 - n_bits - wish_count*8) .. (64 - n_bits),
        // i.e. immediately below the existing valid bits.
        *bit_buffer |= chunk >> *n_bits;
        *inp = &inp[wish_count..];
        *n_bits += (wish_count as u8) * 8;
    }
    #[inline(always)]
    fn refill_byte(bit_buffer: &mut u64, n_bits: &mut u8, byte: u8) {
        // New byte goes BELOW existing valid bits at position (56-n_bits)..(64-n_bits).
        *bit_buffer |= u64::from(byte) << (56 - *n_bits);
        *n_bits += 8;
    }
    #[inline(always)]
    fn extract(bit_buffer: &mut u64, n_bits: &mut u8, width: u8, mask: u64) -> Code {
        // Rotate the top `width` bits to the bottom, mask them out as
        // the code, keep the rest in the buffer.
        let rot = bit_buffer.rotate_left(u32::from(width));
        let code = (rot & mask) as Code;
        *bit_buffer = rot & !mask;
        *n_bits -= width;
        code
    }
    #[inline(always)]
    fn put_back(bit_buffer: &mut u64, n_bits: &mut u8, width: u8, code: Code) {
        // Push the code back into the TOP of the buffer: rotate right
        // by width (moving everything down), then OR the code into the
        // now-vacated high bits.
        *bit_buffer = (*bit_buffer >> width) | (u64::from(code) << (64 - width));
        *n_bits += width;
    }
    #[inline(always)]
    fn peek_code(bit_buffer: u64, width: u8, mask: u64) -> Code {
        // MSB extract is a rotate-left by width then mask; peek is the
        // same minus the state mutation. LLVM elides the temporary.
        let rot = bit_buffer.rotate_left(u32::from(width));
        (rot & mask) as Code
    }
}

pub(crate) struct DecodeStateStreaming<P: StreamingBitPacking, CgC: CodegenConstants> {
    // PreQ+SufQ table. No firsts[] array —
    // first-byte lookup walks the prefix chain, which is cheap when chains
    // are short (single-byte literals skip the walk entirely).
    suffixes: Box<[[u8; STREAMING_Q]; MAX_ENTRIES]>,
    prefixes: Box<[Code; MAX_ENTRIES]>,
    lm1s: Box<[u16; MAX_ENTRIES]>,

    // Bit reader (64-bit, direction via P).
    bit_buffer: u64,
    n_bits: u8,
    _packing: core::marker::PhantomData<fn() -> (P, CgC)>,

    // LZW state. `prev_code == end_code` is the "no previous" sentinel
    // used after construction and after a clear code.
    clear_code: Code,
    end_code: Code,
    /// Next slot to write when we derive. When this reaches MAX_ENTRIES,
    /// the table is full and derive becomes a no-op.
    save_code: Code,
    /// Previous decoded code, for the next derive.
    prev_code: Code,
    /// Current code width in bits.
    width: u8,
    /// Mask of the current width: (1 << width) - 1.
    width_mask: Code,
    /// Derives-until-next-width-bump counter. Decremented per derive.
    /// When it reaches 0, the width bumps and this resets to 1 << width.
    /// Once width hits MAX_CODESIZE, this stays at u16::MAX so derive
    /// never bumps again.
    codes_until_bump: u16,

    min_size: u8,
    has_ended: bool,
    implicit_reset: bool,
    /// TIFF early-change mode: bump width one iteration earlier.
    /// Runtime flag (not a const generic) because it only affects the
    /// init and bump-slow paths, not the per-code hot loop.
    is_tiff: bool,

    // Mid-code suspension buffer.
    pending: Box<[u8; MAX_ENTRIES]>,
    /// Bytes written into pending.
    pending_len: u16,
    /// Bytes already drained to the caller.
    pending_off: u16,
}

impl<P: StreamingBitPacking, CgC: CodegenConstants> DecodeStateStreaming<P, CgC> {
    pub(crate) fn new(min_size: u8) -> Self {
        let clear_code = 1u16 << u16::from(min_size);
        let end_code = clear_code + 1;
        let width = min_size + 1;
        let tiff_offset = 0; // is_tiff starts false; wired up by caller
        let save_code = end_code + 1;
        let mut state = DecodeStateStreaming::<P, CgC> {
            suffixes: boxed_arr(),
            prefixes: boxed_arr(),
            lm1s: boxed_arr(),
            bit_buffer: 0,
            n_bits: 0,
            _packing: core::marker::PhantomData,
            clear_code,
            end_code,
            save_code,
            prev_code: end_code, // sentinel: "no previous code yet"
            width,
            width_mask: (1u16 << width) - 1,
            codes_until_bump: (1u16 << width)
                .saturating_sub(save_code)
                .saturating_sub(tiff_offset),
            min_size,
            has_ended: false,
            implicit_reset: true,
            is_tiff: false,
            pending: boxed_arr(),
            pending_len: 0,
            pending_off: 0,
        };
        state.init_table();
        state.bump_if_lowbit();
        state
    }

    /// Bump width once if `codes_until_bump == 0` after init, which happens
    /// when `min_size < 2` and `save_code` already equals `1 << width`.
    /// At most one bump is ever needed (the gap is at most 2 for min_size=0).
    /// Placed at call sites (not inside init_table) to keep init_table's
    /// LLVM codegen identical to the pre-change version — see weezl PR #67.
    #[inline(always)]
    fn bump_if_lowbit(&mut self) {
        if self.codes_until_bump == 0 && self.width < MAX_CODESIZE {
            self.bump_width_slow();
        }
    }

    fn init_table(&mut self) {
        // Populate literal slots: each literal code i decodes to byte i.
        // In PreQ+SufQ, a literal has lm1=0, suffix[0]=i, suffix[1..]=0,
        // prefix=0 (never read because lm1/Q == 0).
        for i in 0..(1u16 << u16::from(self.min_size)) {
            let idx = usize::from(i) & STREAMING_MASK;
            self.suffixes[idx] = [0u8; STREAMING_Q];
            self.suffixes[idx][0] = i as u8;
            self.prefixes[idx] = 0;
            self.lm1s[idx] = 0;
        }
        self.save_code = self.end_code + 1;
        self.prev_code = self.end_code;
        self.width = self.min_size + 1;
        self.width_mask = (1u16 << self.width) - 1;
        let tiff_offset: u16 = if self.is_tiff { 1 } else { 0 };
        self.codes_until_bump = (1u16 << self.width)
            .saturating_sub(self.save_code)
            .saturating_sub(tiff_offset);
    }

    /// Derive a new table entry: parent = prev_code, new suffix byte = `byte`.
    /// Matches the PreQ+SufQ rule: if parent's suffix is full (parent_lm1 % Q == Q-1),
    /// start a new Q-chunk; otherwise extend parent's suffix.
    ///
    /// Uses a `codes_until_bump` countdown to avoid a per-call width
    /// comparison; the width-bump branch only runs once per width epoch
    /// (~256 derives at width 9) instead of on every derive.
    #[inline(always)]
    fn derive(&mut self, byte: u8) {
        if self.save_code >= MAX_ENTRIES as Code {
            return;
        }
        let idx = usize::from(self.save_code) & STREAMING_MASK;
        let parent = usize::from(self.prev_code) & STREAMING_MASK;
        let parent_lm1 = self.lm1s[parent];
        let new_lm1 = parent_lm1.wrapping_add(1);
        let pos = (parent_lm1 as usize & (STREAMING_Q - 1)) + 1;

        if pos < STREAMING_Q {
            self.suffixes[idx] = self.suffixes[parent];
            self.suffixes[idx][pos] = byte;
            self.prefixes[idx] = self.prefixes[parent];
        } else {
            self.suffixes[idx] = [0u8; STREAMING_Q];
            self.suffixes[idx][0] = byte;
            self.prefixes[idx] = self.prev_code;
        }
        self.lm1s[idx] = new_lm1;
        self.save_code += 1;

        // Width-bump countdown: one decrement per derive, one branch that
        // only fires at width transitions (~every 256/512/1024/2048 derives).
        self.codes_until_bump = self.codes_until_bump.wrapping_sub(1);
        if self.codes_until_bump == 0 {
            self.bump_width_slow();
        }
    }

    /// Out-of-line width bump, called from derive() only at epoch boundaries.
    ///
    /// Note that the TIFF early-change offset only applies to the FIRST
    /// bump (at init time). Subsequent intervals are identical between
    /// TIFF and non-TIFF (both are 1 << (new_width - 1)). The offset is
    /// therefore NOT repeated here; init_table handles it once.
    #[cold]
    #[inline(never)]
    fn bump_width_slow(&mut self) {
        if self.width < MAX_CODESIZE {
            self.width += 1;
            self.width_mask = (self.width_mask << 1) | 1;
            // Next bump distance is exactly 1 << (new_width - 1).
            self.codes_until_bump = 1u16 << (self.width - 1);
        } else {
            // Width is maxed out. Park the counter so we never enter this
            // branch again (until reset).
            self.codes_until_bump = u16::MAX;
        }
    }

    /// Reconstruct a code's value into `out`. `out.len()` must equal
    /// `lm1s[code] + 1`. Walks the prefix chain in 8-byte strides, tail first.
    ///
    /// Takes the table arrays by explicit reference (rather than `&self`) so
    /// the caller can simultaneously borrow `&mut self.pending` without
    /// tripping the borrow checker.
    ///
    /// Uses a chunks_exact_mut pattern for bounds-check-free chain walks —
    /// verified empirically to compile to a 7-instruction loop body per
    /// Q-chunk with a single qword load and a single qword store, no
    /// per-iter bounds check, and no memcpy call.
    #[inline(always)]
    fn reconstruct_streaming_into(
        suffixes: &[[u8; STREAMING_Q]; MAX_ENTRIES],
        prefixes: &[Code; MAX_ENTRIES],
        code: Code,
        out: &mut [u8],
    ) {
        let o = out.len();
        let ci = usize::from(code) & STREAMING_MASK;
        let suf = &suffixes[ci];

        // Short path: whole value fits in one Q-chunk.
        if o <= STREAMING_Q {
            out.copy_from_slice(&suf[..o]);
            return;
        }

        // Tail: last incomplete chunk.
        let tail_len = ((o - 1) & (STREAMING_Q - 1)) + 1;
        let tail_start = o - tail_len;
        out[tail_start..].copy_from_slice(&suf[..tail_len]);
        let mut c = prefixes[ci];

        // Full 8-byte chunks, walking the prefix chain backward.
        // chunks_exact_mut guarantees each chunk has exactly STREAMING_Q bytes,
        // so LLVM compiles copy_from_slice to a single qword move with no
        // bounds check. The `.rev()` walks from end to start.
        for chunk in out[..tail_start].chunks_exact_mut(STREAMING_Q).rev() {
            let ci = usize::from(c) & STREAMING_MASK;
            chunk.copy_from_slice(&suffixes[ci]);
            c = prefixes[ci];
        }
    }

    /// Convenience wrapper that borrows `self` immutably.
    #[inline(always)]
    fn reconstruct_streaming(&self, code: Code, out: &mut [u8]) {
        Self::reconstruct_streaming_into(&self.suffixes, &self.prefixes, code, out);
    }

    /// First byte of the value at `code` — the leftmost byte walked to
    /// by reconstruct. For literals (lm1 == 0), this is suffix[0].
    /// For longer values, we follow the prefix chain until we hit the
    /// chunk containing the first byte.
    #[inline(always)]
    fn first_of(&self, mut code: Code) -> u8 {
        // Walk all the way to the chain start. The root of the chain
        // (the original literal) always has lm1 < Q, so its suffix[0]
        // is the first byte of the entire value.
        loop {
            let ci = usize::from(code) & STREAMING_MASK;
            let lm1 = self.lm1s[ci];
            if lm1 < STREAMING_Q as u16 {
                return self.suffixes[ci][0];
            }
            code = self.prefixes[ci];
        }
    }

    /// Drain the pending buffer into `out`. Returns the number of bytes
    /// drained. After a successful full drain, `pending_len == pending_off`
    /// and we clear both to zero.
    #[inline(always)]
    fn drain_pending(&mut self, out: &mut [u8]) -> usize {
        let avail = usize::from(self.pending_len - self.pending_off);
        let n = avail.min(out.len());
        let off = usize::from(self.pending_off);
        out[..n].copy_from_slice(&self.pending[off..off + n]);
        self.pending_off += n as u16;
        if self.pending_off == self.pending_len {
            self.pending_off = 0;
            self.pending_len = 0;
        }
        n
    }
}

impl<P: StreamingBitPacking + 'static, CgC: CodegenConstants + 'static> Stateful
    for DecodeStateStreaming<P, CgC>
{
    fn has_ended(&self) -> bool {
        self.has_ended
    }

    fn restart(&mut self) {
        self.has_ended = false;
    }

    fn reset(&mut self) {
        self.init_table();
        self.bump_if_lowbit();
        self.bit_buffer = 0;
        self.n_bits = 0;
        self.pending_len = 0;
        self.pending_off = 0;
        self.has_ended = false;
    }

    fn advance(&mut self, mut inp: &[u8], mut out: &mut [u8]) -> BufferResult {
        if self.has_ended {
            return BufferResult {
                consumed_in: 0,
                consumed_out: 0,
                status: Ok(LzwStatus::Done),
            };
        }

        let o_in = inp.len();
        let o_out = out.len();

        // First: drain any bytes still pending from a previously-started code.
        if self.pending_len > 0 {
            let n = self.drain_pending(out);
            out = &mut out[n..];
            if self.pending_len > 0 {
                // Still pending — caller's out is full. Don't touch inp.
                return BufferResult {
                    consumed_in: 0,
                    consumed_out: n,
                    status: Ok(LzwStatus::Ok),
                };
            }
        }

        let mut status = Ok(LzwStatus::Ok);

        // Main decode loop: one code per iteration, wuffs-style.
        loop {
            // yield_on_full: if the caller wants to stop as soon as the
            // output is full and we've already written something this
            // call, return immediately without attempting another code.
            // The bit buffer still holds whatever bits we had, so the
            // next advance() call resumes cleanly.
            if CgC::YIELD_ON_FULL && out.is_empty() {
                break;
            }

            // Refill the bit buffer if it doesn't hold enough bits for the
            // next code. Fast path: 8-byte read via P::refill_fast8.
            if self.n_bits < self.width {
                if inp.len() >= 8 {
                    P::refill_fast8(&mut self.bit_buffer, &mut self.n_bits, &mut inp);
                } else if !inp.is_empty() {
                    // Slow path: byte-at-a-time until we have enough bits
                    // or the input is empty.
                    while self.n_bits < self.width && !inp.is_empty() {
                        P::refill_byte(&mut self.bit_buffer, &mut self.n_bits, inp[0]);
                        inp = &inp[1..];
                    }
                    if self.n_bits < self.width {
                        status = if o_in > inp.len() {
                            Ok(LzwStatus::Ok)
                        } else {
                            Ok(LzwStatus::NoProgress)
                        };
                        break;
                    }
                } else {
                    status = if o_in > inp.len() {
                        Ok(LzwStatus::Ok)
                    } else {
                        Ok(LzwStatus::NoProgress)
                    };
                    break;
                }
            }

            // Extract one code via the trait (LSB = low bits, MSB = rotate-left).
            let code = P::extract(
                &mut self.bit_buffer,
                &mut self.n_bits,
                self.width,
                u64::from(self.width_mask),
            );

            // Dispatch.
            if code < self.clear_code {
                // ==== LITERAL path ====
                if out.is_empty() {
                    // Put the bits back — we can't commit this code yet.
                    P::put_back(&mut self.bit_buffer, &mut self.n_bits, self.width, code);
                    break;
                }
                out[0] = code as u8;
                out = &mut out[1..];
                if self.prev_code != self.end_code {
                    self.derive(code as u8);
                }
                self.prev_code = code;

                // MINI-BURST: inline-process consecutive codes without
                // going through the full outer-loop dispatch. Two
                // fast paths:
                //   a) literal (peek < clear_code): 1-byte output
                //   b) short copy (clear_code < peek < save_code, lm1 < Q):
                //      value fits in one Q-byte suffix chunk, no chain walk
                //
                // Bail out on clear/end/KwKwK/invalid/long-copy, or
                // when out doesn't have at least Q bytes of slack
                // (needed so a short copy has room without bounds checks).
                //
                // prev_code != end_code is guaranteed at entry because
                // we just wrote a literal above.
                while self.n_bits >= self.width && out.len() >= STREAMING_Q {
                    let peek =
                        P::peek_code(self.bit_buffer, self.width, u64::from(self.width_mask));
                    if peek < self.clear_code {
                        // ---- LITERAL fast path ----
                        let _ = P::extract(
                            &mut self.bit_buffer,
                            &mut self.n_bits,
                            self.width,
                            u64::from(self.width_mask),
                        );
                        out[0] = peek as u8;
                        out = &mut out[1..];
                        self.derive(peek as u8);
                        self.prev_code = peek;
                    } else if peek > self.end_code && peek < self.save_code {
                        // ---- SHORT COPY fast path ----
                        // Guard: peek must be strictly between end_code
                        // and save_code (i.e., a valid existing derived
                        // entry). This explicitly excludes clear_code
                        // and end_code from the fast path, which would
                        // otherwise read stale table slots.
                        //
                        // Only codes with lm1 < Q have their full
                        // value in suffix[0..value_len] (no prefix
                        // chain to walk). Photographic data with
                        // horizontal predictor is dominated by short
                        // copies — this is where the fast path helps.
                        let ci = usize::from(peek) & STREAMING_MASK;
                        let lm1 = self.lm1s[ci];
                        if lm1 >= STREAMING_Q as u16 {
                            break; // long copy, fall back to main dispatch
                        }
                        let value_len = usize::from(lm1) + 1;
                        let _ = P::extract(
                            &mut self.bit_buffer,
                            &mut self.n_bits,
                            self.width,
                            u64::from(self.width_mask),
                        );
                        // Safe: out.len() >= STREAMING_Q >= value_len.
                        let suf = &self.suffixes[ci];
                        for i in 0..value_len {
                            out[i] = suf[i];
                        }
                        let first = suf[0];
                        out = &mut out[value_len..];
                        self.derive(first);
                        self.prev_code = peek;
                    } else {
                        // clear / end / KwKwK / invalid — full dispatch
                        break;
                    }
                }
            } else if code == self.clear_code {
                // ==== CLEAR ====
                self.init_table();
                self.bump_if_lowbit();
                // prev_code is back to the sentinel.
            } else if code == self.end_code {
                // ==== END ====
                self.has_ended = true;
                status = Ok(LzwStatus::Done);
                break;
            } else if code < self.save_code {
                // ==== COPY (known code) ====
                if self.prev_code == self.end_code && !self.implicit_reset {
                    status = Err(LzwError::InvalidCode);
                    break;
                }
                let value_len = usize::from(self.lm1s[usize::from(code) & STREAMING_MASK]) + 1;

                if value_len <= out.len() {
                    // Direct reconstruct into caller's slice.
                    let (target, tail) = out.split_at_mut(value_len);
                    self.reconstruct_streaming(code, target);
                    let first = target[0];
                    out = tail;
                    if self.prev_code != self.end_code {
                        self.derive(first);
                    }
                    self.prev_code = code;
                } else {
                    // value_len > out.len(): spill through pending so we
                    // can partially fill out this call and drain the rest
                    // on the next advance(). yield_on_full mode ALSO
                    // spills — the yield behavior is in the top-of-loop
                    // check which breaks as soon as out is fully drained.
                    let first = self.first_of(code);
                    Self::reconstruct_streaming_into(
                        &self.suffixes,
                        &self.prefixes,
                        code,
                        &mut self.pending[..value_len],
                    );
                    self.pending_len = value_len as u16;
                    self.pending_off = 0;
                    let n = self.drain_pending(out);
                    out = &mut out[n..];
                    if self.prev_code != self.end_code {
                        self.derive(first);
                    }
                    self.prev_code = code;
                    // pending still has data — caller's out is full.
                    if self.pending_len > 0 {
                        break;
                    }
                }
            } else if code == self.save_code {
                // ==== KwKwK (code equals the key being added) ====
                if self.prev_code == self.end_code {
                    status = Err(LzwError::InvalidCode);
                    break;
                }
                // Value = prev value + first byte of prev value.
                let prev_ci = usize::from(self.prev_code) & STREAMING_MASK;
                let prev_len = usize::from(self.lm1s[prev_ci]) + 1;
                let value_len = prev_len + 1;

                if value_len <= out.len() {
                    let (target, tail) = out.split_at_mut(value_len);
                    // Reconstruct first; then target[0] IS the first byte of
                    // the full value (= first byte of prev = suffix byte).
                    // This avoids a separate O(chain) first_of() walk, which
                    // is catastrophic on solid-color data where every code
                    // is a KwKwK code with a long chain.
                    Self::reconstruct_streaming_into(
                        &self.suffixes,
                        &self.prefixes,
                        self.prev_code,
                        &mut target[..prev_len],
                    );
                    let first = target[0];
                    target[prev_len] = first;
                    out = tail;
                    self.derive(first);
                    self.prev_code = code;
                } else {
                    // Spill path: reconstruct into pending, read first from
                    // pending[0] (cheap because we just wrote it).
                    Self::reconstruct_streaming_into(
                        &self.suffixes,
                        &self.prefixes,
                        self.prev_code,
                        &mut self.pending[..prev_len],
                    );
                    let first = self.pending[0];
                    self.pending[prev_len] = first;
                    self.pending_len = value_len as u16;
                    self.pending_off = 0;
                    let n = self.drain_pending(out);
                    out = &mut out[n..];
                    self.derive(first);
                    self.prev_code = code;
                    if self.pending_len > 0 {
                        break;
                    }
                }
            } else {
                // Invalid code.
                status = Err(LzwError::InvalidCode);
                break;
            }
        }

        BufferResult {
            consumed_in: o_in - inp.len(),
            consumed_out: o_out - out.len(),
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::alloc::vec::Vec;
    #[cfg(feature = "std")]
    use crate::StreamBuf;
    use crate::{decode::Decoder, BitOrder};

    #[test]
    fn invalid_code_size_low() {
        let _ = Decoder::new(BitOrder::Msb, 0);
        let _ = Decoder::new(BitOrder::Msb, 1);
    }

    #[test]
    #[should_panic]
    fn invalid_code_size_high() {
        let _ = Decoder::new(BitOrder::Msb, 14);
    }

    fn make_encoded() -> Vec<u8> {
        const FILE: &'static [u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/benches/binary-8-msb.lzw"
        ));
        return Vec::from(FILE);
    }

    #[test]
    #[cfg(feature = "std")]
    fn into_stream_buffer_no_alloc() {
        let encoded = make_encoded();
        let mut decoder = Decoder::new(BitOrder::Msb, 8);

        let mut output = vec![];
        let mut buffer = [0; 512];
        let mut istream = decoder.into_stream(&mut output);
        istream.set_buffer(&mut buffer[..]);
        istream.decode(&encoded[..]).status.unwrap();

        match istream.buffer {
            Some(StreamBuf::Borrowed(_)) => {}
            None => panic!("Decoded without buffer??"),
            Some(StreamBuf::Owned(_)) => panic!("Unexpected buffer allocation"),
        }
    }

    #[test]
    #[cfg(feature = "std")]
    fn into_stream_buffer_small_alloc() {
        struct WriteTap<W: std::io::Write>(W);
        const BUF_SIZE: usize = 512;

        impl<W: std::io::Write> std::io::Write for WriteTap<W> {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                assert!(buf.len() <= BUF_SIZE);
                self.0.write(buf)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                self.0.flush()
            }
        }

        let encoded = make_encoded();
        let mut decoder = Decoder::new(BitOrder::Msb, 8);

        let mut output = vec![];
        let mut istream = decoder.into_stream(WriteTap(&mut output));
        istream.set_buffer_size(512);
        istream.decode(&encoded[..]).status.unwrap();

        match istream.buffer {
            Some(StreamBuf::Owned(vec)) => assert!(vec.len() <= BUF_SIZE),
            Some(StreamBuf::Borrowed(_)) => panic!("Unexpected borrowed buffer, where from?"),
            None => panic!("Decoded without buffer??"),
        }
    }

    #[test]
    #[cfg(feature = "std")]
    fn reset() {
        let encoded = make_encoded();
        let mut decoder = Decoder::new(BitOrder::Msb, 8);
        let mut reference = None;

        for _ in 0..2 {
            let mut output = vec![];
            let mut buffer = [0; 512];
            let mut istream = decoder.into_stream(&mut output);
            istream.set_buffer(&mut buffer[..]);
            istream.decode_all(&encoded[..]).status.unwrap();

            decoder.reset();
            if let Some(reference) = &reference {
                assert_eq!(output, *reference);
            } else {
                reference = Some(output);
            }
        }
    }

    #[test]
    fn table_derive() {
        let mut table = super::Table::new();
        table.init(8);

        let mut base = super::DerivationBase {
            code: 1,
            first: 0x1,
        };

        for i in 0..16 {
            table.derive(&base, i);
            base.code = (table.len - 1) as u16;
        }

        let last = (table.len - 1) as u16;
        assert_eq!(table.first_of(last), 1);
        assert_eq!(table.code_len(last), 17);
        assert_eq!(table.suffixes[last as usize], [15, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(table.chain[last as usize].prev, last - 1);
        assert_eq!(
            table.suffixes[last as usize - 1],
            [7, 8, 9, 10, 11, 12, 13, 14]
        );
        assert_eq!(table.chain[last as usize - 1].prev, last - 9);
        assert_eq!(table.suffixes[last as usize - 9], [1, 0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(table.chain[last as usize - 9].prev, 0);

        let mut out = [0; 17];
        table.reconstruct(last, &mut out);
        assert_eq!(out[..8], [1, 0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(out[8..16], [7, 8, 9, 10, 11, 12, 13, 14]);
        assert_eq!(out[16..], [15]);

        out.fill(0x42);
        table.reconstruct(last - 1, &mut out[..16]);
        assert_eq!(out[..8], [1, 0, 1, 2, 3, 4, 5, 6]);
        assert_eq!(out[8..16], [7, 8, 9, 10, 11, 12, 13, 14]);
    }
}
