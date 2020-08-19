//! A module for all encoding needs.
use crate::{MAX_CODESIZE, MAX_ENTRIES, BitOrder, Code};
use crate::decode::{LzwError, LzwStatus, StreamResult};

use crate::alloc::{boxed::Box, vec::Vec};
#[cfg(feature = "std")]
use std::io::{self, BufRead, Write};
#[cfg(feature = "std")]
use crate::decode::AllResult;

pub struct Encoder {
    state: Box<dyn Stateful + Send + 'static>,
}

/// A encoding stream sink.
///
/// See [`Encoder::into_stream`] on how to create this type and more information.
///
/// [`Encoder::into_stream`]: struct.Encoder.html#method.into_stream
pub struct IntoStream<'d, W> {
    encoder: &'d mut Encoder,
    writer: W,
}

trait Stateful {
    fn advance(&mut self, inp: &[u8], out: &mut [u8]) -> StreamResult;
    fn mark_ended(&mut self) -> bool;
}

struct EncodeState<B: Buffer> {
    /// The configured minimal code size.
    min_size: u8,
    /// The current encoding symbol tree.
    tree: Tree,
    /// If we have pushed the end code.
    has_ended: bool,
    /// The code corresponding to the currently read characters.
    current_code: Code,
    /// The clear code for resetting the dictionary.
    clear_code: Code,
    /// The bit buffer for encoding.
    buffer: B,
}

struct MsbBuffer {
    /// The current code length.
    code_size: u8,
    /// The buffer bits.
    buffer: u64,
    /// The number of valid buffer bits.
    bits_in_buffer: u8,
}

struct LsbBuffer {
    /// The current code length.
    code_size: u8,
    /// The buffer bits.
    buffer: u64,
    /// The number of valid buffer bits.
    bits_in_buffer: u8,
}

trait Buffer {
    fn new(size: u8) -> Self;
    /// Reset the code size in the buffer.
    fn reset(&mut self, min_size: u8);
    /// Insert a code into the buffer.
    fn buffer_code(&mut self, code: Code);
    /// Push bytes if the buffer space is getting small.
    fn push_out(&mut self, out: &mut &mut [u8]) -> bool;
    /// Flush all full bytes, returning if at least one more byte remains.
    fn flush_out(&mut self, out: &mut &mut [u8]) -> bool;
    /// Pad the buffer to a full byte.
    fn buffer_pad(&mut self);
    /// Increase the maximum code size.
    fn bump_code_size(&mut self);
    /// Return the maximum code with the current code size.
    fn max_code(&self) -> Code;
    /// Return the current code size in bits.
    fn code_size(&self) -> u8;
}

/// One tree node for at most each code.
/// To avoid using too much memory we keep nodes with few successors in optimized form. This form
/// doesn't offer lookup by indexing but instead does a linear search.
#[derive(Default)]
struct Tree {
    simples: Vec<Simple>,
    complex: Vec<Full>,
    keys: Vec<CompressedKey>,
}

#[derive(Clone, Copy)]
enum FullKey {
    NoSuccessor,
    Simple(u16),
    Full(u16),
}

#[derive(Clone, Copy)]
struct CompressedKey(u16);

const SHORT: usize = 16;

#[derive(Clone, Copy)]
struct Simple {
    codes: [Code; SHORT],
    chars: [u8; SHORT],
    count: u8,
}

#[derive(Clone, Copy)]
struct Full {
    char_continuation: [Code; 256],
}

impl Encoder {
    pub fn new(order: BitOrder, size: u8) -> Self {
        type Boxed = Box<dyn Stateful + Send + 'static>;
        let state = match order {
            BitOrder::Lsb => Box::new(EncodeState::<LsbBuffer>::new(size)) as Boxed,
            BitOrder::Msb => Box::new(EncodeState::<MsbBuffer>::new(size)) as Boxed,
        };

        Encoder {
            state,
        }
    }

