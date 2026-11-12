//! Undoing one layer of compression, recognised by its magic bytes, with the output
//! capped at [`MAX_DECOMPRESSED`] while it streams.

use std::io::{self, Read};

use super::{Compression, MAX_DECOMPRESSED};

/// The largest zstd window accepted. The decoder reserves the window a frame header
/// asks for before it decodes anything, so a ten-byte header could otherwise make it
/// reserve 128 MiB. zstd's own levels up to 19 use at most 8 MiB.
const MAX_ZSTD_WINDOW: u64 = 8 * 1024 * 1024;

/// The compression `raw` starts with, by its magic bytes.
pub(super) fn sniff(raw: &[u8]) -> Option<Compression> {
    match raw {
        [0x1f, 0x8b, ..] => Some(Compression::Gzip),
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Some(Compression::Zstd),
        [0x04, 0x22, 0x4d, 0x18, ..] => Some(Compression::Lz4),
        [0x78, b1, ..] if u16::from_be_bytes([0x78, *b1]).is_multiple_of(31) => {
            Some(Compression::Zlib)
        }
        _ => None,
    }
}

/// The decompressed bytes and whether they were cut at [`MAX_DECOMPRESSED`], or `None`
/// when `raw` has no known magic or does not decompress.
pub(super) fn decompress(raw: &[u8]) -> Option<(Compression, Vec<u8>, bool)> {
    let c = sniff(raw)?;
    let out = match c {
        Compression::Gzip => read_capped(flate2::read::GzDecoder::new(raw)),
        Compression::Zlib => read_capped(flate2::read::ZlibDecoder::new(raw)),
        Compression::Lz4 => read_capped(lz4_flex::frame::FrameDecoder::new(raw)),
        Compression::Zstd => {
            let dec =
                ruzstd::decoding::StreamingDecoder::new_with_max_window_size(raw, MAX_ZSTD_WINDOW)
                    .ok()?;
            read_capped(dec)
        }
    };
    let (bytes, truncated) = out.ok()?;
    Some((c, bytes, truncated))
}

/// Reads at most [`MAX_DECOMPRESSED`] bytes from `r` (and one more, to know whether
/// there was more), never the whole stream.
pub(super) fn read_capped(r: impl Read) -> io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::new();
    r.take(MAX_DECOMPRESSED as u64 + 1).read_to_end(&mut buf)?;
    let truncated = buf.len() > MAX_DECOMPRESSED;
    buf.truncate(MAX_DECOMPRESSED);
    Ok((buf, truncated))
}
