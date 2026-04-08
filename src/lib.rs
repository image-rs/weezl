//! # LZW decoder and encoder
//!
//! This crates provides an `Encoder` and a `Decoder` in their respective modules. The code words
//! are written from and to bit byte slices (or streams) where it is possible to write either the
//! most or least significant bits first. The maximum possible code size is 12 bits, the smallest
//! available code size is 2 bits.
//!
//! ## Example
//!
//! These two code blocks show the compression and corresponding decompression. Note that you must
//! use the same arguments to `Encoder` and `Decoder`, otherwise the decoding might fail or produce
//! bad results.
//!
#![cfg_attr(feature = "std", doc = "```")]
#![cfg_attr(not(feature = "std"), doc = "```ignore")]
//! use weezl::{BitOrder, encode::Encoder};
//!
//! let data = b"Hello, world";
//! let compressed = Encoder::new(BitOrder::Msb, 9)
//!     .encode(data)
//!     .unwrap();
//! ```
//!
#![cfg_attr(feature = "std", doc = "```")]
#![cfg_attr(not(feature = "std"), doc = "```ignore")]
//! use weezl::{BitOrder, decode::Decoder};
//! # let compressed = b"\x80\x04\x81\x94l\x1b\x06\xf0\xb0 \x1d\xc6\xf1\xc8l\x19 \x10".to_vec();
//! # let data = b"Hello, world";
//!
//! let decompressed = Decoder::new(BitOrder::Msb, 9)
//!     .decode(&compressed)
//!     .unwrap();
//! assert_eq!(decompressed, data);
//! ```
//!
//! ## LZW Details
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
//! ## Allocations and standard library
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

/// A default buffer size for encoding/decoding buffer.
///
/// Note that this is larger than the default size for buffers (usually 4K) since each code word
/// can expand to multiple bytes. Expanding one buffer would yield multiple and require a costly
/// break in the decoding loop. Note that the decoded size can be up to quadratic in code block.
pub(crate) const STREAM_BUF_SIZE: usize = 1 << 24;

/// The order of bits in bytes.
#[derive(Clone, Copy, Debug)]
pub enum BitOrder {
    /// The most significant bit is processed first.
    Msb,
    /// The least significant bit is processed first.
    Lsb,
}

/// An owned or borrowed buffer for stream operations.
#[cfg(feature = "alloc")]
pub(crate) enum StreamBuf<'d> {
    Borrowed(&'d mut [u8]),
    Owned(crate::alloc::vec::Vec<u8>),
}

#[cold]
fn assert_decode_size(size: u8) {
    assert!(
        size <= MAX_CODESIZE,
        "Maximum code size 12 required, got {}",
        size
    );
}

#[cold]
fn assert_encode_size(size: u8) {
    assert!(size >= 2, "Minimum code size 2 required, got {}", size);
    assert!(
        size <= MAX_CODESIZE,
        "Maximum code size 12 required, got {}",
        size
    );
}

#[cfg(feature = "alloc")]
pub mod decode;
#[cfg(feature = "alloc")]
pub mod encode;
mod error;

#[cfg(feature = "std")]
pub use self::error::StreamResult;
pub use self::error::{BufferResult, LzwError, LzwStatus};

#[cfg(all(test, feature = "alloc"))]
mod tests {
    use crate::alloc::vec;
    use crate::alloc::vec::Vec;
    use crate::decode::{Configuration as DecodeConfig, Decoder, TableStrategy};
    use crate::encode::Encoder;
    use crate::BitOrder;

    #[cfg(feature = "std")]
    use crate::{decode, encode};

    #[test]
    fn stable_send() {
        fn must_be_send<T: Send + 'static>() {}
        must_be_send::<Decoder>();
        must_be_send::<Encoder>();

        #[cfg(feature = "std")]
        fn _send_and_lt<'lt, T: Send + 'lt>() {}

        // Check that the inference `W: Send + 'd` => `IntoStream: Send + 'd` works.
        #[cfg(feature = "std")]
        fn _all_send_writer<'d, W: std::io::Write + Send + 'd>() {
            _send_and_lt::<'d, decode::IntoStream<'d, W>>();
            _send_and_lt::<'d, encode::IntoStream<'d, W>>();
        }
    }

    /// Regression: the chunked table guards against index wrap-around when
    /// min_code_size = 12 (clear = 4096, end = 4097) — those would alias into
    /// alphabet entries with `& MASK` indexing.
    #[test]
    fn roundtrip_min_code_size_12() {
        let data: Vec<u8> = (0..4096u16).flat_map(|n| n.to_le_bytes()).collect();
        for &order in &[BitOrder::Msb, BitOrder::Lsb] {
            let encoded = Encoder::new(order, 12).encode(&data).unwrap();
            for &strategy in &[TableStrategy::Classic, TableStrategy::Chunked] {
                let decoded = DecodeConfig::new(order, 12)
                    .with_table_strategy(strategy)
                    .build()
                    .decode(&encoded)
                    .unwrap();
                assert_eq!(decoded, data, "order={:?} strategy={:?}", order, strategy);
            }
        }
    }

    /// Both table strategies must produce identical output on the same encoded
    /// data across every legal code size and bit order.
    #[test]
    fn roundtrip_chunked_all_sizes() {
        for size in 2u8..=12 {
            // Deterministic palette-ish payload that stays within the alphabet
            // of this code size and exercises long LZW strings.
            let alphabet_mask: u8 = if size >= 8 {
                0xFF
            } else {
                (1u16 << size).wrapping_sub(1) as u8
            };
            let mut data = vec![0u8; 16 * 1024];
            let mut state: u32 = 0x1234_5678;
            for b in data.iter_mut() {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                *b = (state >> 24) as u8 & alphabet_mask;
            }

            for &order in &[BitOrder::Msb, BitOrder::Lsb] {
                let encoded = Encoder::new(order, size).encode(&data).unwrap();
                let classic = DecodeConfig::new(order, size)
                    .with_table_strategy(TableStrategy::Classic)
                    .build()
                    .decode(&encoded)
                    .unwrap();
                let chunked = DecodeConfig::new(order, size)
                    .with_table_strategy(TableStrategy::Chunked)
                    .build()
                    .decode(&encoded)
                    .unwrap();
                assert_eq!(classic, data, "classic size={} order={:?}", size, order);
                assert_eq!(chunked, data, "chunked size={} order={:?}", size, order);
            }
        }
    }
}