    /// Encode some bytes from `inp` into `out`.
    ///
    /// See [`into_stream`] for high-level functions (this interface is only available with the
    /// `std` feature) and [`finish`] for marking the input data as complete.
    ///
    /// [`into_stream`]: #method.into_stream
    /// [`finish`]: #method.finish
    pub fn encode_bytes(&mut self, inp: &[u8], out: &mut [u8]) -> StreamResult {
        self.state.advance(inp, out)
    }

    /// Construct a decoder into a writer.
    #[cfg(feature = "std")]
    pub fn into_stream<W: Write>(&mut self, writer: W) -> IntoStream<'_, W> {
        IntoStream { encoder: self, writer }
    }

    /// Mark the encoding as finished.
    ///
    /// In following calls to `encode_bytes` the encoder will try to emit an end code after
    /// encoding all of `inp`. It's not recommended, but also not unsound, to use different byte
    /// slices in different calls from this point forward. The behaviour after the end marker has
    /// been written is unspecified but again you can rely on its being sound.
    pub fn finish(&mut self) {
        self.state.mark_ended();
    }
}

#[cfg(feature = "std")]
impl<W: Write> IntoStream<'_, W> {
    /// Encode data from a reader.
    ///
    /// This will drain the supplied reader. It will not encode an end marker after all data has
    /// been processed.
    pub fn encode(&mut self, read: impl BufRead) -> AllResult {
        self.encode_part(read, false)
    }

    /// Encode data from a reader and an end marker.
    pub fn encode_all(mut self, read: impl BufRead) -> AllResult {
        self.encode_part(read, true)
    }

    fn encode_part(&mut self, mut read: impl BufRead, finish: bool) -> AllResult {
        let IntoStream { encoder, writer } = self;
        enum Progress {
            Ok,
            Done,
        }

        let mut bytes_read = 0;
        let mut bytes_written = 0;

        let read_bytes = &mut bytes_read;
        let write_bytes = &mut bytes_written;

        let mut outbuf = vec![0; 1 << 26];
        let once = move || {
            let data = read.fill_buf()?;

            if data.is_empty() {
                if finish {
                    encoder.finish();
                } else {
                    return Ok(Progress::Done)
                }
            }

            let result = encoder.encode_bytes(data, &mut outbuf[..]);
            *read_bytes += result.consumed_in;
            *write_bytes += result.consumed_out;
            read.consume(result.consumed_in);

            let done = result.status.map_err(|err| io::Error::new(
                    io::ErrorKind::InvalidData, &*format!("{:?}", err)
                ))?;

            if let LzwStatus::Done = done {
                writer.write_all(&outbuf[..result.consumed_out])?;
                return Ok(Progress::Done);
            }

            if let LzwStatus::NoProgress = done {
                return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof, "No more data but no end marker detected"
                    ));
            }

            writer.write_all(&outbuf[..result.consumed_out])?;
            Ok(Progress::Ok)
        };

        let status = core::iter::repeat_with(once)
            // scan+fuse can be replaced with map_while
            .scan((), |(), result| match result {
                Ok(Progress::Ok) => Some(Ok(())),
                Err(err) => Some(Err(err)),
                Ok(Progress::Done) => None,
            })
            .fuse()
            .collect();

        AllResult {
            bytes_read,
            bytes_written,
            status,
        }
    }
}

impl<B: Buffer> EncodeState<B> {
    fn new(min_size: u8) -> Self {
        let clear_code = 1 << min_size;
        let mut tree = Tree::default();
        tree.init(min_size);
        let mut state = EncodeState {
            min_size,
            tree,
            has_ended: false,
            current_code: clear_code,
            clear_code,
            buffer: B::new(min_size),
        };
        state.buffer_code(clear_code);
        state
    }
}

