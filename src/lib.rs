//! # LZW decoder and encoder
//!
//! This crates provides a `LzwEncoder` and `LzwDecoder`. The code words are written from
//! and to bit streams where it is possible to write either the most or least significant 
//! bit first. The maximum possible code size is 16 bits. Both types rely on RAII to
//! produced correct results.
//!
//! The de- and encoder expect the LZW stream to start with a clear code and end with an
//! end code which are defined as follows:
//!
//!  * `CLEAR_CODE == 1 << min_code_size`
//!  * `END_CODE   == CLEAR_CODE + 1`
//!
//! Examplary use of the encoder:
//!
//! ```
//! use weezl::{ByteOrder, enlzw::Encoder};
//! let size = 8;
//! let data = b"TOBEORNOTTOBEORTOBEORNOT";
//! let mut compressed = vec![];
//!
//! let mut enc = Encoder::new(ByteOrder::Msb, size);
//! enc.encode_bytes(data)
//!     {
//!         let mut enc = Encoder::new(LsbWriter::new(&mut compressed), size).unwrap();
//!         enc.encode_bytes(data).unwrap();
//!     }
//! ```
pub(crate) const MAX_CODESIZE: u8 = 12;
pub(crate) const MAX_ENTRIES: usize = 1 << MAX_CODESIZE as usize;

/// Alias for a LZW code point
pub(crate) type Code = u16;

pub enum ByteOrder {
    Msb,
    Lsb,
}

pub mod enlzw;
pub mod relzw;
