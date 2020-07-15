use crate::lzw::{MAX_CODESIZE, MAX_ENTRIES, Code};
use std::io::{self, BufRead, Write};

pub struct Decoder {
    state: Box<DecodeState>,
}

#[derive(Clone)]
struct Link {
    prev: Code,
    byte: u8,
}

struct DecodeState {
    /// The original minimum code size.
    min_size: u8,

    /// The table of decoded codes.
    table: Table,

    /// The buffer of decoded data.
    buffer: Buffer,

    /// The link which we are still decoding and its original code.
    last: Option<(Code, Link)>,

    /// The current code size.
    code_size: u8,
    code_mask: u16,

    next_code: Code,

    /// Code to reset all tables.
    clear_code: Code,

    /// Code to signal the end of the stream.
    end_code: Code,

    has_ended: bool,

    bit_buffer: u64,
    bits: u8,
}

struct Buffer {
    bytes: Box<[u8]>,
    read_mark: usize,
    write_mark: usize,
}

struct Table {
    inner: Vec<Link>,
    depths: Vec<u16>,
}

pub struct StreamResult {
    pub consumed_in: usize,
    pub consumed_out: usize,
    pub status: Result<LzwStatus, LzwError>,
}

pub struct AllResult {
    /// The total number of bytes consumed from the reader.
    pub bytes_read: usize,
    /// The total number of bytes written into the writer.
    pub bytes_written: usize,
    pub status: std::io::Result<()>,
}

pub enum ByteOrder {
    Msb,
    Lsb,
}

#[derive(Debug, Clone, Copy)]
pub enum LzwStatus {
    Ok,
    NoProgress,
    Done,
}

#[derive(Debug, Clone, Copy)]
pub enum LzwError {
    InvalidCode,
}

impl Decoder {
    pub fn new(_: ByteOrder, size: u8) -> Self {
        Decoder {
            state: Box::new(DecodeState::new(size)),
        }
    }

    pub fn decode_bytes(&mut self, inp: &[u8], out: &mut [u8]) -> StreamResult {
        self.state.advance(inp, out)
    }

    pub fn decode_all(&mut self, mut read: impl BufRead, mut write: impl Write) -> AllResult {
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

            let result = self.decode_bytes(data, &mut outbuf[..]);
            *read_bytes += result.consumed_in;
            *write_bytes += result.consumed_out;
            read.consume(result.consumed_in);

            let done = result.status.map_err(|err| io::Error::new(
                    io::ErrorKind::InvalidData, &*format!("{:?}", err)
                ))?;

            if let LzwStatus::Done = done {
                write.write_all(&outbuf[..result.consumed_out])?;
                return Ok(Progress::Done);
            }

            if let LzwStatus::NoProgress = done {
                return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof, "No more data but no end marker detected"
                    ));
            }

            write.write_all(&outbuf[..result.consumed_out])?;
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

    pub fn has_ended(&self) -> bool {
        self.state.has_ended
    }
}

impl DecodeState {
    fn new(min_size: u8) -> Self {
        DecodeState {
            min_size: min_size,
            table: Table::new(),
            buffer: Buffer::new(),
            last: None,
            clear_code: 1 << min_size,
            end_code: (1 << min_size) + 1,
            next_code: (1 << min_size) + 2,
            has_ended: false,
            bit_buffer: 0,
            bits: 0,
            code_size: min_size + 1,
            code_mask: (1u16 << (min_size + 1)) - 1,
        }
    }

    fn reset_tables(&mut self) {
        self.code_size = self.min_size + 1;
        self.code_mask = (1 << self.code_size) - 1;
        self.next_code = (1 << self.min_size) + 2;
        self.table.clear(self.min_size);
    }

