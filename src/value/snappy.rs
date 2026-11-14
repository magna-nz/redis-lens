//! Snappy decompression, raw blocks and the framing format, with the output capped at
//! a limit as it is written. Written here rather than taken from a crate so that a
//! block's output can stop at the cap (a raw block can claim 4 GiB in five bytes) and
//! so nothing is allocated by a length the input claims.
//!
//! The framing format's chunks carry a masked CRC-32C of their data, which is checked:
//! a frame that fails it is not Snappy.

/// The framing format's stream identifier chunk.
pub(super) const FRAMED_MAGIC: [u8; 10] =
    [0xff, 0x06, 0x00, 0x00, b's', b'N', b'a', b'P', b'p', b'Y'];

/// The most a framed chunk's data decompresses to.
const MAX_CHUNK: usize = 65_536;

/// A raw Snappy block, decompressed: up to `cap` bytes, and whether there were more.
/// `None` unless the whole block is valid up to where it stopped: every element in
/// bounds, and (when not cut) exactly the length its header says.
pub(super) fn raw(input: &[u8], cap: usize) -> Option<(Vec<u8>, bool)> {
    let mut i = 0;
    let n = varint32(input, &mut i)? as usize;
    let mut out = Vec::new();
    let truncated = block(&input[i..], n, cap, &mut out)?;
    Some((out, truncated))
}

/// Decodes the elements of a block whose header says `n` bytes into `out`, stopping
/// at `cap` bytes of `out`. `Some(true)` when stopped at the cap.
fn block(b: &[u8], n: usize, cap: usize, out: &mut Vec<u8>) -> Option<bool> {
    let start = out.len();
    let mut i = 0;
    while i < b.len() {
        let tag = b[i];
        i += 1;
        let (len, offset) = match tag & 3 {
            0 => {
                let mut len = usize::from(tag >> 2);
                if len >= 60 {
                    let extra = len - 59;
                    let bytes = b.get(i..i + extra)?;
                    i += extra;
                    let mut v = 0usize;
                    for (k, &x) in bytes.iter().enumerate() {
                        v |= usize::from(x) << (8 * k);
                    }
                    len = v;
                }
                let len = len.checked_add(1)?;
                let lit = b.get(i..i.checked_add(len)?)?;
                i += len;
                if out.len() - start + len > n {
                    return None;
                }
                let room = cap.saturating_sub(out.len());
                out.extend_from_slice(&lit[..len.min(room)]);
                if len > room {
                    return Some(true);
                }
                continue;
            }
            1 => {
                let x = *b.get(i)?;
                i += 1;
                (
                    4 + usize::from((tag >> 2) & 7),
                    (usize::from(tag >> 5) << 8) | usize::from(x),
                )
            }
            2 => {
                let x = b.get(i..i + 2)?;
                i += 2;
                (
                    1 + usize::from(tag >> 2),
                    usize::from(u16::from_le_bytes([x[0], x[1]])),
                )
            }
            _ => {
                let x = b.get(i..i + 4)?;
                i += 4;
                (
                    1 + usize::from(tag >> 2),
                    u32::from_le_bytes([x[0], x[1], x[2], x[3]]) as usize,
                )
            }
        };
        let written = out.len() - start;
        if offset == 0 || offset > written || written + len > n {
            return None;
        }
        for _ in 0..len {
            if out.len() >= cap {
                return Some(true);
            }
            let b = out[out.len() - offset];
            out.push(b);
        }
    }
    (out.len() - start == n).then_some(false)
}

