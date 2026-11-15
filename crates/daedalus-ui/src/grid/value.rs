use gpui::SharedString;

// Bounds layout cost for huge values; the full value is still copyable.
pub(crate) const PANEL_BYTES: usize = 64 * 1024;
// A hex dump is about four times longer than its bytes, so it gets a tighter cap.
pub(crate) const HEX_BYTES: usize = 4096;

// Deep nesting multiplies indentation, so the pretty output needs its own bound.
pub(crate) const PRETTY_BYTES: usize = 4 * PANEL_BYTES;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ValueKind {
    Null,
    Text,
    Json,
    Binary,
}

/// A cell value prepared for the panel; never Debug because it holds cell data.
pub(crate) struct Inspected {
    pub kind: ValueKind,
    pub body: SharedString,
    pub summary: SharedString,
}

pub(crate) fn inspect(value: Option<&str>) -> Inspected {
    let Some(v) = value else {
        return Inspected {
            kind: ValueKind::Null,
            body: SharedString::default(),
            summary: "NULL".into(),
        };
    };
    if let Some(n) = hex_len(v) {
        let mut summary = format!("binary · {}", human_bytes(n));
        if n > HEX_BYTES {
            summary.push_str(" · showing first 4.0 KB");
        }
        return Inspected {
            kind: ValueKind::Binary,
            body: hex_dump(v, HEX_BYTES).into(),
            summary: summary.into(),
        };
    }
    let trimmed = v.trim();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && v.len() <= PANEL_BYTES
        && serde_json::from_str::<serde_json::Value>(v).is_ok()
    {
        let pretty = pretty_json(trimmed);
        if pretty.len() <= PRETTY_BYTES {
            return Inspected {
                kind: ValueKind::Json,
                body: pretty.into(),
                summary: format!("json · {}", human_bytes(v.len())).into(),
            };
        }
    }
    let mut summary = format!("text · {}", human_bytes(v.len()));
    if v.len() > PANEL_BYTES {
        summary.push_str(" · showing first 64.0 KB");
    }
    Inspected {
        kind: ValueKind::Text,
        body: text_body(v).into(),
        summary: summary.into(),
    }
}

pub(crate) fn hex_len(value: &str) -> Option<usize> {
    let hex = value.strip_prefix("\\x")?;
    (hex.len() % 2 == 0 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex.len() / 2)
}

pub(crate) fn hex_dump(value: &str, max_bytes: usize) -> String {
    let hex = value.strip_prefix("\\x").unwrap_or("");
    let bytes: Vec<u8> = (0..hex.len() / 2)
        .take(max_bytes)
        .filter_map(|i| u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok())
        .collect();
    let mut lines = Vec::new();
    for (n, chunk) in bytes.chunks(16).enumerate() {
        let mut line = format!("{:08x} ", n * 16);
        for i in 0..16 {
            if i == 8 {
                line.push(' ');
            }
            match chunk.get(i) {
                Some(b) => line.push_str(&format!(" {b:02x}")),
                None => line.push_str("   "),
            }
        }
        line.push_str("  ");
        line.extend(chunk.iter().map(|&b| {
            if (0x20..0x7f).contains(&b) {
                b as char
            } else {
                '.'
            }
        }));
        lines.push(line);
    }
    lines.join("\n")
}

fn indent(out: &mut String, depth: usize) {
    out.push('\n');
    for _ in 0..depth {
        out.push_str("  ");
    }
}

/// Re-indents already-valid JSON without reparsing, so numbers and key order stay byte-exact.
pub(crate) fn pretty_json(src: &str) -> String {
    let mut out = String::with_capacity(src.len() + src.len() / 4);
    let (mut depth, mut in_str, mut escaped) = (0usize, false, false);
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        if in_str {
            out.push(c);
            if escaped {
                escaped = false
            } else if c == '\\' {
                escaped = true
            } else if c == '"' {
                in_str = false
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                out.push(c)
            }
            '{' | '[' => {
                out.push(c);
                while chars.next_if(|n| n.is_whitespace()).is_some() {}
                if let Some(close) = chars.next_if(|&n| n == '}' || n == ']') {
                    out.push(close)
                } else {
                    depth += 1;
                    indent(&mut out, depth)
                }
            }
            '}' | ']' => {
                depth = depth.saturating_sub(1);
                indent(&mut out, depth);
                out.push(c)
            }
            ',' => {
                out.push(',');
                indent(&mut out, depth)
            }
            ':' => out.push_str(": "),
            c if c.is_whitespace() => {}
            c => out.push(c),
        }
    }
    out
}

pub(crate) fn human_bytes(n: usize) -> String {
    if n < 1024 {
        format!("{n} B")
    } else if n < 1024 * 1024 {
        format!("{:.1} KB", n as f64 / 1024.)
    } else {
        format!("{:.1} MB", n as f64 / (1024. * 1024.))
    }
}

