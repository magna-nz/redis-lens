use std::io::Write as _;

use proptest::prelude::*;
use unicode_width::UnicodeWidthChar;

use super::*;

// ---- helpers -------------------------------------------------------------------

type Compressor = fn(&[u8]) -> Vec<u8>;

fn gzip(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn zlib(data: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn lz4(data: &[u8]) -> Vec<u8> {
    let mut e = lz4_flex::frame::FrameEncoder::new(Vec::new());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

fn zstd(data: &[u8]) -> Vec<u8> {
    ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
}

fn msgpack(v: &rmpv::Value) -> Vec<u8> {
    let mut out = Vec::new();
    rmpv::encode::write_value(&mut out, v).unwrap();
    out
}

fn unsafe_char(c: char) -> bool {
    c.is_control() || is_bidi_control(c)
}

/// Panics (without printing the text) when `s` holds a control or bidi character.
fn assert_safe(s: &str, what: &str) {
    assert!(!s.chars().any(unsafe_char), "{what}: unsafe character");
}

fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn lines_of(raw: &[u8], format: Format, width: usize) -> Vec<String> {
    render(&decode(raw), format, width).lines().to_vec()
}

const SAMPLE_JSON: &[u8] = br#"{"name":"lens","tags":["a","b"],"n":1}"#;

// ---- existing contract tests ---------------------------------------------------

#[test]
fn raw_debug_never_shows_the_value() {
    let r = Raw::from("s3cret-value");
    assert_eq!(format!("{r:?}"), "Raw(<12 bytes>)");
    let d = decode(r.as_bytes());
    assert!(!format!("{d:?}").contains("s3cret"));
    assert!(!format!("{:?}", render(&d, Format::Text, 80)).contains("s3cret"));
}

#[test]
fn escape_draws_controls_as_escapes() {
    assert_eq!(escape("a\x1b[2Jb"), "a\\x1b[2Jb");
    assert_eq!(escape("l1\nl2\tx\r"), "l1\\nl2\\tx\\r");
    assert_eq!(escape("\u{9b}31m"), "\\u{9b}31m");
    assert_eq!(escape("ab\u{202e}cd"), "ab\\u{202e}cd");
    assert_eq!(escape("plain \\ text é"), "plain \\ text é");
}

#[test]
fn escape_lengths_match_what_escape_writes() {
    for c in (0u32..0x3000).filter_map(char::from_u32) {
        let e = escape(&c.to_string());
        match escaped_len(c) {
            Some(n) => {
                assert_eq!(e.len(), n, "U+{:04X}", c as u32);
                assert!(n <= MAX_ESCAPE_WIDTH);
            }
            None => assert_eq!(e, c.to_string()),
        }
    }
    assert_eq!(
        escape("\u{2069}\u{61c}\x7f\0"),
        "\\u{2069}\\u{61c}\\x7f\\x00"
    );
}

#[test]
fn debug_never_shows_the_value_in_any_format() {
    let marker = "zq-MARKER-7731";
    let text = format!("hello {marker}");
    let json = format!(r#"{{"k":"{marker}"}}"#);
    let mp = msgpack(&rmpv::Value::Map(vec![(
        rmpv::Value::from(marker),
        rmpv::Value::Binary(vec![0xff]),
    )]));
    for raw in [
        text.as_bytes().to_vec(),
        json.as_bytes().to_vec(),
        mp,
        gzip(json.as_bytes()),
    ] {
        let d = decode(&raw);
        assert!(!format!("{d:?}").contains(marker));
        assert!(!format!("{:?}", Raw::from(raw.as_slice())).contains(marker));
        for f in Format::ALL {
            let r = render(&d, f, 80);
            assert!(!format!("{r:?}").contains(marker));
        }
    }
}

// ---- decompression -------------------------------------------------------------

#[test]
fn each_compression_round_trips() {
    let cases: [(Compression, Compressor); 4] = [
        (Compression::Gzip, gzip),
        (Compression::Zlib, zlib),
        (Compression::Lz4, lz4),
        (Compression::Zstd, zstd),
    ];
    for (c, compress) in cases {
        let packed = compress(SAMPLE_JSON);
        assert_ne!(packed.as_slice(), SAMPLE_JSON);
        let d = decode(&packed);
        assert_eq!(d.compression, Some(c), "{}", c.label());
        assert!(!d.truncated);
        assert_eq!(d.bytes(), SAMPLE_JSON);
        assert_eq!(d.detected, Format::Json);
        // Only one layer is undone.
        let twice = decode(&compress(&packed));
        assert_eq!(twice.compression, Some(c));
        assert_eq!(twice.bytes(), packed.as_slice());
    }
}

#[test]
fn empty_compressed_value_decodes_to_nothing() {
    let d = decode(&gzip(b""));
    assert_eq!(d.compression, Some(Compression::Gzip));
    assert!(d.bytes().is_empty());
    assert_eq!(d.detected, Format::Text);
    assert!(render(&d, Format::Hex, 80).lines().is_empty());
}

#[test]
fn a_gzip_bomb_stops_at_the_cap() {
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let zeros = vec![0u8; 1024 * 1024];
    for _ in 0..50 {
        e.write_all(&zeros).unwrap();
    }
    let bomb = e.finish().unwrap();
    assert!(bomb.len() < MAX_DECOMPRESSED / 4);
    let d = decode(&bomb);
    assert_eq!(d.compression, Some(Compression::Gzip));
    assert!(d.truncated);
    assert_eq!(d.bytes().len(), MAX_DECOMPRESSED);
}

#[test]
fn an_lz4_and_zstd_bomb_stop_at_the_cap() {
    let big = vec![b'a'; 3 * MAX_DECOMPRESSED];
    for packed in [lz4(&big), zstd(&big)] {
        let d = decode(&packed);
        assert!(d.compression.is_some());
        assert!(d.truncated);
        assert_eq!(d.bytes().len(), MAX_DECOMPRESSED);
        assert_eq!(d.detected, Format::Text);
    }
}

#[test]
fn capped_read_never_reads_the_whole_stream() {
    // An endless stream: reading it all would never finish.
    let (bytes, truncated) = compress::read_capped(std::io::repeat(7)).unwrap();
    assert!(truncated);
    assert_eq!(bytes.len(), MAX_DECOMPRESSED);
    assert!(bytes.capacity() <= 2 * (MAX_DECOMPRESSED + 1));
}

#[test]
fn a_zstd_header_asking_for_a_huge_window_is_left_raw() {
    // Frame header descriptor 0 (no content size, not single segment), window
    // descriptor 0xa0: window log 30, 1 GiB.
    let raw = [0x28, 0xb5, 0x2f, 0xfd, 0x00, 0xa0, 0x01, 0x00, 0x00];
    let d = decode(&raw);
    assert_eq!(d.compression, None);
    assert_eq!(d.bytes(), raw.as_slice());
}

#[test]
fn magic_bytes_that_do_not_decompress_stay_raw() {
    for raw in [
        &b"\x1f\x8bnot gzip at all"[..],
        b"\x28\xb5\x2f\xfdjunk",
        b"\x04\x22\x4d\x18junk",
        b"x\x9cjunk",
        b"x^ is text that looks like a zlib header",
        b"\x1f\x8b",
    ] {
        let d = decode(raw);
        assert_eq!(d.compression, None);
        assert!(!d.truncated);
        assert_eq!(d.bytes(), raw);
    }
    // A cut-off compressed value (a 64 KiB GETRANGE of a bigger one) stays raw too.
    let packed = gzip(
        &(0..200_000u32)
            .flat_map(|n| n.to_le_bytes())
            .collect::<Vec<_>>(),
    );
    let cut = &packed[..packed.len() / 2];
    assert_eq!(decode(cut).compression, None);
}

#[test]
fn text_cut_mid_character_by_the_cap_is_still_text() {
    let mut s = String::from("a");
    while s.len() <= MAX_DECOMPRESSED + 10 {
        s.push('é');
    }
    let d = decode(&gzip(s.as_bytes()));
    assert!(d.truncated);
    assert!(std::str::from_utf8(d.bytes()).is_err());
    assert_eq!(d.detected, Format::Text);
    let r = render(&d, Format::Text, 1000);
    assert!(r.lines().last().unwrap().ends_with("\\xc3"));
}

// ---- detection -----------------------------------------------------------------

#[test]
fn detection() {
    let det = |b: &[u8]| decode(b).detected;
    assert_eq!(det(b"{\"a\":1}"), Format::Json);
    assert_eq!(det(b"  [1, 2]\n"), Format::Json);
    assert_eq!(det(b"[]"), Format::Json);
    for text in [
        &b"7"[..],
        b"\"hi\"",
        b"true",
        b"null",
        b"{not json",
        b"[1,",
        b"",
    ] {
        assert_eq!(det(text), Format::Text);
    }
    assert_eq!(det(b"{\"a\":1} trailing"), Format::Text);
    let map = msgpack(&rmpv::Value::Map(vec![(
        rmpv::Value::from("k"),
        rmpv::Value::from(1),
    )]));
    assert_eq!(det(&map), Format::MsgPack);
    let arr = msgpack(&rmpv::Value::Array(vec![
        rmpv::Value::Nil,
        rmpv::Value::from(2),
    ]));
    assert_eq!(det(&arr), Format::MsgPack);
    // A MessagePack scalar is not detected.
    assert_eq!(det(&msgpack(&rmpv::Value::F64(1.5))), Format::Hex);
    // MessagePack with bytes left over is not MessagePack.
    let mut more = map.clone();
    more.push(0xc1);
    assert_eq!(det(&more), Format::Hex);
    assert_eq!(det(&[0xff, 0x00, 0xfe]), Format::Hex);
    assert_eq!(det("héllo 日本".as_bytes()), Format::Text);
}

#[test]
fn msgpack_that_is_also_utf8_is_text() {
    // array16 with 0x8000 elements: 0xdc 0x80 is U+0700, then 0x00 and 'a's (fixints).
    let mut b = vec![0xdc, 0x80, 0x00];
    b.extend(std::iter::repeat_n(b'a', 0x8000));
    assert!(std::str::from_utf8(&b).is_ok());
    assert!(json::msgpack_value(&b).is_some());
    assert_eq!(decode(&b).detected, Format::Text);
}

#[test]
fn msgpack_length_claims_do_not_allocate() {
    for claim in [
        &[0xdd, 0xff, 0xff, 0xff, 0xff][..],   // array32 of 4 Gi elements
        &[0xdf, 0xff, 0xff, 0xff, 0xff, 0x01], // map32
        &[0xdb, 0xff, 0xff, 0xff, 0xff, b'a'], // str32 of 4 GiB
        &[0xc6, 0xff, 0xff, 0xff, 0xff, 0x00], // bin32
        &[0xc9, 0xff, 0xff, 0xff, 0xff, 0x05, 1], // ext32
        &[0x91, 0xdd, 0xff, 0xff, 0xff, 0xff], // nested
    ] {
        let d = decode(claim);
        assert_eq!(d.detected, Format::Hex);
        let r = render(&d, Format::MsgPack, 80);
        assert_eq!(r.format, Format::Hex);
        assert_eq!(r.note, Some("not valid MessagePack, shown as hex"));
    }
}

#[test]
fn deep_nesting_does_not_overflow() {
    let mut mp = vec![0x91; 100_000];
    mp.push(0xc0);
    assert_eq!(decode(&mp).detected, Format::Hex);
    let r = render(&decode(&mp), Format::MsgPack, 80);
    assert_eq!(r.format, Format::Hex);

    // JSON deeper than 128 is not JSON (serde_json's limit), so its indents stay
    // bounded.
    let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
    let d = decode(deep.as_bytes());
    assert_eq!(d.detected, Format::Text);
    let r = render(&d, Format::Json, 80);
    assert_eq!(r.note, Some("not valid JSON, shown as text"));
    let ok = format!("{}{}", "[".repeat(128), "]".repeat(128));
    assert_eq!(decode(ok.as_bytes()).detected, Format::Json);
    let too_deep = format!("{}{}", "[".repeat(129), "]".repeat(129));
    assert_eq!(decode(too_deep.as_bytes()).detected, Format::Text);
    // Brackets inside strings do not count.
    let in_string = format!("[\"{}\"]", "[".repeat(1000));
    assert_eq!(decode(in_string.as_bytes()).detected, Format::Json);
}

#[test]
fn deep_indents_are_counted_without_drawing_them() {
    // 127 levels overall (serde_json's limit, for the reference count below), many
    // times over: lines with indents of up to 252 columns, at width 1 one line per
    // column.
    let one = format!("{}1{}", "[".repeat(126), "]".repeat(126));
    let raw = format!("[{}]", vec![one; 300].join(","));
    let d = decode(raw.as_bytes());
    assert_eq!(d.detected, Format::Json);
    let r = render(&d, Format::Json, 1);
    assert_eq!(r.lines().len(), MAX_LINES);
    // The count matches drawing it all: every pretty line wraps into one line per
    // column (an empty one still takes a line).
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let total: usize = serde_json::to_string_pretty(&v)
        .unwrap()
        .lines()
        .map(|l| l.len().max(1))
        .sum();
    let left_out = total - (MAX_LINES - 1);
    assert!(left_out > 1_000_000);
    // Width 1 shows only the `…` of the notice; at width 80 it shows the number.
    assert_eq!(r.lines()[MAX_LINES - 1], "…");
    let wide = render(&d, Format::Json, 80);
    let total_80: usize = serde_json::to_string_pretty(&v)
        .unwrap()
        .lines()
        .map(|l| l.len().div_ceil(80).max(1))
        .sum();
    assert_eq!(
        wide.lines()[MAX_LINES - 1],
        format!("… {} more lines not shown", total_80 - (MAX_LINES - 1))
    );
}

// ---- render: text --------------------------------------------------------------

#[test]
fn text_splits_at_newlines_and_escapes() {
    let r = render(&decode(b"line 1\nline\t2\r\n\x1b[31mred"), Format::Text, 80);
    assert_eq!(r.format, Format::Text);
    assert_eq!(r.note, None);
    assert_eq!(r.lines(), ["line 1", "line\\t2\\r", "\\x1b[31mred"]);
    assert_eq!(lines_of(b"a\n", Format::Text, 80), ["a", ""]);
    assert_eq!(lines_of(b"", Format::Text, 80), [""]);
}

#[test]
fn text_shows_invalid_bytes_as_hex_escapes() {
    let r = render(&decode(b"ok \xff\xfe end"), Format::Text, 80);
    assert_eq!(r.format, Format::Text);
    assert_eq!(r.lines(), ["ok \\xff\\xfe end"]);
}

#[test]
fn text_wraps_at_the_width_without_splitting_escapes() {
    assert_eq!(
        lines_of(b"abcdefghij", Format::Text, 4),
        ["abcd", "efgh", "ij"]
    );
    assert_eq!(lines_of(b"ab\x1bc", Format::Text, 4), ["ab", "\\x1b", "c"]);
    assert_eq!(lines_of(b"\x1b", Format::Text, 1), ["\\x1b"]);
    assert_eq!(
        lines_of("a\u{202e}".as_bytes(), Format::Text, 3),
        ["a", "\\u{202e}"]
    );
    // Width 0 is taken as 1.
    assert_eq!(lines_of(b"ab", Format::Text, 0), ["a", "b"]);
}

#[test]
fn wide_characters_count_two_columns() {
    assert_eq!(
        lines_of("日本語".as_bytes(), Format::Text, 4),
        ["日本", "語"]
    );
    assert_eq!(
        lines_of("日本語".as_bytes(), Format::Text, 5),
        ["日本", "語"]
    );
    assert_eq!(
        lines_of("a日b".as_bytes(), Format::Text, 2),
        ["a", "日", "b"]
    );
    // Wider than the width: a line of its own.
    assert_eq!(lines_of("日本".as_bytes(), Format::Text, 1), ["日", "本"]);
}

#[test]
fn render_stops_at_max_lines_and_says_how_many_were_left_out() {
    let many = "x\n".repeat(20_000); // 20,001 lines with the empty last one
    let r = render(&decode(many.as_bytes()), Format::Text, 80);
    assert_eq!(r.lines().len(), MAX_LINES);
    assert_eq!(r.lines()[MAX_LINES - 2], "x");
    assert_eq!(
        r.lines()[MAX_LINES - 1],
        format!("… {} more lines not shown", 20_001 - (MAX_LINES - 1))
    );

    let exact = vec!["y"; MAX_LINES].join("\n");
    let r = render(&decode(exact.as_bytes()), Format::Text, 80);
    assert_eq!(r.lines().len(), MAX_LINES);
    assert!(r.lines().iter().all(|l| l == "y"));

    // Wrapped lines count, and the notice fits the width.
    let long = "z".repeat(3 * MAX_LINES);
    let r = render(&decode(long.as_bytes()), Format::Text, 1);
    assert_eq!(r.lines().len(), MAX_LINES);
    assert_eq!(r.lines()[MAX_LINES - 1], "…");

    // Hex too.
    let r = render(&decode(&vec![0u8; 16 * MAX_LINES + 1]), Format::Hex, 80);
    assert_eq!(r.lines().len(), MAX_LINES);
    assert_eq!(r.lines()[MAX_LINES - 1], "… 2 more lines not shown");
}

// ---- render: hex ---------------------------------------------------------------

#[test]
fn hex_like_hexdump() {
    let bytes = b"hello world\n\x00\x01\x02\x03tail";
    let r = render(&decode(bytes), Format::Hex, 80);
    assert_eq!(r.format, Format::Hex);
    assert_eq!(
        r.lines(),
        [
            "00000000  68 65 6c 6c 6f 20 77 6f  72 6c 64 0a 00 01 02 03  |hello world.....|",
            "00000010  74 61 69 6c                                       |tail|",
        ]
    );
    assert_eq!(r.lines()[0].len(), 78);
    assert_eq!(r.lines()[1].len(), 78 - 12);
}

#[test]
fn hex_fits_16_8_or_4_bytes_a_line() {
    let bytes: Vec<u8> = (0u8..32).collect();
    let r = lines_of(&bytes, Format::Hex, 78);
    assert_eq!(r.len(), 2);
    let r = lines_of(&bytes, Format::Hex, 77);
    assert_eq!(r.len(), 4);
    assert_eq!(r[1], "00000008  08 09 0a 0b 0c 0d 0e 0f  |........|");
    assert_eq!(r[1].len(), 45);
    let r = lines_of(&bytes, Format::Hex, 44);
    assert_eq!(r.len(), 8);
    assert_eq!(r[0], "00000000  00 01 02 03  |....|");
    assert_eq!(r[0].len(), 29);
    // Not even 4 fit: still 4, wrapped.
    let r = lines_of(b"abcd", Format::Hex, 10);
    assert_eq!(r, ["00000000  ", "61 62 63 6", "4  |abcd|"]);
    assert!(lines_of(b"", Format::Hex, 80).is_empty());
}

// ---- render: JSON and MessagePack ----------------------------------------------

#[test]
fn json_is_pretty_printed_like_serde_json() {
    let raw = br#" {"name":"lens","tags":["a","b"],"empty":{},"none":[],"n":-1.5e3,"nested":{"x":[{"y":null}]}} "#;
    let v: serde_json::Value = serde_json::from_slice(raw).unwrap();
    let expected = serde_json::to_string_pretty(&v).unwrap();
    // serde_json's map sorts keys; this input is in sorted order only partly, so
    // compare against a key-order-insensitive re-parse plus the exact indent form.
    let r = render(&decode(raw), Format::Json, 200);
    assert_eq!(r.format, Format::Json);
    assert_eq!(r.note, None);
    let joined = r.lines().join("\n");
    let back: serde_json::Value = serde_json::from_str(&joined).unwrap();
    assert_eq!(back, v);
    assert_eq!(r.lines()[0], "{");
    assert_eq!(r.lines()[1], "  \"name\": \"lens\",");
    assert_eq!(r.lines()[2], "  \"tags\": [");
    assert_eq!(r.lines()[3], "    \"a\",");
    assert!(joined.contains("\"empty\": {},\n  \"none\": [],"));
    assert_eq!(r.lines().len(), expected.lines().count());
}

#[test]
fn json_keeps_key_order_duplicates_and_big_numbers() {
    let raw = br#"{"b":1,"a":2,"b":3,"big":123456789012345678901234567890}"#;
    assert_eq!(
        lines_of(raw, Format::Json, 80),
        [
            "{",
            "  \"b\": 1,",
            "  \"a\": 2,",
            "  \"b\": 3,",
            "  \"big\": 123456789012345678901234567890",
            "}",
        ]
    );
    assert_eq!(
        copy_text(&decode(raw), Format::Json).unwrap(),
        r#"{"b":1,"a":2,"b":3,"big":123456789012345678901234567890}"#
    );
}

#[test]
fn json_asked_for_takes_any_json_value() {
    let r = render(&decode(b"7"), Format::Json, 80);
    assert_eq!((r.format, r.note), (Format::Json, None));
    assert_eq!(r.lines(), ["7"]);
    let r = render(&decode(b"\"a\\nb\""), Format::Json, 80);
    assert_eq!(r.lines(), ["\"a\\nb\""]);
}

#[test]
fn msgpack_is_drawn_like_pretty_json() {
    use rmpv::Value;
    let v = Value::Map(vec![
        (Value::from("k"), Value::Binary(vec![0x01, 0xab])),
        (Value::from(1), Value::Ext(5, vec![0x0a, 0x0b])),
        (Value::from("f"), Value::F32(0.1)),
        (
            Value::from("list"),
            Value::Array(vec![Value::Nil, Value::from(true)]),
        ),
        (Value::Nil, Value::F64(f64::NAN)),
        (Value::from("i"), Value::from(-3)),
        (Value::from("u"), Value::from(u64::MAX)),
        (Value::from("e"), Value::Array(vec![])),
    ]);
    let mut raw = msgpack(&v);
    assert_eq!(decode(&raw).detected, Format::MsgPack);
    let r = render(&decode(&raw), Format::MsgPack, 80);
    assert_eq!((r.format, r.note), (Format::MsgPack, None));
    assert_eq!(
        r.lines(),
        [
            "{",
            "  \"k\": \"01ab\",",
            "  \"1\": \"ext(5, 0a0b)\",",
            "  \"f\": 0.1,",
            "  \"list\": [",
            "    null,",
            "    true",
            "  ],",
            "  \"null\": \"NaN\",",
            "  \"i\": -3,",
            "  \"u\": 18446744073709551615,",
            "  \"e\": []",
            "}",
        ]
    );
    let copied = copy_text(&decode(&raw), Format::MsgPack).unwrap();
    assert_eq!(
        copied,
        r#"{"k":"01ab","1":"ext(5, 0a0b)","f":0.1,"list":[null,true],"null":"NaN","i":-3,"u":18446744073709551615,"e":[]}"#
    );
    assert!(serde_json::from_str::<serde_json::Value>(&copied).is_ok());
    // Explicitly asked for, a scalar is fine.
    raw = msgpack(&Value::from("plain"));
    let r = render(&decode(&raw), Format::MsgPack, 80);
    assert_eq!((r.format, r.note), (Format::MsgPack, None));
    assert_eq!(r.lines(), ["\"plain\""]);
}

#[test]
fn formats_that_do_not_fit_fall_back_with_a_note() {
    let text = decode(b"just text");
    let bin = decode(&[0xc1, 0xff, 0x00]);
    let cases = [
        (
            &text,
            Format::Json,
            Format::Text,
            "not valid JSON, shown as text",
        ),
        (
            &bin,
            Format::Json,
            Format::Hex,
            "not valid JSON, shown as hex",
        ),
        (
            &text,
            Format::MsgPack,
            Format::Text,
            "not valid MessagePack, shown as text",
        ),
        (
            &bin,
            Format::MsgPack,
            Format::Hex,
            "not valid MessagePack, shown as hex",
        ),
    ];
    for (d, asked, used, note) in cases {
        let r = render(d, asked, 80);
        assert_eq!(r.format, used);
        assert_eq!(r.note, Some(note));
    }
    assert_eq!(render(&text, Format::Json, 80).lines(), ["just text"]);
    // Text and hex always work.
    for f in [Format::Text, Format::Hex] {
        assert_eq!(render(&bin, f, 80).note, None);
        assert_eq!(render(&bin, f, 80).format, f);
    }
    assert_eq!(render(&bin, Format::Text, 80).lines(), ["\\xc1\\xff\\x00"]);
}

// ---- terminal safety -----------------------------------------------------------

#[test]
fn hostile_values_are_safe_in_every_format() {
    let evil = "a\x1b[2J\u{9b}31m\u{202e}rtl\r\x07\x7f\u{85}b";
    let json = format!(
        r#"{{"v":"\u001b[2J {}","{}":1}}"#,
        "\u{9b}\u{202e}\x7f\u{85}", "\u{2066}k"
    );
    let mp = msgpack(&rmpv::Value::Map(vec![
        (rmpv::Value::from(evil), rmpv::Value::from(evil)),
        (
            rmpv::Value::Binary(vec![0x1b]),
            rmpv::Value::Ext(1, vec![0x9b]),
        ),
    ]));
    assert_eq!(decode(json.as_bytes()).detected, Format::Json);
    assert_eq!(decode(&mp).detected, Format::MsgPack);
    let raws = [
        evil.as_bytes().to_vec(),
        json.as_bytes().to_vec(),
        mp.clone(),
        gzip(evil.as_bytes()),
        b"\x1b[2J\x9b\xff\r\x07".to_vec(),
    ];
    for raw in &raws {
        let d = decode(raw);
        for f in Format::ALL {
            for w in [1, 5, 80] {
                for line in render(&d, f, w).lines() {
                    assert_safe(line, f.label());
                }
            }
            if let Ok(s) = copy_text(&d, f) {
                assert_safe(&s, "copy");
            }
        }
        for n in [0, 1, 3, 10, 200] {
            let p = preview(raw, n);
            assert_safe(&p, "preview");
            assert!(p.chars().count() <= n);
        }
    }
    let r = render(&decode(evil.as_bytes()), Format::Text, 200);
    assert_eq!(
        r.lines(),
        ["a\\x1b[2J\\u{9b}31m\\u{202e}rtl\\r\\x07\\x7f\\u{85}b"]
    );
    // The JSON keeps its meaning, with the raw controls as JSON escapes.
    let copied = copy_text(&decode(json.as_bytes()), Format::Json).unwrap();
    assert_eq!(
        copied,
        concat!(
            r#"{"v":"\u001b[2J \u009b"#,
            r"\u202e",
            r#"\u007f\u0085",""#,
            r"\u2066",
            r#"k":1}"#
        )
    );
    let back: serde_json::Value = serde_json::from_str(&copied).unwrap();
    assert_eq!(back["v"], "\x1b[2J \u{9b}\u{202e}\x7f\u{85}");
}

// ---- copy ----------------------------------------------------------------------

#[test]
fn copy_text_rules() {
    assert_eq!(copy_text(&decode(b"plain"), Format::Text).unwrap(), "plain");
    for refused in [
        &b"two\nlines"[..],
        b"tab\t",
        b"bidi\xe2\x80\xae",
        b"bad \xff",
    ] {
        assert_eq!(
            copy_text(&decode(refused), Format::Text),
            Err(CopyRefusal::ControlCharacters)
        );
        // Switching to hex copies it.
        let hex = copy_text(&decode(refused), Format::Hex).unwrap();
        assert!(hex.bytes().all(|b| b.is_ascii_hexdigit()));
    }
    assert_eq!(copy_text(&decode(b"hel"), Format::Hex).unwrap(), "68656c");
    assert_eq!(
        copy_text(&decode(b"{ \"a\" : [ 1 , 2 ] }\n"), Format::Json).unwrap(),
        r#"{"a":[1,2]}"#
    );
    // Falls back like render: text rules, then hex rules.
    assert_eq!(copy_text(&decode(b"txt"), Format::Json).unwrap(), "txt");
    assert_eq!(
        copy_text(&decode(b"a\nb"), Format::MsgPack),
        Err(CopyRefusal::ControlCharacters)
    );
    assert_eq!(
        copy_text(&decode(&[0xc1, 0x01]), Format::Json).unwrap(),
        "c101"
    );
    // Copies the decompressed value.
    let d = decode(&gzip(SAMPLE_JSON));
    assert_eq!(
        copy_text(&d, Format::Json).unwrap(),
        std::str::from_utf8(SAMPLE_JSON).unwrap()
    );
}

// ---- preview -------------------------------------------------------------------

#[test]
fn preview_is_one_compact_line() {
    assert_eq!(
        preview(b"{ \"a\": 1,\n \"b\": [1, 2] }", 80),
        r#"{"a":1,"b":[1,2]}"#
    );
    assert_eq!(preview(b"line1\nline2\t\x1b", 80), "line1\\nline2\\t\\x1b");
    assert_eq!(preview(&[0x68, 0x65, 0x00, 0xff], 80), "68 65 00 ff");
    assert_eq!(preview(&gzip(b"[1, 2]"), 80), "[1,2]");
    let mp = msgpack(&rmpv::Value::Array(vec![
        rmpv::Value::from(1),
        rmpv::Value::Nil,
    ]));
    assert_eq!(preview(&mp, 80), "[1,null]");
    // Not UTF-8: hex.
    assert_eq!(preview(b"bad \xff", 80), "62 61 64 20 ff");
}

#[test]
fn preview_cuts_with_an_ellipsis() {
    assert_eq!(preview(b"abcdef", 6), "abcdef");
    assert_eq!(preview(b"abcdefg", 6), "abcde…");
    assert_eq!(preview(b"abc", 0), "");
    assert_eq!(preview(b"abc", 1), "…");
    assert_eq!(preview(b"a\nb", 2), "a…");
    assert_eq!(preview(&[0xffu8; 100], 8), "ff ff f…");
    assert_eq!(preview(b"[1,2,3,4,5,6,7,8,9]", 6), "[1,2,…");
    assert_eq!(preview("日本語です".as_bytes(), 3), "日本…");
    // A large value is cut, not converted whole.
    let big = "w".repeat(MAX_DECOMPRESSED);
    assert_eq!(preview(big.as_bytes(), 4).chars().count(), 4);
    let big_json = format!("[{}1]", "1,".repeat(300_000));
    assert_eq!(preview(big_json.as_bytes(), 5), "[1,1…");
}

// ---- proptests -----------------------------------------------------------------

fn arb_msgpack() -> impl Strategy<Value = rmpv::Value> {
    use rmpv::Value;
    let leaf = prop_oneof![
        Just(Value::Nil),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<u64>().prop_map(Value::from),
        any::<f32>().prop_map(Value::F32),
        any::<f64>().prop_map(Value::F64),
        ".*".prop_map(Value::from),
        proptest::collection::vec(any::<u8>(), 0..16).prop_map(Value::Binary),
        (any::<i8>(), proptest::collection::vec(any::<u8>(), 0..8))
            .prop_map(|(t, d)| Value::Ext(t, d)),
    ];
    leaf.prop_recursive(4, 32, 6, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            proptest::collection::vec((inner.clone(), inner), 0..6).prop_map(Value::Map),
        ]
    })
}

fn arb_json() -> impl Strategy<Value = serde_json::Value> {
    use serde_json::Value;
    let leaf = prop_oneof![
        Just(Value::Null),
        any::<bool>().prop_map(Value::from),
        any::<i64>().prop_map(Value::from),
        any::<f64>().prop_map(Value::from),
        ".*".prop_map(Value::from),
    ];
    leaf.prop_recursive(4, 32, 6, |inner| {
        prop_oneof![
            proptest::collection::vec(inner.clone(), 0..6).prop_map(Value::Array),
            proptest::collection::btree_map(".*", inner, 0..6)
                .prop_map(|m| Value::Object(m.into_iter().collect())),
        ]
    })
}

/// Bytes worth throwing at the renderer: anything, any text, JSON and MessagePack.
fn arb_value_bytes() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        proptest::collection::vec(any::<u8>(), 0..400),
        ".{0,200}".prop_map(String::into_bytes),
        arb_json().prop_map(|v| serde_json::to_vec_pretty(&v).unwrap()),
        arb_msgpack().prop_map(|v| msgpack(&v)),
    ]
}