fn varint32(b: &[u8], i: &mut usize) -> Option<u32> {
    let mut v: u32 = 0;
    for k in 0..5 {
        let x = *b.get(*i)?;
        *i += 1;
        if k == 4 && x > 0x0f {
            return None;
        }
        v |= u32::from(x & 0x7f) << (7 * k);
        if x & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

/// A Snappy framed stream, decompressed: up to `cap` bytes, and whether there were
/// more. `None` when it does not start with the stream identifier, a chunk is invalid
/// or its checksum does not match.
pub(super) fn framed(input: &[u8], cap: usize) -> Option<(Vec<u8>, bool)> {
    if !input.starts_with(&FRAMED_MAGIC) {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 0;
    let mut chunk = Vec::new();
    while i < input.len() {
        let head = input.get(i..i + 4)?;
        let kind = head[0];
        let len = usize::from(head[1]) | usize::from(head[2]) << 8 | usize::from(head[3]) << 16;
        i += 4;
        let data = input.get(i..i.checked_add(len)?)?;
        i += len;
        match kind {
            0xff => {
                if data != &FRAMED_MAGIC[4..] {
                    return None;
                }
            }
            0x00 | 0x01 => {
                let (crc, body) = data.split_first_chunk::<4>()?;
                let want = u32::from_le_bytes(*crc);
                chunk.clear();
                let plain: &[u8] = if kind == 0x00 {
                    let mut j = 0;
                    let n = varint32(body, &mut j)? as usize;
                    if n > MAX_CHUNK || block(&body[j..], n, MAX_CHUNK, &mut chunk)? {
                        return None;
                    }
                    &chunk
                } else {
                    if body.len() > MAX_CHUNK {
                        return None;
                    }
                    body
                };
                if masked_crc32c(plain) != want {
                    return None;
                }
                let room = cap.saturating_sub(out.len());
                out.extend_from_slice(&plain[..plain.len().min(room)]);
                if plain.len() > room {
                    return Some((out, true));
                }
            }
            // Reserved unskippable chunks.
            0x02..=0x7f => return None,
            // Padding and reserved skippable chunks.
            _ => {}
        }
    }
    Some((out, false))
}

/// CRC-32C (Castagnoli), reflected, one table.
const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut n = 0;
    while n < 256 {
        let mut c = n as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0x82f6_3b78 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[n] = c;
        n += 1;
    }
    t
};

fn crc32c(b: &[u8]) -> u32 {
    let mut c = !0u32;
    for &x in b {
        c = CRC_TABLE[((c ^ u32::from(x)) & 0xff) as usize] ^ (c >> 8);
    }
    !c
}

/// The framing format's masked checksum.
fn masked_crc32c(b: &[u8]) -> u32 {
    let c = crc32c(b);
    (c.rotate_right(15)).wrapping_add(0xa282_ead8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32c_check_value() {
        assert_eq!(crc32c(b"123456789"), 0xe306_9283);
    }

    #[test]
    fn raw_blocks_decode_with_copies() {
        // "abcabcabcd": literal "abc", a copy of 6 from offset 3, literal "d".
        let b = [10, 0x08, b'a', b'b', b'c', 0x09, 0x03, 0x00, b'd'];
        assert_eq!(raw(&b, 1 << 20), Some((b"abcabcabcd".to_vec(), false)));
        // Cut at the cap.
        assert_eq!(raw(&b, 4), Some((b"abca".to_vec(), true)));
        // A wrong length, an offset before the start, a header that claims more.
        assert_eq!(
            raw(&[9, 0x08, b'a', b'b', b'c', 0x09, 0x03, 0x00, b'd'], 99),
            None
        );
        assert_eq!(raw(&[4, 0x09, 0x05, 0x00], 99), None);
        assert_eq!(raw(&[5, 0x04, b'a', b'b'], 99), None);
        assert_eq!(raw(&[], 99), None);
        assert_eq!(raw(&[0xff, 0xff, 0xff, 0xff, 0xff, 0x01], 99), None);
    }

    #[test]
    fn a_huge_claim_stops_at_the_cap_without_reserving_it() {
        // 4 GiB claimed, a one-byte literal, then copies of 64 from offset 1.
        let mut b = vec![0xff, 0xff, 0xff, 0xff, 0x0f, 0x00, b'z'];
        for _ in 0..40_000 {
            b.extend([0xfe, 0x01, 0x00]);
        }
        let (out, truncated) = raw(&b, 1 << 20).unwrap();
        assert!(truncated);
        assert_eq!(out.len(), 1 << 20);
        assert!(out.iter().all(|&x| x == b'z'));
    }

    #[test]
    fn framed_streams_check_their_checksums() {
        let data = b"hello hello hello";
        let mut s = FRAMED_MAGIC.to_vec();
        let crc = masked_crc32c(data).to_le_bytes();
        // An uncompressed chunk.
        let len = (data.len() + 4) as u32;
        s.extend([0x01, len as u8, (len >> 8) as u8, 0]);
        s.extend(crc);
        s.extend(data);
        // Padding, then a compressed chunk holding "abcabcabcd".
        s.extend([0xfe, 1, 0, 0, 0]);
        let block = [10, 0x08, b'a', b'b', b'c', 0x09, 0x03, 0x00, b'd'];
        let crc2 = masked_crc32c(b"abcabcabcd").to_le_bytes();
        s.extend([0x00, (block.len() + 4) as u8, 0, 0]);
        s.extend(crc2);
        s.extend(block);
        let mut want = data.to_vec();
        want.extend(b"abcabcabcd");
        assert_eq!(framed(&s, 1 << 20), Some((want.clone(), false)));
        assert_eq!(framed(&s, 5), Some((want[..5].to_vec(), true)));
        // A wrong checksum, an unskippable chunk, a short chunk.
        let mut bad = s.clone();
        let at = FRAMED_MAGIC.len() + 4;
        bad[at] ^= 1;
        assert_eq!(framed(&bad, 1 << 20), None);
        let mut bad = FRAMED_MAGIC.to_vec();
        bad.extend([0x02, 0, 0, 0]);
        assert_eq!(framed(&bad, 1 << 20), None);
        assert_eq!(framed(&s[..s.len() - 1], 1 << 20), None);
        assert_eq!(framed(&FRAMED_MAGIC, 99), Some((Vec::new(), false)));
    }
}