fn text_body(v: &str) -> String {
    let mut end = v.len().min(PANEL_BYTES);
    while !v.is_char_boundary(end) {
        end -= 1;
    }
    // gpui text does not render tabs.
    v[..end]
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes_hex(range: std::ops::Range<u8>) -> String {
        let mut s = String::from("\\x");
        for b in range {
            s.push_str(&format!("{b:02x}"));
        }
        s
    }

    #[test]
    fn pretty_json_reindents_verbatim() {
        let src = r#"{"a":1,"b":[1, 2,{}],"c":"x,{y}\"z","d":[ ]}"#;
        assert_eq!(
            pretty_json(src),
            "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2,\n    {}\n  ],\n  \"c\": \"x,{y}\\\"z\",\n  \"d\": []\n}"
        );
    }

    #[test]
    fn pretty_json_keeps_numbers() {
        assert_eq!(
            pretty_json(r#"{"n":1.10,"big":12345678901234567890123}"#),
            "{\n  \"n\": 1.10,\n  \"big\": 12345678901234567890123\n}"
        );
    }

    #[test]
    fn pretty_json_empty_array() {
        assert_eq!(pretty_json("[]"), "[]");
    }

    #[test]
    fn hex_dump_short() {
        assert_eq!(
            hex_dump("\\x48656c6c6f00ff", HEX_BYTES),
            "00000000  48 65 6c 6c 6f 00 ff                              Hello.."
        );
    }

    #[test]
    fn hex_dump_two_lines() {
        assert_eq!(
            hex_dump(&bytes_hex(0..20), HEX_BYTES),
            "00000000  00 01 02 03 04 05 06 07  08 09 0a 0b 0c 0d 0e 0f  ................\n00000010  10 11 12 13                                       ...."
        );
    }

    #[test]
    fn hex_dump_respects_max() {
        assert_eq!(
            hex_dump(&bytes_hex(0..20), 3),
            "00000000  00 01 02                                          ..."
        );
    }

    #[test]
    fn hex_len_rules() {
        assert_eq!(hex_len("\\x"), Some(0));
        assert_eq!(hex_len("\\xABcd"), Some(2));
        assert_eq!(hex_len("\\xabc"), None);
        assert_eq!(hex_len("\\xzz"), None);
    }

    #[test]
    fn human_bytes_units() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
        assert_eq!(human_bytes(65536), "64.0 KB");
        assert_eq!(human_bytes(3 * 1024 * 1024), "3.0 MB");
    }

    #[test]
    fn inspect_kinds() {
        assert_eq!(inspect(None).kind, ValueKind::Null);
        let t = inspect(Some("hello"));
        assert_eq!(t.kind, ValueKind::Text);
        assert_eq!(t.summary.as_ref(), "text · 5 B");
        assert_eq!(inspect(Some(" {\"a\":1}")).kind, ValueKind::Json);
        assert_eq!(inspect(Some("{not json")).kind, ValueKind::Text);
        let b = inspect(Some("\\x00ff"));
        assert_eq!(b.kind, ValueKind::Binary);
        assert_eq!(b.summary.as_ref(), "binary · 2 B");
        assert_eq!(inspect(Some("[1,2")).kind, ValueKind::Text);
    }

    #[test]
    fn inspect_text_normalises() {
        assert_eq!(inspect(Some("a\r\nb\rc\td")).body.as_ref(), "a\nb\nc    d");
    }

    #[test]
    fn inspect_text_cuts_long() {
        let mut v: String = std::iter::repeat_n('a', PANEL_BYTES - 1).collect();
        v.extend(std::iter::repeat_n('é', 6));
        let i = inspect(Some(&v));
        assert!(i.body.len() <= PANEL_BYTES);
        assert!(i.summary.ends_with("showing first 64.0 KB"));
    }

    #[test]
    fn inspect_json_over_cap_is_text() {
        let mut v = String::from("[");
        while v.len() <= PANEL_BYTES {
            v.push_str("1,");
        }
        v.push('1');
        v.push(']');
        assert_eq!(inspect(Some(&v)).kind, ValueKind::Text);
    }

    #[test]
    fn inspect_deeply_nested_json_stays_bounded() {
        let depth = 100;
        let mut v = String::new();
        while v.len() + depth < PANEL_BYTES / 2 {
            for _ in 0..depth {
                v.push('[');
            }
            for _ in 0..depth {
                v.push(']');
            }
            v.push(',');
        }
        let v = format!("[{}[]]", v);
        let i = inspect(Some(&v));
        assert_eq!(i.kind, ValueKind::Text);
        assert!(i.body.len() <= PRETTY_BYTES);
    }
}