impl<B: Buffer> Stateful for EncodeState<B> {
    fn advance(&mut self, mut inp: &[u8], mut out: &mut [u8]) -> StreamResult {
        let c_in = inp.len();
        let c_out = out.len();
        let mut status = Ok(LzwStatus::Ok);

        'encoding: loop {
            if self.push_out(&mut out) {
                break;
            }

            if inp.is_empty() && self.has_ended {
                let end = self.end_code();
                if self.current_code != end {
                    if self.current_code != self.clear_code {
                        self.buffer_code(self.current_code);

                        // When reading this code, the decoder will add an extra entry to its table
                        // before reading th end code. Thusly, it may increase its code size based
                        // on this additional entry.
                        if self.tree.keys.len() + 1 > usize::from(self.buffer.max_code()) + 1
                            && self.buffer.code_size() < MAX_CODESIZE
                        {
                            self.buffer.bump_code_size();
                        }
                    }
                    self.buffer_code(end);
                    self.current_code = end;
                    self.buffer_pad();
                }

                break;
            }

            let mut next_code = None;
            let mut bytes = inp.iter();
            while let Some(&byte) = bytes.next() {
                if self.min_size < 8 && byte >= 1 << self.min_size {
                    status = Err(LzwError::InvalidCode);
                    break 'encoding;
                }

                inp = bytes.as_slice();
                match self.tree.iterate(self.current_code, byte) {
                    Ok(code) => self.current_code = code,
                    Err(_) => {
                        next_code = Some(self.current_code);

                        self.current_code = u16::from(byte);
                        break;
                    },
                }
            }

            match next_code {
                // No more bytes, no code produced.
                None => break,
                Some(code) => {
                    self.buffer_code(code);

                    if self.tree.keys.len() > usize::from(self.buffer.max_code()) + 1
                        && self.buffer.code_size() < MAX_CODESIZE
                    {
                        self.buffer.bump_code_size();
                    }

                    if self.tree.keys.len() > MAX_ENTRIES {
                        self.buffer_code(self.clear_code);
                        self.tree.reset(self.min_size);
                        self.buffer.reset(self.min_size);
                    }
                }
            }
        }

        if inp.is_empty() && self.current_code == self.clear_code + 1 {
            if !self.flush_out(&mut out) {
                status = Ok(LzwStatus::Done);
            }
        }

        StreamResult {
            consumed_in: c_in - inp.len(),
            consumed_out: c_out - out.len(),
            status,
        }
    }

    fn mark_ended(&mut self) -> bool {
        core::mem::replace(&mut self.has_ended, true)
    }
}

impl<B: Buffer> EncodeState<B> {
    fn push_out(&mut self, out: &mut &mut [u8]) -> bool {
        self.buffer.push_out(out)
    }

    fn flush_out(&mut self, out: &mut &mut [u8]) -> bool {
        self.buffer.flush_out(out)
    }

    fn end_code(&self) -> Code {
        self.clear_code + 1
    }

    fn buffer_pad(&mut self) {
        self.buffer.buffer_pad();
    }

    fn buffer_code(&mut self, code: Code) {
        self.buffer.buffer_code(code);
    }
}

impl Buffer for MsbBuffer {
    fn new(min_size: u8) -> Self {
        MsbBuffer {
            code_size: min_size + 1,
            buffer: 0,
            bits_in_buffer: 0,
        }
    }

    fn reset(&mut self, min_size: u8) {
        self.code_size = min_size + 1;
    }

    fn buffer_code(&mut self, code: Code) {
        let shift = 64 - self.bits_in_buffer - self.code_size;
        self.buffer |= u64::from(code) << shift;
        self.bits_in_buffer += self.code_size;
    }

    fn push_out(&mut self, out: &mut &mut [u8]) -> bool {
        if self.bits_in_buffer + 2*self.code_size < 64 {
            return false;
        }

        self.flush_out(out)
    }

