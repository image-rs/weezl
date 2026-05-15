# Version 0.2.1

- Fix a bug that overwrote some initial entries in streams that do not use
  clear codes immediately with a full dictionary.

## Version 0.2.0

- Overhaul of the decoder algorithm, inspired by [Wuffs], to write 8 bytes at
  once and construct a tree accordingly. Many streams are now decoded with GB/s
  performance (#77). See the tradeoff in the next bullet point.
- Initialization of a decoder is now more expensive as we pre-allocate fixed
  size tables. This is so that decoding no longer performs bounds checks in
  most of its inner loops which significantly affects performance. If this is
  prohibitive for your use-case, please open an issue to discuss.
- 0 and 1 bit data-sizes are now fully supported. Note that 0-bit data uses an
  initial 2 bit code width, and 1-bit data uses 3 bits with TIFF size switch
  (#67, #78).

Fixes:
- Decoding codes with an initial code size of 12 sometimes failed to detect
  malformed streams, causing a panic or malformed output (#64).
- A small output buffer can no longer cause an erroneous `NoProgress` status
  when the progress was filling (only) the internal word buffer (#68). Decoding
  from a stream or other abstraction could silently forget data.

[Wuffs]: https://github.com/google/wuffs/tree/main/std/lzw

## Version 0.1.12

- Further adjusted a debug assertion for TIFF compatibility. It still had one
  of its OR conditions formulated incorrectly, relying on equality of the
  code word mask and the size switch code. In TIFF we hit the limit of the
  code dictionary (all 12-bit words) one code later than we would detect a
  code switch that we need to ignore.

## Version 0.1.11

- Adjusted a debug assertion that handled 1-bit code sizes incorrectly in the
  decoder (relevant for TIFF but non-standard for GIF). In that case the size
  switch semantics are a bit messy.

## Version 0.1.10

- Reverted changes made in 0.1.9 to the behavior of the decoder under non
  libtiff-compatibility mode. Trying to read the decoder with an empty output
  buffer will at least inspect the next symbol and either error or indicate the
  end-of-stream accordingly.

## Version 0.1.9 (yanked)

- Increased decoding throughput by 3—30% depending on CPU and data.
- Added `{encode,decode}::Configuration` as builder types for their respective
  module. They can be cheaply cloned.
- Added `decode::Configuration::with_yield_on_full_buffer` to explicitly opt-in
  to libtiff compatibility. The decoder will not read or interpret further
  symbols of the decoding stream when the output buffer is full. This enables a
  caller to stop fetching symbols and elide an end of stream marker based on
  out-of-band length information. The decoder might otherwise error, trying to
  interpret data that does not belong to the stream.

## Version 0.1.8

- Fixed incorrect state after `Decoder::reset`
- Added `Debug` to result types

## Version 0.1.7

- Implicit reset is now supported for decoding.

## Version 0.1.6

- Fixed an integer overflow and panic that could occur during decoding.
  Decoding performance may degrade after long sequences without a reset code.

## Version 0.1.5

- Added `IntoVec` adapters that simplify in-memory de- and encoding. A further
  'one-shot' interface is exposed in the `Decoder` and `Encoder` themselves
  which makes the process a one liner in the simplest cases. Contrary to
  `IntoStream`, these are available in all cases and do not require `std`.

## Version 0.1.4

- Added `IntoAsync` adapters for asynchronous de- and encoding. The interface
  is implemented only in terms of `futures = 0.3` traits at the moment.
- Code sizes smaller than 2 are now allowed for decoding. Since they do not
  roundtrip it is still an error to use them in the decoder but this avoids
  accidental panicking, i.e. denial of service, in parsers.

## Version 0.1.3

- Fixes an issue in compression that caused some data to be lost around clear
  codes. This could corrupt the data stream.

## Version 0.1.2

- Fixes incorrect compression after `Encoder::reset`.

## Version 0.1.1 

- The `IntoStream` types now reuse their internal buffers.
- Added the methods `set_buffer`, `set_buffer_size` to `IntoStream` for both
  the encoder and decoder, used to control the automatic allocation.
- Deprecated `IntoStream` in configurations without the `std` feature where the
  type can't even be constructed.

## Version 0.1.0 – Aleph

- Initial major release
- Support gif and tiff code size changes
- Rough performance numbers:
  On i5-4690, 8GiB DIMM DDR3 Synchronous 1600 MHz (0,6 ns)
  ~70MB/s encode, ~230MB/s decode
