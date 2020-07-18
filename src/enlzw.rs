//! A rebuilt encoder.
use crate::lzw::{MAX_CODESIZE, MAX_ENTRIES, Code};
use std::io::{self, BufRead, Write};

use crate::relzw::{ByteOrder, LzwStatus, LzwError, StreamResult};

pub struct Encoder {
    state: Box<dyn Stateful + Send + 'static>,
}

trait Stateful {
    fn advance(&mut self, inp: &[u8], out: &mut [u8]) -> StreamResult;
    fn mark_ended(&mut self) -> bool;
}

struct EncodeState {
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
    /// The current code length.
    code_size: u8,
    /// The buffer bits.
    buffer: u64,
    /// The number of valid buffer bits.
    bits_in_buffer: u8,
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
    pub fn new(_: ByteOrder, size: u8) -> Self {
        Encoder {
            state: Box::new(EncodeState::new(size)),
        }
    }

    pub fn encode_bytes(&mut self, inp: &[u8], out: &mut [u8]) -> StreamResult {
        self.state.advance(inp, out)
    }
}

impl EncodeState {
    fn new(min_size: u8) -> Self {
        let clear_code = 1 << min_size;
        let mut tree = Tree::default();
        tree.init(min_size);
        EncodeState {
            min_size,
            tree,
            has_ended: false,
            current_code: clear_code,
            clear_code,
            code_size: min_size + 1,
            buffer: 0,
            bits_in_buffer: 0,
        }
    }
}

impl Stateful for EncodeState {
    fn advance(&mut self, mut inp: &[u8], mut out: &mut [u8]) -> StreamResult {
        let c_in = inp.len();
        let c_out = out.len();

        loop {
            if self.push_out(&mut out) {
                break;
            }

            if inp.is_empty() && self.has_ended {
                self.buffer_code(self.clear_code + 1);
                break;
            }

            if self.tree.keys.len() == MAX_ENTRIES {
                self.buffer_code(self.clear_code);
                self.tree.reset(self.min_size);
                self.current_code = self.clear_code;
                continue;
            }

            let mut next_code = None;
            let mut bytes = inp.iter();
            while let Some(&byte) = bytes.next() {
                match self.tree.iterate(self.current_code, byte) {
                    Ok(code) => self.current_code = code,
                    Err(_) => {
                        next_code = Some(self.current_code);
                        self.current_code = self.clear_code;
                    },
                }
            }

            inp = bytes.as_slice();
            match next_code {
                // No more bytes, no code produced.
                None => break,
                Some(code) => {
                    self.buffer_code(code);
                }
            }
        }

        if inp.is_empty() && self.has_ended {
            self.flush_out(&mut out);
        }

        StreamResult {
            consumed_in: c_in - inp.len(),
            consumed_out: c_out - out.len(),
            status: Ok(LzwStatus::Ok),
        }
    }

    fn mark_ended(&mut self) -> bool {
        core::mem::replace(&mut self.has_ended, true)
    }
}

impl EncodeState {
    fn push_out(&mut self, out: &mut &mut [u8]) -> bool {
        if self.bits_in_buffer + 2*self.code_size < 64 {
            return false;
        }

        self.flush_out(out)
    }

    fn flush_out(&mut self, out: &mut &mut [u8]) -> bool {
        let want = usize::from(self.bits_in_buffer/8);
        let count = want.min((*out).len());
        let mut bytes = core::mem::replace(out, &mut []).iter_mut();

        for b in bytes.by_ref().take(count) {
            *b = (self.buffer & 0xff00_0000_0000_0000) as u8;
            self.buffer <<= 8;
            self.bits_in_buffer -= 8;
        }

        *out = bytes.into_slice();
        count < want
    }

    fn buffer_code(&mut self, code: Code) {
        let shift = 64 - self.bits_in_buffer + self.code_size;
        self.buffer |= u64::from(code) << shift;
        self.bits_in_buffer += self.code_size;
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
                    char_continuation: [0; 256],
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