    fn flush_out(&mut self, out: &mut &mut [u8]) -> bool {
        let want = usize::from(self.bits_in_buffer/8);
        let count = want.min((*out).len());
        let (bytes, tail) = core::mem::replace(out, &mut []).split_at_mut(count);
        *out = tail;

        for b in bytes {
            *b = ((self.buffer & 0xff00_0000_0000_0000) >> 56) as u8;
            self.buffer <<= 8;
            self.bits_in_buffer -= 8;
        }

        count < want
    }

    fn buffer_pad(&mut self) {
        let to_byte = self.bits_in_buffer.wrapping_neg() & 0x7;
        self.bits_in_buffer += to_byte;
    }

    fn bump_code_size(&mut self) {
        self.code_size += 1;
    }

    fn max_code(&self) -> Code {
        (1 << self.code_size) - 1
    }

    fn code_size(&self) -> u8 {
        self.code_size
    }
}

impl Buffer for LsbBuffer {
    fn new(min_size: u8) -> Self {
        LsbBuffer {
            code_size: min_size + 1,
            buffer: 0,
            bits_in_buffer: 0,
        }
    }

    fn reset(&mut self, min_size: u8) {
        self.code_size = min_size + 1;
    }

    fn buffer_code(&mut self, code: Code) {
        self.buffer |= u64::from(code) << self.bits_in_buffer;
        self.bits_in_buffer += self.code_size;
    }

    fn push_out(&mut self, out: &mut &mut [u8]) -> bool {
        if self.bits_in_buffer + 2*self.code_size < 64 {
            return false;
        }

        self.flush_out(out)
    }

    fn flush_out(&mut self, out: &mut &mut [u8]) -> bool {
        let want = usize::from(self.bits_in_buffer/8);
        let count = want.min((*out).len());
        let (bytes, tail) = core::mem::replace(out, &mut []).split_at_mut(count);
        *out = tail;

        for b in bytes {
            *b = (self.buffer & 0x0000_0000_0000_00ff) as u8;
            self.buffer >>= 8;
            self.bits_in_buffer -= 8;
        }

        count < want
    }

    fn buffer_pad(&mut self) {
        let to_byte = self.bits_in_buffer.wrapping_neg() & 0x7;
        self.bits_in_buffer += to_byte;
    }

    fn bump_code_size(&mut self) {
        self.code_size += 1;
    }

    fn max_code(&self) -> Code {
        (1 << self.code_size) - 1
    }

    fn code_size(&self) -> u8 {
        self.code_size
    }
}

impl Tree {
    fn init(&mut self, min_size: u8) {
        // We need a way to represent the state of a currently empty buffer. We use the clear code
        // for this, thus create one complex mapping that leads to the one-char base codes.
        self.keys.resize((1 << min_size) + 2, FullKey::NoSuccessor.into());
        self.complex.push(Full {
            char_continuation: [0; 256],
        });
        let map_of_begin = self.complex.last_mut().unwrap();
        for ch in 0u16..256 {
            map_of_begin.char_continuation[usize::from(ch)] = ch;
        }
        self.keys[1 << min_size] = FullKey::Full(0).into();
    }

    fn reset(&mut self, min_size: u8) {
        self.simples.clear();
        // Keep entry for clear code.
        self.complex.truncate(1);
        self.keys.truncate((1 << min_size) + 2);
        for k in self.keys[..(1 << min_size) + 2].iter_mut() {
            *k = FullKey::NoSuccessor.into();
        }
    }

    fn at_key(&self, code: Code, ch: u8) -> Option<Code> {
        let key = self.keys[usize::from(code)];
        match FullKey::from(key) {
            FullKey::NoSuccessor => None,
            FullKey::Simple(idx) => {
                let nexts = &self.simples[usize::from(idx)];
                let successors = nexts.codes.iter().zip(nexts.chars.iter())
                    .take(usize::from(nexts.count));
                for (&scode, &sch) in successors {
                    if sch == ch {
                        return Some(scode);
                    }
                }

                None
            },
            FullKey::Full(idx) => {
                let full = &self.complex[usize::from(idx)];
                let precode = full.char_continuation[usize::from(ch)];
                if usize::from(precode) < MAX_ENTRIES {
                    Some(precode)
                } else {
                    None
                }
            },
        }
    }

