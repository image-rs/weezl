//! # LZW decoder and encoder
//!
//! This crates provides an `Encoder` and a `Decoder` in their respective modules. The code words
//! are written from and to bit byte slices (or streams) where it is possible to write either the
//! most or least significant bits first. The maximum possible code size is 12 bits, the smallest
//! available code size is 2 bits.
//!
//! The de- and encoder expect the LZW stream to start with a clear code and end with an
//! end code which are defined as follows:
//!
//!  * `CLEAR_CODE == 1 << min_code_size`
//!  * `END_CODE   == CLEAR_CODE + 1`
//!
//! For optimal performance, all buffers and input and output slices should be as large as possible
//! and at least 2048 bytes long. This extends to input streams which should have similarly sized
//! buffers. This library uses Rust's standard allocation interfaces (`Box` and `Vec` to be
//! precise). Since there are no ways to handle allocation errors it is not recommended to operate
//! it on 16-bit targets.
//!
//! Exemplary use of the encoder:
//!
#![cfg_attr(feature = "std", doc="```")]
#![cfg_attr(not(feature = "std"), doc="```ignore")]
//! use weezl::{BitOrder, encode::Encoder};
//! let size = 8;
//! let data = b"TOBEORNOTTOBEORTOBEORNOT";
//! let mut compressed = vec![];
//!
//! let mut enc = Encoder::new(BitOrder::Msb, size);
//! let result = enc.into_stream(&mut compressed).encode(&data[..]);
//! result.status.unwrap();
//! ```
//!
//! The main algorithm can be used in `no_std` as well, although it requires an allocator. This
//! restriction might be lifted at a later stage. For this you should deactivate the `std` feature.
//! The main interfaces stay intact but the `into_stream` combinator is no available.
#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
#![forbid(missing_docs)]

#[cfg(all(feature = "alloc", not(feature = "std")))]
extern crate alloc;
#[cfg(all(feature = "alloc", feature = "std"))]
use std as alloc;

pub(crate) const MAX_CODESIZE: u8 = 12;
pub(crate) const MAX_ENTRIES: usize = 1 << MAX_CODESIZE as usize;

/// Alias for a LZW code point
pub(crate) type Code = u16;

/// The order of bits in bytes.
pub enum BitOrder {
    /// The most significant bit is processed first.
    Msb,
    /// The least significant bit is processed first.
    Lsb,
}

#[cfg(feature = "alloc")]
pub mod encode;
#[cfg(feature = "alloc")]
pub mod decode;
