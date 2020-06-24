use crate::lzw::{MAX_CODESIZE, MAX_ENTRIES, Code};

pub struct Decoder {
    state: Box<DecodeState>,
}

#[derive(Clone)]
struct Link {
    prev: Option<Code>,
    byte: u8,
}

struct DecodeState {
    /// The original minimum code size.
    min_size: u8,

    /// The table of decoded codes.
    table: Vec<Link>,

    /// The buffer of decoded data.
    buffer: Buffer,

    /// The link which we are still decoding and its original code.
    last: Option<(Code, Link)>,

    /// The current code size.
    code_size: u8,

    next_code: Code,

    /// Code to reset all tables.
    clear_code: Code,

    /// Code to signal the end of the stream.
    end_code: Code,

    has_ended: bool,

    bit_buffer: u32,
    bits: u8,
}

struct Buffer {
    bytes: Box<[u8]>,
    read_mark: usize,
    write_mark: usize,
}

pub struct StreamResult {
    pub consumed_in: usize,
    pub consumed_out: usize,
    pub status: Result<LzwStatus, LzwError>,
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

    pub fn has_ended(&self) -> bool {
        self.state.has_ended
    }
}

impl DecodeState {
    fn new(min_size: u8) -> Self {
        DecodeState {
            min_size: min_size,
            table: Vec::with_capacity(512),
            buffer: Buffer::new(),
            last: None,
            clear_code: 1 << min_size,
            end_code: (1 << min_size) + 1,
            next_code: (1 << min_size) + 2,
            has_ended: false,
            bit_buffer: 0,
            bits: 0,
            code_size: min_size + 1,
        }
    }

    fn reset_tables(&mut self) {
        self.table.clear();
        self.code_size = self.min_size + 1;
        self.next_code = (1 << self.min_size) + 2;
        for i in 0..(1u16 << u16::from(self.min_size)) {
            self.table.push(Link::base(i as u8));
        }
        // Clear code.
        self.table.push(Link::base(0));
        // End code.
        self.table.push(Link::base(0));
    }

    fn advance(&mut self, mut inp: &[u8], mut out: &mut [u8]) -> StreamResult {
        let o_in = inp.len();
        let o_out = out.len();

        let mut code_link = None;
        let mut status = Ok(LzwStatus::Ok);

        match self.last.take() {
            // No last state? This is the first code after a reset?
            None => {
                match self.refill_bits(&mut inp) {
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
                            let link = self.table[usize::from(init_code)].clone();
                            code_link = Some((init_code, link));
                        }
                    }
                }
            }
            Some(tup) => code_link = Some(tup),
        };

        while let Some((code, link)) = code_link.take() {
            let remain = self.buffer.buffer();
            if remain.len() > out.len() {
                if out.is_empty() {
                    status = Ok(LzwStatus::NoProgress);
                } else {
                    out.copy_from_slice(&remain[..out.len()]);
                    self.buffer.consume(out.len());
                    out = &mut [];
                }
                code_link = Some((code, link));
                break;
            }

            let consumed = remain.len();
            out[..consumed].copy_from_slice(remain);
            self.buffer.consume(consumed);
            out = &mut out[consumed..];

            let new_code = match self.refill_bits(&mut inp) {
                Some(code) => code,
                // Not enough input available.
                None => {
                    if consumed == 0 {
                        status = Ok(LzwStatus::NoProgress);
                    }
                    code_link = Some((code, link));
                    break;
                }
            };

            if new_code == self.clear_code {
                self.reset_tables();
                continue;
            }

            if new_code == self.end_code {
                status = Ok(LzwStatus::Done);
                break;
            }

            if new_code > self.next_code {
                status = Err(LzwError::InvalidCode);
                break;
            }

            // Each newly read code creates one new code/link based on the preceding code.
            let cha;
            if new_code == self.next_code {
                cha = self.buffer.reconstruct_high();
            } else {
                cha = self.buffer.reconstruct_low(&self.table, new_code);
            }

            if self.next_code == (1u16 << self.code_size) - 1 && self.code_size < MAX_CODESIZE {
                self.code_size += 1;
            }

            let new_link;
            if self.table.len() < MAX_ENTRIES {
                let link = Link::derive(cha, code);
                self.table.push(link.clone());
                self.next_code += 1;
                new_link = link;
            } else {
                new_link = link.clone();
            }

            code_link = Some((new_code, new_link));
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

    fn refill_bits(&mut self, inp: &mut &[u8]) -> Option<Code> {
        if self.bits < self.code_size {
            // TODO: handle lsb?
            let wish_count = (32 - self.bits) / 8;
            let mut buffer = [0u8; 4];
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
            self.bit_buffer |= u32::from_be_bytes(buffer) >> self.bits;
            self.bits += new_bits;

            if self.bits < self.code_size {
                return None;
            }
        }
        let mask = (1 << self.code_size) - 1;
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

    fn reconstruct_low(&mut self, table: &[Link], code: Code) -> u8 {
        let mut code_iter = Some(code);
        self.write_mark = 0;
        self.read_mark = 0;

        while let Some(k) = code_iter {
            //(code, cha) = self.table[k as usize];
            // Note: This could possibly be replaced with an unchecked array access if
            //  - value is asserted to be < self.next_code() in push
            //  - min_size is asserted to be < MAX_CODESIZE 
            let entry = &table[k as usize];
            code_iter = entry.prev;
            self.bytes[self.write_mark] = entry.byte;
            self.write_mark += 1;
        }

        self.bytes[..self.write_mark].reverse();
        self.bytes[0]
    }

    fn buffer(&self) -> &[u8] {
        &self.bytes[self.read_mark..self.write_mark]
    }

    fn consume(&mut self, amt: usize) {
        self.read_mark += amt;
    }
}

impl Link {
    fn base(byte: u8) -> Self {
        Link { prev: None, byte }
    }

    fn derive(byte: u8, prev: Code) -> Self {
        Link { prev: Some(prev), byte }
    }
}
