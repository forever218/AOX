//! Charset: what the bytes say they are, and then decoding them.
//!
//! # Order, and why
//!
//! 1. an explicit `charset` argument (the caller saw mojibake and is correcting us);
//! 2. a BOM;
//! 3. `Content-Type: …; charset=…`;
//! 4. `<meta charset=…>` / `<meta http-equiv=…>` in the first 4 KiB;
//! 5. **strict** UTF-8 validation — the modern default, and a real answer rather than a guess;
//! 6. `[web] charset_fallback` (GB18030 by default), then UTF-8-with-replacement.
//!
//! Step 5 before 6 matters: "is this valid UTF-8?" is a question `MultiByteToWideChar` cannot answer,
//! which is why this module uses `encoding_rs` instead of the zero-cost Win32 path (docs/WEB.md D2).
//!
//! # GB2312 / GBK are upgraded to GB18030
//!
//! They are nested subsets, and a page labelled `gb2312` that actually contains a 4-byte GB18030
//! character is common enough in the wild. Decoding with the superset can only turn a replacement
//! character into the right glyph, never the other way around.

use encoding_rs::{Encoding, GB18030, UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252};

use super::fetch::WebError;

/// The encoding that won, and which step chose it — reported in the result header so a mis-decode is
/// visible instead of mysterious.
#[derive(Debug, Clone, Copy)]
pub struct Detected {
    pub encoding: &'static Encoding,
    /// `override` | `bom` | `header` | `meta` | `sniffed` | `fallback` | `none`
    pub source: &'static str,
}

/// Decide how to decode `head` (the first bytes of the body — enough for a meta scan).
///
/// `content_type` is the raw header value, if any. `fallback` is `[web] charset_fallback`.
pub fn detect(
    head: &[u8],
    content_type: Option<&str>,
    override_label: Option<&str>,
    fallback: &str,
) -> Result<Detected, WebError> {
    if let Some(raw) = override_label.map(str::trim).filter(|s| !s.is_empty()) {
        let Some((encoding, _)) = resolve(raw) else {
            return Err(WebError::new(crate::tf!("unknown charset \"{}\"", raw))
                .hint("use an IANA label such as utf-8, gb18030, big5, shift_jis, windows-1252"));
        };
        return Ok(Detected { encoding, source: "override" });
    }

    if let Some((encoding, source)) = bom(head) {
        return Ok(Detected { encoding, source });
    }

    if let Some(label) = content_type.and_then(charset_param) {
        if let Some((encoding, _)) = resolve(&label) {
            return Ok(Detected { encoding, source: "header" });
        }
    }

    if let Some(label) = meta_charset(head) {
        if let Some((encoding, _)) = resolve(&label) {
            return Ok(Detected { encoding, source: "meta" });
        }
    }

    if std::str::from_utf8(head).is_ok() {
        // A truncated body can end mid-character, so "the head is valid UTF-8" is necessary but not
        // sufficient; `decode` below would report replacement characters if the rest disagrees. The
        // label is still the best available answer, and the header line says where it came from.
        return Ok(Detected { encoding: UTF_8, source: "sniffed" });
    }

    let fb = fallback.trim().to_ascii_lowercase();
    let (encoding, source) = match fb.as_str() {
        "none" | "off" => (UTF_8, "none"),
        "windows-1252" | "latin1" | "iso-8859-1" => (WINDOWS_1252, "fallback"),
        _ => (GB18030, "fallback"),
    };
    Ok(Detected { encoding, source })
}

/// Decode, returning the text and how many replacement characters it contains.
///
/// The count is what lets the tool say "this may be mis-decoded" without pretending to be sure.
pub fn decode(bytes: &[u8], det: &Detected) -> (String, usize) {
    let (text, _, _) = det.encoding.decode(bytes);
    let bad = text.matches('\u{FFFD}').count();
    (text.into_owned(), bad)
}

fn resolve(label: &str) -> Option<(&'static Encoding, bool)> {
    let l = label
        .trim()
        .trim_matches(|c| c == '"' || c == '\'')
        .trim()
        .to_ascii_lowercase();
    if l.is_empty() {
        return None;
    }
    if matches!(l.as_str(), "gb2312" | "gbk" | "x-gbk" | "gb18030" | "gb_2312-80") {
        return Some((GB18030, true));
    }
    Encoding::for_label(l.as_bytes()).map(|e| (e, false))
}

fn bom(head: &[u8]) -> Option<(&'static Encoding, &'static str)> {
    if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Some((UTF_8, "bom"))
    } else if head.starts_with(&[0xFF, 0xFE]) {
        Some((UTF_16LE, "bom"))
    } else if head.starts_with(&[0xFE, 0xFF]) {
        Some((UTF_16BE, "bom"))
    } else {
        None
    }
}