    fn advance(&mut self, mut inp: &[u8], mut out: &mut [u8]) -> StreamResult {
        let o_in = inp.len();
        let o_out = out.len();

        let mut code_link = None;
        let mut status = Ok(LzwStatus::Ok);

        match self.last.take() {
            // No last state? This is the first code after a reset?
            None => {
                match self.next_symbol(&mut inp) {
                    Some(code) if code > self.next_code => status = Err(LzwError::InvalidCode),
                    Some(code) if code == self.next_code => status = Err(LzwError::InvalidCode),
                    None => status = Ok(LzwStatus::NoProgress),
                    Some(init_code) => {
                        if init_code == self.clear_code {
                            self.reset_tables();
                        } else if init_code == self.end_code {
                            self.has_ended = true;
                            status = Ok(LzwStatus::Done);
                        } else if self.table.is_empty() {
                            status = Err(LzwError::InvalidCode);
                        } else {
                            self.buffer.reconstruct_low(&self.table, init_code);
                            let link = self.table.at(init_code).clone();
                            code_link = Some((init_code, link));
                        }
                    }
                }
            }
            Some(tup) => code_link = Some(tup),
        };

        let mut burst_required_for_progress = false;
        if let Some((code, link)) = code_link.take() {
            code_link = Some((code, link));
            let remain = self.buffer.buffer();
            if remain.len() > out.len() {
                if out.is_empty() {
                    status = Ok(LzwStatus::NoProgress);
                } else {
                    out.copy_from_slice(&remain[..out.len()]);
                    self.buffer.consume(out.len());
                    out = &mut [];
                }
            } else if remain.is_empty() {
                status = Ok(LzwStatus::NoProgress);
                burst_required_for_progress = true;
            } else {
                let consumed = remain.len();
                out[..consumed].copy_from_slice(remain);
                self.buffer.consume(consumed);
                out = &mut out[consumed..];
                burst_required_for_progress = false;
            }
        }

        let mut burst = [0; 6];
        let mut bytes = [0u16; 6];
        let mut target: [&mut [u8]; 6] = Default::default();
        let mut last_decoded: Option<&[u8]> = None;

        while let Some((mut code, mut link)) = code_link.take() {
            if out.is_empty() && !self.buffer.buffer().is_empty() {
                code_link = Some((code, link));
                break;
            }

            let mut burst_size = 0;

            self.refill_bits(&mut inp);
            // A burst is a sequence of decodes that are completely independent of each other. This
            // is the case if neither is an end code, a clear code, or a next code, i.e. we have
            // all of them in the decoding table and thus known their depths, and additionally if
            // we can decode them directly into the output buffer.
            for b in &mut burst {
                *b = match self.get_bits() {
                    None => break,
                    Some(code) => code,
                };

                if burst_size > 0 {
                    let len = bytes[burst_size-1];
                    let (into, tail) = out.split_at_mut(usize::from(len));
                    target[burst_size - 1] = into;
                    out = tail;
                }

                let current_code = self.next_code + burst_size as u16;
                burst_size += 1;
                if current_code == self.code_mask {
                    break;
                }

                if *b == self.clear_code || *b == self.end_code || *b >= self.next_code {
                    break;
                }

                let len = self.table.depths[usize::from(*b)];
                if out.len() < usize::from(len) {
                    break;
                }

                bytes[burst_size-1] = len;
            }

            if burst_size == 0 {
                if burst_required_for_progress {
                    status = Ok(LzwStatus::NoProgress);
                }
                code_link = Some((code, link));
                break;
            }

            burst_required_for_progress = false;
            let (&new_code, burst) = burst[..burst_size].split_last().unwrap();
            for (&burst, target) in burst.iter().zip(&mut target[..burst_size-1]) {
                let cha = self.buffer.reconstruct_direct(&self.table, burst, target);
                let new_link = self.table.derive(&link, cha, code);
                self.next_code += 1;
                code = burst;
                link = new_link;
            }

            if let Some(new_last) = target[..burst_size-1].last_mut() {
                let slice = core::mem::replace(new_last, &mut []);
                last_decoded = Some(&*slice);
            }

            if new_code == self.clear_code {
                self.reset_tables();
                last_decoded = None;
                continue;
            }

            if new_code == self.end_code {
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
                self.table.depths[usize::from(code)] + 1
            } else {
                self.table.depths[usize::from(new_code)]
            };

            // Ohh, we will need to store our current state into the buffer.
            let cha;
            let is_in_buffer;
            if usize::from(required_len) > out.len() {
                is_in_buffer = true;
                if new_code == self.next_code {
                    if let Some(last) = last_decoded.take() {
                        self.buffer.bytes[..last.len()].copy_from_slice(last);
                        self.buffer.write_mark = last.len();
                        self.buffer.read_mark = last.len();
                    }

                    cha = self.buffer.reconstruct_high();
                } else {
                    cha = self.buffer.reconstruct_low(&self.table, new_code);
                }
            } else {
                is_in_buffer = false;
                let (target, tail) = out.split_at_mut(usize::from(required_len));
                out = tail;

                if new_code == self.next_code {
                    // Reconstruct high.
                    let source = match last_decoded.take() {
                        Some(last) => last,
                        None => &self.buffer.bytes[..self.buffer.write_mark],
                    };
                    cha = source[0];
                    target[..source.len()].copy_from_slice(source);
                    target[source.len()..][0] = source[0];
                } else {
                    cha = self.buffer.reconstruct_direct(&self.table, new_code, target);
                }

                last_decoded = Some(target);
            }

            if self.next_code == self.code_mask && self.code_size < MAX_CODESIZE {
                self.bump_code_size();
            }

            let new_link;
            // Each newly read code creates one new code/link based on the preceding code if we
            // have enough space to put it there.
            if !self.table.is_full() {
                let link = self.table.derive(&link, cha, code);
                self.next_code += 1;
                new_link = link;
            } else {
                new_link = link.clone();
            }

            code_link = Some((new_code, new_link));

            if is_in_buffer {
                break;
            }
        }

        if let Some(tail) = last_decoded {
            self.buffer.bytes[..tail.len()].copy_from_slice(tail);
            self.buffer.write_mark = tail.len();
            self.buffer.read_mark = tail.len();
        }

        if o_in > inp.len() {
            if let Ok(LzwStatus::NoProgress) = status {
                status = Ok(LzwStatus::Ok);
            }
        }

        // Store the code/link state.
        self.last = code_link;

        StreamResult {
            consumed_in: o_in.wrapping_sub(inp.len()),
            consumed_out: o_out.wrapping_sub(out.len()),
            status,
        }
    }

