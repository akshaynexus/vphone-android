//! Plist serialisers that reproduce Python's `plistlib` output byte for byte,
//! since the tools this crate replaces write `config.plist` and manifests with it.

use plist::Value;

const HEADER: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n";

fn escape(text: &str, out: &mut String) {
    // plistlib: `&`, `<`, `>` and line endings normalised to \n.
    for c in text.replace("\r\n", "\n").replace('\r', "\n").chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            other => out.push(other),
        }
    }
}

fn indent(level: usize, out: &mut String) {
    for _ in 0..level {
        out.push('\t');
    }
}

fn base64_lines(data: &[u8], level: usize, out: &mut String) {
    use base64::Engine;
    // plistlib's _encode_base64: chunks of (line width / 4 * 3) raw bytes, each
    // encoded on its own line, where line width is max(16, 76 - indent columns).
    let columns = 8 * level;
    let width = 16.max(76usize.saturating_sub(columns));
    let chunk = width / 4 * 3;
    let mut pieces: Vec<&[u8]> = data.chunks(chunk).collect();
    if data.is_empty() {
        pieces.clear();
    }
    for piece in pieces {
        indent(level, out);
        out.push_str(&base64::engine::general_purpose::STANDARD.encode(piece));
        out.push('\n');
    }
}

fn value(v: &Value, level: usize, out: &mut String) {
    match v {
        Value::Dictionary(d) => {
            if d.is_empty() {
                indent(level, out);
                out.push_str("<dict/>\n");
                return;
            }
            indent(level, out);
            out.push_str("<dict>\n");
            let mut keys: Vec<&String> = d.keys().collect();
            keys.sort();
            for key in keys {
                indent(level + 1, out);
                out.push_str("<key>");
                escape(key, out);
                out.push_str("</key>\n");
                value(&d[key.as_str()], level + 1, out);
            }
            indent(level, out);
            out.push_str("</dict>\n");
        }
        Value::Array(a) => {
            if a.is_empty() {
                indent(level, out);
                out.push_str("<array/>\n");
                return;
            }
            indent(level, out);
            out.push_str("<array>\n");
            for item in a {
                value(item, level + 1, out);
            }
            indent(level, out);
            out.push_str("</array>\n");
        }
        Value::Boolean(b) => {
            indent(level, out);
            out.push_str(if *b { "<true/>\n" } else { "<false/>\n" });
        }
        Value::Integer(i) => {
            indent(level, out);
            out.push_str("<integer>");
            match i.as_signed() {
                Some(n) => out.push_str(&n.to_string()),
                None => out.push_str(&i.as_unsigned().unwrap_or_default().to_string()),
            }
            out.push_str("</integer>\n");
        }
        Value::Real(r) => {
            indent(level, out);
            out.push_str("<real>");
            out.push_str(&python_real(*r));
            out.push_str("</real>\n");
        }
        Value::String(s) => {
            indent(level, out);
            out.push_str("<string>");
            escape(s, out);
            out.push_str("</string>\n");
        }
        Value::Data(data) => {
            indent(level, out);
            out.push_str("<data>\n");
            base64_lines(data, level, out);
            indent(level, out);
            out.push_str("</data>\n");
        }
        Value::Date(d) => {
            indent(level, out);
            out.push_str("<date>");
            out.push_str(&d.to_xml_format());
            out.push_str("</date>\n");
        }
        Value::Uid(_) => {
            // plistlib raises on a UID in XML; nothing this crate writes has one.
            indent(level, out);
            out.push_str("<dict/>\n");
        }
        other => {
            // plist::Value is non-exhaustive; nothing this crate writes reaches here.
            indent(level, out);
            out.push_str(&format!("<!-- unsupported {other:?} -->\n"));
        }
    }
}

/// `repr(float)` as plistlib writes it.
fn python_real(r: f64) -> String {
    if r.is_finite() && r == r.trunc() && r.abs() < 1e16 {
        format!("{r:.1}")
    } else {
        format!("{r}")
    }
}

/// `plistlib.dumps(value, fmt=FMT_XML)` (sort_keys=True).
pub fn to_xml(v: &Value) -> Vec<u8> {
    let mut out = String::from(HEADER);
    value(v, 0, &mut out);
    out.push_str("</plist>\n");
    out.into_bytes()
}

/// `plistlib.dumps({"ECID": ecid}, fmt=FMT_BINARY)`: Virtualization.framework's
/// machine identifier.
pub fn machine_identifier(ecid: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(64);
    out.extend_from_slice(b"bplist00");
    // Objects: 0 = dict, 1 = key "ECID", 2 = the integer.
    let mut offsets = Vec::with_capacity(3);
    offsets.push(out.len());
    out.extend_from_slice(&[0xD1, 1, 2]);
    offsets.push(out.len());
    out.push(0x54);
    out.extend_from_slice(b"ECID");
    offsets.push(out.len());
    if ecid < 1 << 8 {
        out.extend_from_slice(&[0x10, ecid as u8]);
    } else if ecid < 1 << 16 {
        out.push(0x11);
        out.extend_from_slice(&(ecid as u16).to_be_bytes());
    } else if ecid < 1 << 32 {
        out.push(0x12);
        out.extend_from_slice(&(ecid as u32).to_be_bytes());
    } else if ecid < 1 << 63 {
        out.push(0x13);
        out.extend_from_slice(&ecid.to_be_bytes());
    } else {
        out.push(0x14);
        out.extend_from_slice(&0u64.to_be_bytes());
        out.extend_from_slice(&ecid.to_be_bytes());
    }
    let table = out.len() as u64;
    let offset_size: u8 = if table < 1 << 8 {
        1
    } else if table < 1 << 16 {
        2
    } else {
        4
    };
    for o in &offsets {
        out.extend_from_slice(&(*o as u64).to_be_bytes()[8 - offset_size as usize..]);
    }
    out.extend_from_slice(&[0; 6]);
    out.push(offset_size);
    out.push(1); // object ref size
    out.extend_from_slice(&3u64.to_be_bytes()); // object count
    out.extend_from_slice(&0u64.to_be_bytes()); // top object
    out.extend_from_slice(&table.to_be_bytes());
    out
}
