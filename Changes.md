## 0.1.0-beta.1

Renamed `AllResult` to `StreamResult` and `StreamResult` to `BufferResult`.
A lot of other interface compatible polish

## 0.1.0-beta.0

Fixed a bug from 0.1.0-alpha that caused decoding to skip over one word. This
might have potentially even corrupted the output of subsequent words.
