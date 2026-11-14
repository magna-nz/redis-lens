//! A key's cluster slot, computed here (spec "Cluster live views (P4)", Routing): CRC16
//! (XMODEM) of the key, or of its `{hash tag}`, modulo 16384. redis-lens never asks the
//! server with `CLUSTER KEYSLOT`.
//!
//! Owner: connections.

/// How many slots a cluster has.
pub const SLOTS: u16 = 16_384;

/// The slot `key` hashes to.
pub fn key_slot(key: &[u8]) -> u16 {
    crc16(hash_part(key)) % SLOTS
}

/// The part of `key` that is hashed: what is between the first `{` and the first `}`
/// after it, when that is not empty; otherwise the whole key.
pub fn hash_part(key: &[u8]) -> &[u8] {
    let Some(open) = key.iter().position(|&b| b == b'{') else {
        return key;
    };
    let rest = &key[open + 1..];
    match rest.iter().position(|&b| b == b'}') {
        Some(close) if close > 0 => &rest[..close],
        _ => key,
    }
}

/// CRC16-CCITT (XMODEM): polynomial 0x1021, initial value 0, no reflection, the one
/// Redis Cluster uses.
pub fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &b in data {
        crc ^= u16::from(b) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn crc16_is_xmodem() {
        assert_eq!(crc16(b"123456789"), 0x31C3);
        assert_eq!(key_slot(b"123456789"), 12739);
        assert_eq!(crc16(b""), 0);
    }

    #[test]
    fn slots_match_redis() {
        // `CLUSTER KEYSLOT` on a real server.
        assert_eq!(key_slot(b"foo"), 12182);
        assert_eq!(key_slot(b"bar"), 5061);
    }

    #[test]
    fn hash_tags() {
        assert_eq!(
            key_slot(b"{user1000}.following"),
            key_slot(b"{user1000}.followers")
        );
        assert_eq!(key_slot(b"{user1000}.following"), key_slot(b"user1000"));
        // An empty tag: the whole key.
        assert_eq!(hash_part(b"foo{}{bar}"), b"foo{}{bar}");
        // The first `{` and the first `}` after it.
        assert_eq!(hash_part(b"foo{{bar}}zap"), b"{bar");
        assert_eq!(hash_part(b"foo{bar}{zap}"), b"bar");
        // No closing brace.
        assert_eq!(hash_part(b"foo{bar"), b"foo{bar");
        assert_eq!(hash_part(b"}foo{"), b"}foo{");
    }

    proptest! {
        #[test]
        fn a_slot_is_always_in_range(key in proptest::collection::vec(any::<u8>(), 0..64)) {
            prop_assert!(key_slot(&key) < SLOTS);
        }

        #[test]
        fn keys_with_the_same_tag_share_a_slot(
            tag in "[a-z0-9:]{1,12}",
            a in "[a-z0-9:{]{0,8}",
            b in "[a-z0-9:{}]{0,8}",
        ) {
            // `a` holds no `}`, so the tag is the first complete one in each key.
            let x = format!("{a}{{{tag}}}{b}");
            let y = format!("{a}{{{tag}}}");
            prop_assume!(!a.contains('{'));
            prop_assert_eq!(key_slot(x.as_bytes()), key_slot(y.as_bytes()));
            prop_assert_eq!(key_slot(x.as_bytes()), key_slot(tag.as_bytes()));
        }
    }
}