    /// Iterate to the next char.
    /// Return Ok when it was already in the tree or creates a new entry for it and returns Err.
    fn iterate(&mut self, code: Code, ch: u8) -> Result<Code, Code> {
        if let Some(next) = self.at_key(code, ch) {
            Ok(next)
        } else {
            Err(self.append(code, ch))
        }
    }

    fn append(&mut self, code: Code, ch: u8) -> Code {
        let next: Code = self.keys.len() as u16;
        let key = self.keys[usize::from(code)];
        // TODO: with debug assertions, check for non-existence
        match FullKey::from(key) {
            FullKey::NoSuccessor => {
                let new_key = FullKey::Simple(self.simples.len() as u16);
                self.simples.push(Simple::default());
                let simples = self.simples.last_mut().unwrap();
                simples.codes[0] = next;
                simples.chars[0] = ch;
                simples.count = 1;
                self.keys[usize::from(code)] = new_key.into();
            }
            FullKey::Simple(idx) if usize::from(self.simples[usize::from(idx)].count) < SHORT => {
                let nexts = &mut self.simples[usize::from(idx)];
                let nidx = usize::from(nexts.count);
                nexts.chars[nidx] = ch;
                nexts.codes[nidx] = next;
                nexts.count += 1;
            }
            FullKey::Simple(idx) => {
                let new_key = FullKey::Full(self.complex.len() as u16);
                let simples = &self.simples[usize::from(idx)];
                self.complex.push(Full {
                    char_continuation: [Code::max_value(); 256],
                });
                let full = self.complex.last_mut().unwrap();
                for (&pch, &pcont) in simples.chars.iter().zip(simples.codes.iter()) {
                    full.char_continuation[usize::from(pch)] = pcont;
                }
                self.keys[usize::from(code)] = new_key.into();
            }
            FullKey::Full(idx) => {
                let full = &mut self.complex[usize::from(idx)];
                full.char_continuation[usize::from(ch)] = next;
            }
        }
        self.keys.push(FullKey::NoSuccessor.into());
        next
    }
}

impl Default for FullKey {
    fn default() -> Self {
        FullKey::NoSuccessor
    }
}

impl Default for Simple {
    fn default() -> Self {
        Simple {
            codes: [0; SHORT],
            chars: [0; SHORT],
            count: 0,
        }
    }
}

impl From<CompressedKey> for FullKey {
    fn from(CompressedKey(key): CompressedKey) -> Self {
        match (key >> MAX_CODESIZE) & 0xf {
            0 => FullKey::Full(key & 0xfff),
            1 => FullKey::Simple(key & 0xfff),
            _ => FullKey::NoSuccessor,
        }
    }
}

impl From<FullKey> for CompressedKey {
    fn from(full: FullKey) -> Self {
        CompressedKey(match full {
            FullKey::NoSuccessor => 0x2000,
            FullKey::Simple(code) => 0x1000 | code,
            FullKey::Full(code) => code,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{BitOrder, Encoder, LzwError};

    #[test]
    fn invalid_input_rejected() {
        const BIT_LEN: u8 = 2;
        let ref input = [0, 1 << BIT_LEN /* invalid */, 0];
        let ref mut target = [0u8; 128];
        let mut encoder = Encoder::new(BitOrder::Msb, BIT_LEN);

        encoder.finish();
        // We require simulation of normality, that is byte-for-byte compression.
        let result = encoder.encode_bytes(input, target);
        assert!(if let Err(LzwError::InvalidCode) = result.status { true } else { false });
        assert_eq!(result.consumed_in, 1);
    }
}