/// `text/html; charset=gbk` → `gbk`. Case-insensitive, tolerates quotes and extra parameters.
pub fn charset_param(value: &str) -> Option<String> {
    let low = value.to_ascii_lowercase();
    let at = low.find("charset")?;
    let rest = &value[at + "charset".len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?.trim_start();
    let bytes = rest.as_bytes();
    let quote = bytes.first().copied().filter(|b| *b == b'"' || *b == b'\'');
    let rest = match quote {
        Some(_) => &rest[1..],
        None => rest,
    };
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':')))
        .unwrap_or(rest.len());
    let label = rest[..end].trim();
    (!label.is_empty()).then(|| label.to_string())
}

/// First `charset=…` inside the first 4 KiB, whether written as `<meta charset>` or inside
/// `<meta http-equiv="content-type">`.
fn meta_charset(head: &[u8]) -> Option<String> {
    let window = &head[..head.len().min(4096)];
    let text = String::from_utf8_lossy(window).to_ascii_lowercase();
    let mut from = 0usize;
    while let Some(at) = text[from..].find("charset") {
        let abs = from + at;
        if let Some(label) = charset_param(&text[abs..]) {
            return Some(label);
        }
        from = abs + "charset".len();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gbk(s: &str) -> Vec<u8> {
        let (bytes, _, _) = GB18030.encode(s);
        bytes.into_owned()
    }

    #[test]
    fn an_explicit_override_wins_over_everything() {
        let det = detect(b"\xEF\xBB\xBFhello", Some("text/html; charset=gbk"), Some("utf-8"), "gb18030")
            .unwrap();
        assert_eq!(det.source, "override");
        assert_eq!(det.encoding, UTF_8);
    }

    #[test]
    fn an_unknown_override_is_an_error_with_a_hint() {
        let err = detect(b"x", None, Some("klingon"), "gb18030").unwrap_err();
        assert!(err.message.contains("klingon"), "{}", err.message);
        assert!(err.hint.unwrap().contains("gb18030"));
    }

    #[test]
    fn a_bom_beats_the_header() {
        let det = detect(b"\xEF\xBB\xBFhi", Some("text/html; charset=windows-1252"), None, "gb18030")
            .unwrap();
        assert_eq!(det.source, "bom");
        assert_eq!(det.encoding, UTF_8);
    }

    #[test]
    fn gb2312_in_the_header_is_upgraded_to_gb18030() {
        let det = detect(b"<html>", Some("text/html; charset=gb2312"), None, "gb18030").unwrap();
        assert_eq!(det.source, "header");
        assert_eq!(det.encoding, GB18030);
    }

    #[test]
    fn a_meta_tag_is_read_when_the_header_is_silent() {
        let head = br#"<html><head><meta http-equiv="Content-Type" content="text/html; charset=gbk">"#;
        let det = detect(head, Some("text/html"), None, "gb18030").unwrap();
        assert_eq!(det.source, "meta");
        assert_eq!(det.encoding, GB18030);
    }

    #[test]
    fn modern_pages_are_sniffed_as_utf8() {
        let det = detect("中文".as_bytes(), Some("text/html"), None, "gb18030").unwrap();
        assert_eq!(det.source, "sniffed");
        assert_eq!(det.encoding, UTF_8);
    }

    #[test]
    fn undeclared_gbk_falls_back_and_decodes_correctly() {
        let bytes = gbk("中文测试");
        let det = detect(&bytes, Some("text/html"), None, "gb18030").unwrap();
        assert_eq!(det.source, "fallback");
        let (text, bad) = decode(&bytes, &det);
        assert_eq!(text, "中文测试");
        assert_eq!(bad, 0);
    }

    #[test]
    fn fallback_none_decodes_utf8_with_replacement() {
        let det = detect(&gbk("中文"), None, None, "none").unwrap();
        assert_eq!(det.source, "none");
        let (_, bad) = decode(&gbk("中文"), &det);
        assert!(bad > 0, "undecodable bytes must be counted, not hidden");
    }

    #[test]
    fn charset_parameter_parsing_handles_the_usual_shapes() {
        assert_eq!(charset_param("text/html; charset=utf-8").as_deref(), Some("utf-8"));
        assert_eq!(charset_param("text/html;Charset=\"GBK\"; x=1").as_deref(), Some("GBK"));
        assert_eq!(charset_param("text/html").as_deref(), None);
        assert_eq!(charset_param("text/html; charset=").as_deref(), None);
    }
}