    fn next_symbol(&mut self, inp: &mut &[u8]) -> Option<Code> {
        if self.bits < self.code_size {
            self.refill_bits(inp);
        }

        self.get_bits()
    }

    fn bump_code_size(&mut self) {
        self.code_size += 1;
        self.code_mask = (self.code_mask << 1) | 1;
    }

    fn refill_bits(&mut self, inp: &mut &[u8]) {
        // TODO: handle lsb?
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

    fn get_bits(&mut self) -> Option<Code> {
        if self.bits < self.code_size {
            return None;
        }

        let mask = u64::from(self.code_mask);
        let rotbuf = self.bit_buffer.rotate_left(self.code_size.into());
        self.bit_buffer = rotbuf & !mask;
        self.bits -= self.code_size;
        Some((rotbuf & mask) as u16)
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

    fn reconstruct_high(&mut self) -> u8 {
        self.bytes[self.write_mark] = self.bytes[0];
        self.write_mark += 1;
        self.read_mark = 0;
        self.bytes[0]
    }

    fn reconstruct_low(&mut self, table: &Table, code: Code) -> u8 {
        self.write_mark = 0;
        self.read_mark = 0;
        let depth = table.depths[usize::from(code)];
        let mut memory = core::mem::take(&mut self.bytes);

        let out = &mut memory[..usize::from(depth)];
        let last = self.reconstruct_direct(table, code, out);

        self.bytes = memory;
        self.write_mark = usize::from(depth);
        last
    }

    fn reconstruct_direct(&mut self, table: &Table, code: Code, out: &mut [u8]) -> u8 {
        let mut code_iter = code;
        let table = &table.inner[..=usize::from(code)];
        let len = code_iter;
        for ch in out.iter_mut().rev() {
            //(code, cha) = self.table[k as usize];
            // Note: This could possibly be replaced with an unchecked array access if
            //  - value is asserted to be < self.next_code() in push
            //  - min_size is asserted to be < MAX_CODESIZE 
            let entry = &table[usize::from(code_iter)];
            code_iter = core::cmp::min(len, entry.prev);
            *ch = entry.byte;
        }
        out[0]
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
            inner: Vec::with_capacity(MAX_ENTRIES),
            depths: Vec::with_capacity(MAX_ENTRIES),
        }
    }

    fn clear(&mut self, min_size: u8) {
        self.inner.clear();
        self.depths.clear();
        for i in 0..(1u16 << u16::from(min_size)) {
            self.inner.push(Link::base(i as u8));
            self.depths.push(1);
        }
        // Clear code.
        self.inner.push(Link::base(0));
        self.depths.push(0);
        // End code.
        self.inner.push(Link::base(0));
        self.depths.push(0);
    }

    fn at(&self, code: Code) -> &Link {
        &self.inner[usize::from(code)]
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    fn is_full(&self) -> bool {
        self.inner.len() >= MAX_ENTRIES
    }

    fn derive(&mut self, from: &Link, byte: u8, prev: Code) -> Link {
        let link = from.derive(byte, prev);
        let depth = self.depths[usize::from(prev)] + 1;
        self.inner.push(link.clone());
        self.depths.push(depth);
        link
    }
}

impl Link {
    fn base(byte: u8) -> Self {
        Link { prev: 0, byte }
    }

    // TODO: this has self type to make it clear we might depend on the old in a future
    // optimization. However, that has no practical purpose right now.
    fn derive(&self, byte: u8, prev: Code) -> Self {
        Link { prev, byte }
    }
}