/// Each compression's magic, with a zlib header that passes the check.
const MAGICS: [&[u8]; 5] = [
    &[0x1f, 0x8b],
    &[0x28, 0xb5, 0x2f, 0xfd],
    &[0x04, 0x22, 0x4d, 0x18],
    &[0x78, 0x9c],
    &[0x78, 0x01],
];

proptest! {
    /// Lines never hold a control or bidi character, never number more than
    /// MAX_LINES, and are at most max(width, MAX_ESCAPE_WIDTH) columns wide: wrapping
    /// never splits a unit (a character or its escape), so only a unit wider than the
    /// width (an escape of up to MAX_ESCAPE_WIDTH, or a 2-column character at width 1)
    /// overflows it, and then it stands alone on its line.
    #[test]
    fn render_is_always_safe_and_bounded(raw in arb_value_bytes(), width in 1usize..120) {
        let d = decode(&raw);
        for f in Format::ALL {
            let r = render(&d, f, width);
            prop_assert!(r.lines().len() <= MAX_LINES);
            if r.note.is_none() {
                prop_assert_eq!(r.format, f);
            }
            for line in r.lines() {
                prop_assert!(!line.chars().any(unsafe_char));
                prop_assert!(display_width(line) <= width.max(MAX_ESCAPE_WIDTH));
            }
        }
    }

    #[test]
    fn preview_is_safe_and_short(raw in arb_value_bytes(), max in 0usize..60) {
        let p = preview(&raw, max);
        prop_assert!(p.chars().count() <= max);
        prop_assert!(!p.chars().any(unsafe_char));
        prop_assert!(!p.contains('\n'));
    }

    #[test]
    fn copies_never_hold_control_characters(raw in arb_value_bytes()) {
        let d = decode(&raw);
        for f in Format::ALL {
            if let Ok(s) = copy_text(&d, f) {
                prop_assert!(!s.chars().any(unsafe_char));
            }
        }
        prop_assert!(copy_text(&d, Format::Hex).is_ok());
    }

    /// JSON copied as JSON (and MessagePack copied as JSON) is valid JSON.
    #[test]
    fn json_copies_are_json(v in arb_json(), m in arb_msgpack()) {
        let raw = serde_json::to_vec_pretty(&v).unwrap();
        let copied = copy_text(&decode(&raw), Format::Json).unwrap();
        let back: serde_json::Value = serde_json::from_str(&copied).unwrap();
        prop_assert_eq!(back, v);
        let mp = msgpack(&m);
        let d = decode(&mp);
        if let Ok(s) = copy_text(&d, Format::MsgPack)
            && render(&d, Format::MsgPack, 80).note.is_none()
        {
            prop_assert!(serde_json::from_str::<serde_json::Value>(&s).is_ok());
        }
    }

    #[test]
    fn decode_never_panics(raw in proptest::collection::vec(any::<u8>(), 0..600)) {
        let d = decode(&raw);
        prop_assert!(d.bytes().len() <= MAX_DECOMPRESSED.max(raw.len()));
    }

    #[test]
    fn decode_never_panics_after_a_magic(
        k in 0usize..MAGICS.len(),
        tail in proptest::collection::vec(any::<u8>(), 0..600),
    ) {
        let mut raw = MAGICS[k].to_vec();
        raw.extend(tail);
        let d = decode(&raw);
        if d.compression.is_none() {
            prop_assert_eq!(d.bytes(), raw.as_slice());
        }
        for f in Format::ALL {
            let _ = render(&d, f, 40);
        }
    }

    /// Corrupting a real compressed value never panics either.
    #[test]
    fn decode_never_panics_on_corrupted_streams(
        k in 0usize..4,
        data in ".{0,300}",
        flips in proptest::collection::vec((any::<usize>(), any::<u8>()), 1..4),
    ) {
        let compressors: [Compressor; 4] = [gzip, zlib, lz4, zstd];
        let mut raw = compressors[k](data.as_bytes());
        for (at, x) in flips {
            let n = raw.len();
            raw[at % n] ^= x;
        }
        let _ = decode(&raw);
    }
}

/// A map whose key is a map whose key is a map …: each level used to escape the whole
/// key again, so 40 levels in about 80 bytes drew gigabytes. A key is escaped once now.
#[test]
fn nested_map_keys_stay_small() {
    let mut v = rmpv::Value::Nil;
    for _ in 0..40 {
        v = rmpv::Value::Map(vec![(v, rmpv::Value::Nil)]);
    }
    let mut bytes = Vec::new();
    rmpv::encode::write_value(&mut bytes, &v).unwrap();
    assert!(bytes.len() < 100);
    let d = decode(&bytes);
    assert_eq!(d.detected, Format::MsgPack);
    let started = std::time::Instant::now();
    let copied = copy_text(&d, Format::MsgPack).unwrap();
    assert!(copied.len() < 200, "{} bytes", copied.len());
    assert!(preview(&bytes, 80).chars().count() <= 80);
    let r = render(&d, Format::MsgPack, 80);
    assert!(r.lines().len() < 50);
    assert!(started.elapsed() < std::time::Duration::from_secs(1));
}
