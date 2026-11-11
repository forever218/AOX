//! Attachments — hang a local file on a message and send it to the model.
//! # Why one module.
//! All three entry points must do the same thing, so the checks, sniffing and limits are written once.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// Byte cap for a single image (an endpoint hard limit; inline base64 and an external URL count
/// the same).
pub const MAX_IMAGE_BYTES: u64 = 32 * 1024 * 1024;
/// Byte cap for one request body (**inline base64 included**). base64 inflates by about 4/3, so
/// estimate at 4/3 when checking.
pub const MAX_BODY_BYTES: u64 = 48 * 1024 * 1024;
/// Byte cap for any single attachment: a 48 MiB request body converts back to 36 MiB of raw bytes at 4/3.
pub const MAX_FILE_BYTES: u64 = MAX_BODY_BYTES * 3 / 4;
/// Cap on the number of images in one request.
pub const MAX_IMAGES: usize = 600;

/// The four image formats the endpoint lists in its "image understanding" docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Jpeg,
    Png,
    Gif,
    Webp,
}

impl ImageKind {
    pub fn mime(self) -> &'static str {
        match self {
            ImageKind::Jpeg => "image/jpeg",
            ImageKind::Png => "image/png",
            ImageKind::Gif => "image/gif",
            ImageKind::Webp => "image/webp",
        }
    }

    /// Judge by file content (magic number).
    pub fn sniff(head: &[u8]) -> Option<Self> {
        if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
            return Some(ImageKind::Jpeg);
        }
        if head.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some(ImageKind::Png);
        }
        if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
            return Some(ImageKind::Gif);
        }
        // WebP: `RIFF` + 4 bytes of length + `WEBP`. Those 4 length bytes aren't validated (they
        // don't take part in format detection).
        if head.len() >= 12 && head.starts_with(b"RIFF") && &head[8..12] == b"WEBP" {
            return Some(ImageKind::Webp);
        }
        None
    }
}

/// An attachment waiting to be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    /// Local path. Three uses: reading from disk, writing a reference into the `.jsonl`, and telling the model...
    pub path: PathBuf,
    /// Display name (used by the pill in the UI and by the `filename` field).
    pub name: String,
    /// The file's byte count (obtained by stat during `probe`).
    pub size: u64,
    /// The image type sniffed from the magic number; `None` means it isn't one of those four
    /// images.
    pub kind: Option<ImageKind>,
}

/// How many header bytes to read when sniffing. 12 is enough (WebP's signature is the longest, at byte 12)...
const HEAD_LEN: usize = 32;

impl Attachment {
    /// Build an attachment from a path: stat + read a HEAD_LEN-byte header, without pulling the whole file into memory.
    pub fn probe(path: impl Into<PathBuf>) -> Result<Self, String> {
        let path = path.into();
        let md = std::fs::metadata(&path)
            .map_err(|e| crate::tf!("cannot read {}: {}", path.display(), e))?;
        if md.is_dir() {
            return Err(crate::tf!(
                "{} is a directory. Attachments take single files only — zip them into one file first.",
                path.display()
            ));
        }
        if !md.is_file() {
            return Err(crate::tf!("{} is not a regular file, so it cannot be an attachment.", path.display()));
        }
        let size = md.len();
        let kind = sniff_path(&path)?;
        if kind.is_some() && size > MAX_IMAGE_BYTES {
            return Err(crate::tf!(
                "{} is an image but is {} bytes, over the single-image limit of {} MiB (a hard endpoint limit). \
                 Please compress or downscale it before attaching.",
                path.display(),
                size,
                MAX_IMAGE_BYTES / 1024 / 1024
            ));
        }
        if size > MAX_FILE_BYTES {
            return Err(crate::tf!(
                "{} is {} MiB, over the single-attachment limit of {} MiB — once base64-encoded it will not fit in one request body ({} MiB). Please compress or split it.",
                path.display(),
                size / 1024 / 1024,
                MAX_FILE_BYTES / 1024 / 1024,
                MAX_BODY_BYTES / 1024 / 1024
            ));
        }
        Ok(Self {
            name: display_name(&path),
            path,
            size,
            kind,
        })
    }

    /// Read the whole file into memory. Called only when assembling a request.
    pub fn read(&self) -> Result<Vec<u8>, String> {
        std::fs::read(&self.path).map_err(|e| crate::tf!("failed to read {}: {}", self.path.display(), e))
    }
}

/// Display name: the file name (without the directory). The UI pill shows it, and truncation for
/// length is the UI's own business.
fn display_name(path: &Path) -> String {
    path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| {
        // Extreme case (path ends in `..`, etc.) — fall back to the full path, better than blank.
        path.to_string_lossy().into_owned()
    })
}

/// Read the file header to determine the image type.
fn sniff_path(path: &Path) -> Result<Option<ImageKind>, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| crate::tf!("cannot open {}: {}", path.display(), e))?;
    let mut head = [0u8; HEAD_LEN];
    let n = f.read(&mut head).map_err(|e| crate::tf!("failed to read the header of {}: {}", path.display(), e))?;
    Ok(ImageKind::sniff(&head[..n]))
}

/// The batch gate before sending: image count and total request body.
pub fn check_batch(items: &[Attachment], extra_bytes: usize) -> Result<(), String> {
    let images = items.iter().filter(|a| a.kind.is_some()).count();
    if images > MAX_IMAGES {
        return Err(crate::tf!(
            "at most {} images can be sent at once; this time there are {}. Please send them in batches.",
            MAX_IMAGES,
            images
        ));
    }
    // base64 is 4 bytes for every 3 ⇒ estimate as ceil(n/3)*4, then add the body text on top.
    let raw: u64 = items.iter().map(|a| a.size).sum();
    let inlined = raw / 3 * 4 + (raw % 3 != 0) as u64 * 4;
    let total = inlined + extra_bytes as u64;
    if total > MAX_BODY_BYTES {
        return Err(crate::tf!(
            "this batch of attachments is too large: about {} MiB once encoded, over the single-request body limit of {} MiB (a hard endpoint limit). Send them in batches, or compress first.",
            total / 1024 / 1024,
            MAX_BODY_BYTES / 1024 / 1024
        ));
    }
    Ok(())
}

/// The start marker of the attachment listing block.
pub const ATTACH_MARK: &str = "[attachments]\n";

/// Assemble "text + attachments" into one message's `content`.
pub fn to_content(text: &str, items: &[Attachment]) -> Result<Value, String> {
    if items.is_empty() {
        return Ok(Value::String(text.to_string()));
    }

    let mut parts: Vec<Value> = Vec::with_capacity(items.len() + 2);
    // When the body is empty, don't send an empty text block (a content-less block isn't worth betting the endpoint accepts).
    if !text.is_empty() {
        parts.push(json!({"type": "text", "text": text}));
    }
    let mut listing = String::from(ATTACH_MARK);
    for a in items {
        listing.push_str(&format!("- {} → {}\n", a.name, a.path.display()));
    }
    listing.push_str(
        crate::i18n::t("(the files above were sent along with this message; if the endpoint reports a format as unsupported, read the paths above directly with read_file.)"),
    );
    parts.push(json!({"type": "text", "text": listing}));

    for a in items {
        let bytes = a.read()?;
        let data = format!("data:{};base64,{}", mime_of(a, &bytes), b64_encode(&bytes));
        match a.kind {
            Some(_) => parts.push(json!({"type": "image_url", "image_url": {"url": data}})),
            None => parts.push(json!({"type": "file", "file_data": data, "filename": a.name})),
        }
    }
    Ok(Value::Array(parts))
}

/// Strip the attachment payload (base64) before persisting, keeping only the record that "there used...
pub fn strip_payloads(content: &Value) -> Option<Value> {
    let parts = content.as_array()?;
    let mut changed = false;
    let mut kept: Vec<Value> = Vec::with_capacity(parts.len());
    for p in parts {
        match p.get("type").and_then(|t| t.as_str()) {
            Some("image_url") | Some("file") => changed = true,
            _ => kept.push(p.clone()),
        }
    }
    if !changed {
        return None;
    }
    // When every payload has been stripped (in theory impossible: the body and the listing are both text blocks)...
    if kept.is_empty() {
        return Some(Value::String(String::new()));
    }
    Some(Value::Array(kept))
}

/// Attachment::probe one by one, deduplicating along the way, then passing the whole batch through the gate.
pub fn probe_all(paths: &[PathBuf]) -> (Vec<Attachment>, Vec<String>) {
    let mut ok: Vec<Attachment> = Vec::with_capacity(paths.len());
    let mut bad: Vec<String> = Vec::new();
    for p in paths {
        match Attachment::probe(p.clone()) {
            // The same file declared twice = one attachment (a plugin reporting the same image
            // twice is a common typo)
            Ok(a) if ok.iter().any(|x| x.path == a.path) => {}
            Ok(a) => ok.push(a),
            Err(e) => bad.push(e),
        }
    }
    if let Err(e) = check_batch(&ok, 0) {
        bad.push(e);
        // When the whole batch is over the limit, drop it all: keeping the first few would turn
        // "how many actually went out" into something you have to count
        ok.clear();
    }
    (ok, bad)
}

/// It has exactly one use: drawing the pills back in the pending-upload bar / bubble when reopening an old...
pub fn names_from_content(v: &Value) -> Vec<String> {
    files_from_content(v).into_iter().map(|(n, _)| n).collect()
}

/// Recover the attachment listing's names and paths from `content`.
pub fn files_from_content(v: &Value) -> Vec<(String, PathBuf)> {
    let Some(parts) = v.as_array() else {
        return Vec::new();
    };
    for p in parts {
        if p.get("type").and_then(|t| t.as_str()) != Some("text") {
            continue;
        }
        let Some(t) = p.get("text").and_then(|t| t.as_str()) else {
            continue;
        };
        let Some(rest) = t.strip_prefix(ATTACH_MARK) else {
            continue;
        };
        return rest
            .lines()
            .filter_map(|line| {
                let line = line.strip_prefix("- ")?;
                // "name → path": the chance of ` → ` appearing inside a name is negligible, so
                // splitting on the first separator is the safest
                let (name, path) = line.split_once(" → ")?;
                Some((name.trim().to_string(), PathBuf::from(path.trim())))
            })
            .collect();
    }
    Vec::new()
}

/// Fill the MIME for the `data:` URL.
fn mime_of(a: &Attachment, bytes: &[u8]) -> &'static str {
    if let Some(k) = a.kind {
        return k.mime();
    }
    if bytes.starts_with(b"%PDF") {
        return "application/pdf";
    }
    // No NUL in the first 8 KiB and it decodes as UTF-8 ⇒ treat it as text.
    let head = &bytes[..bytes.len().min(8192)];
    if !head.contains(&0) && std::str::from_utf8(head).is_ok() {
        return "text/plain";
    }
    "application/octet-stream"
}

/// Standard base64 (with `=` padding).
pub fn b64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        // When there are fewer than 3 bytes, the one or two missing 6-bit groups are padded with `=`
        if chunk.len() > 1 {
            out.push(T[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(T[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        // The standard RFC 4648 vectors, covering all three lengths: exact 3 bytes / 1 left over /
        // 2 left over.
        assert_eq!(b64_encode(b""), "");
        assert_eq!(b64_encode(b"f"), "Zg==");
        assert_eq!(b64_encode(b"fo"), "Zm8=");
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert_eq!(b64_encode(b"foob"), "Zm9vYg==");
        assert_eq!(b64_encode(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_high_bytes() {
        // High bytes are where a hand-written encoder most often crashes (sign extension).
        assert_eq!(b64_encode(&[0xFF]), "/w==");
        assert_eq!(b64_encode(&[0xFF, 0xFF]), "//8=");
        assert_eq!(b64_encode(&[0xFF, 0xFF, 0xFF]), "////");
        assert_eq!(b64_encode(&[0x00, 0x00, 0x00]), "AAAA");
    }

    #[test]
    fn base64_length_is_always_a_multiple_of_four() {
        // Get the length formula off by one byte and the endpoint receives a broken image.
        for n in 0..64usize {
            let data = vec![0x5Au8; n];
            assert_eq!(b64_encode(&data).len() % 4, 0, "n={n}");
            assert_eq!(b64_encode(&data).len(), (n + 2) / 3 * 4, "n={n}");
        }
    }

    #[test]
    fn sniff_recognises_the_four_supported_formats() {
        assert_eq!(ImageKind::sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(ImageKind::Jpeg));
        assert_eq!(ImageKind::sniff(b"\x89PNG\r\n\x1a\n\x00\x00"), Some(ImageKind::Png));
        assert_eq!(ImageKind::sniff(b"GIF89a.........."), Some(ImageKind::Gif));
        assert_eq!(ImageKind::sniff(b"GIF87a.........."), Some(ImageKind::Gif));
        assert_eq!(ImageKind::sniff(b"RIFF\x00\x00\x00\x00WEBPVP8 "), Some(ImageKind::Webp));
    }

    #[test]
    fn sniff_rejects_near_misses() {
        // `RIFF` but not `WEBP` (e.g. .wav) must not be taken for an image
        assert_eq!(ImageKind::sniff(b"RIFF\x00\x00\x00\x00WAVEfmt "), None);
        // Only a 4-byte `RIFF` header (the WEBP four bytes haven't been read yet)
        assert_eq!(ImageKind::sniff(b"RIFF"), None);
        assert_eq!(ImageKind::sniff(b"hello world"), None);
        assert_eq!(ImageKind::sniff(b""), None);
    }

    #[test]
    fn content_is_a_plain_string_when_there_are_no_attachments() {
        // This is the **zero-drift** guarantee: with no attachments, what's serialised must still
        // be a JSON string.
        let v = to_content("hello", &[]).unwrap();
        assert_eq!(v, Value::String("hello".into()));
    }

    #[test]
    fn content_becomes_parts_when_attachments_exist() {
        let d = std::env::temp_dir().join("aox-attach-parts");
        let _ = std::fs::create_dir_all(&d);
        // The smallest legal PNG header (the content needn't be a real image: this module doesn't
        // validate pixels)
        let png = d.join("a.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").unwrap();

        let a = Attachment::probe(&png).unwrap();
        assert_eq!(a.kind, Some(ImageKind::Png));
        assert_eq!(a.name, "a.png");

        let v = to_content("look at this image", &[a]).unwrap();
        let parts = v.as_array().unwrap();
        assert_eq!(parts.len(), 3, "body + listing + image block");
        assert_eq!(parts[0]["type"], "text");
        assert_eq!(parts[0]["text"], "look at this image", "the user body must be untouched, not polluted by the listing");
        // The listing occupies a block of its own, and must carry the path — the model needs it to
        // read_file when the endpoint refuses
        assert!(parts[1]["text"].as_str().unwrap().starts_with(ATTACH_MARK));
        assert!(parts[1]["text"].as_str().unwrap().contains("a.png"));
        assert_eq!(parts[2]["type"], "image_url");
        assert!(parts[2]["image_url"]["url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/png;base64,"));

        // The listing can be read back (the only source for drawing the pills when reopening an
        // old conversation)
        assert_eq!(names_from_content(&v), vec!["a.png".to_string()]);

        let _ = std::fs::remove_dir_all(&d);
    }

    /// The attachment listing is something the host added, so it must **not** show up in the user
    /// bubble — otherwise the UI would sprout a "- a.png → E:\…" that looks like the user typed it.
    #[test]
    fn flat_text_hides_the_host_listing_but_recovers_the_names() {
        let d = std::env::temp_dir().join("aox-attach-flat");
        let _ = std::fs::create_dir_all(&d);
        let f = d.join("report.pdf");
        std::fs::write(&f, b"%PDF-1.4 fake").unwrap();

        let a = Attachment::probe(&f).unwrap();
        let m = crate::message::Message::user_with_files("take a look for me", &[a]).unwrap();
        let flat = m.flat_text();
        assert!(flat.contains("take a look for me"), "{flat}");
        assert!(!flat.contains("→"), "the path listing must not appear in the readable text: {flat}");

        assert_eq!(names_from_content(m.content.as_ref().unwrap()), vec!["report.pdf".to_string()]);

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn non_image_goes_out_as_a_file_part_and_is_not_refused_locally() {
        let d = std::env::temp_dir().join("aox-attach-filepart");
        let _ = std::fs::create_dir_all(&d);
        // A file with a .xyz suffix and unrecognisable content — the host **must not** block it
        // (the user wants it left to the endpoint)
        let odd = d.join("note.xyz");
        std::fs::write(&odd, b"just some text").unwrap();

        let a = Attachment::probe(&odd).expect("a non-image must not be rejected at the probe stage");
        assert_eq!(a.kind, None);
        let v = to_content("read this", &[a]).unwrap();
        let parts = v.as_array().unwrap();
        assert_eq!(parts[2]["type"], "file", "0=body 1=listing 2=file block");
        assert_eq!(parts[2]["filename"], "note.xyz");
        assert!(parts[2]["file_data"].as_str().unwrap().starts_with("data:text/plain;base64,"));

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn oversized_image_is_refused_before_it_leaves_the_host() {
        let d = std::env::temp_dir().join("aox-attach-bigimg");
        let _ = std::fs::create_dir_all(&d);
        let p = d.join("big.png");
        // Build "PNG header + 32 MiB + 1 byte" — probe only looks at the header and stat, no need
        // to really write an image
        let mut f = std::fs::File::create(&p).unwrap();
        use std::io::Write;
        f.write_all(b"\x89PNG\r\n\x1a\n").unwrap();
        f.set_len(MAX_IMAGE_BYTES + 1).unwrap();
        drop(f);

        let err = Attachment::probe(&p).unwrap_err();
        assert!(err.contains("32 MiB"), "the error must name which gate and what value: {err}");
        assert!(err.contains("compress"), "it must also give the next step: {err}");

        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn batch_gate_counts_base64_inflation() {
        // Sitting right under 48 MiB: 36 MiB raw ⇒ 48 MiB encoded, and no more body can be added
        let mk = |size: u64| Attachment {
            path: PathBuf::from("x"),
            name: "x".into(),
            size,
            kind: None,
        };
        let ok = 36 * 1024 * 1024u64;
        assert!(check_batch(&[mk(ok)], 0).is_ok(), "36 MiB encodes to exactly 48 MiB, so it should be allowed");
        assert!(check_batch(&[mk(ok)], 4096).is_err(), "adding a little more body overflows it, so it must be blocked");
        assert!(check_batch(&[mk(1)], 0).is_ok());
    }

    #[test]
    fn batch_gate_counts_images() {
        let mk = |kind: Option<ImageKind>| Attachment {
            path: PathBuf::from("x"),
            name: "x".into(),
            size: 1,
            kind,
        };
        let many: Vec<_> = (0..MAX_IMAGES + 1).map(|_| mk(Some(ImageKind::Png))).collect();
        assert!(check_batch(&many, 0).is_err());
        // Non-images don't hit this gate (the endpoint judges them one by one)
        let files: Vec<_> = (0..MAX_IMAGES + 1).map(|_| mk(None)).collect();
        assert!(check_batch(&files, 0).is_ok());
    }

    #[test]
    fn directory_is_not_an_attachment() {
        let d = std::env::temp_dir().join("aox-attach-dir");
        let _ = std::fs::create_dir_all(&d);
        let err = Attachment::probe(&d).unwrap_err();
        assert!(err.contains("directory"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `probe_all`'s taste: attach what can be attached, explain why what can't, never drop the whole batch.
    #[test]
    fn probe_all_keeps_the_good_ones_and_explains_the_bad_ones() {
        let d = std::env::temp_dir().join("aox-attach-all");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let good = d.join("good.png");
        std::fs::write(&good, b"\x89PNG\r\n\x1a\n\x00\x00").unwrap();
        let subdir = d.join("sub");
        std::fs::create_dir_all(&subdir).unwrap();
        let missing = d.join("nope.bin");

        let (ok, bad) = probe_all(&[good.clone(), subdir, missing]);
        assert_eq!(ok.len(), 1, "the one that passes must be kept");
        assert_eq!(ok[0].name, "good.png");
        assert_eq!(bad.len(), 2, "the other two need one sentence each: {bad:?}");

        // The same path given twice = one attachment (a plugin reporting the same image twice is a
        // common typo)
        let (ok, _) = probe_all(&[good.clone(), good]);
        assert_eq!(ok.len(), 1);

        let _ = std::fs::remove_dir_all(&d);
    }

    /// Strip the base64 on persist, **but the listing must stay** — otherwise, on reopening an old
    /// conversation, the UI can no longer show what that message carried at the time.
    #[test]
    fn strip_payloads_drops_the_bytes_but_keeps_the_listing() {
        let d = std::env::temp_dir().join("aox-attach-strip");
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let png = d.join("big.png");
        std::fs::write(&png, b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR").unwrap();

        let a = Attachment::probe(&png).unwrap();
        let v = to_content("Look at this", &[a]).unwrap();
        assert!(v.to_string().contains("base64"), "precondition: the encoded form really does carry the payload");

        let slim = strip_payloads(&v).expect("a message with attachments must be rewritten");
        let s = slim.to_string();
        assert!(!s.contains("base64"), "the payload must be removed: {s}");
        assert!(!s.contains("image_url"), "the image block is dropped whole, leaving no fake url: {s}");
        assert!(s.contains("big.png"), "the listing must stay, otherwise history cannot show what was attached: {s}");
        assert!(s.contains("Look at this"), "the body of course must stay: {s}");
        // What remains is a valid shape (it can be parsed back into a message, with no images)
        let m: crate::message::Message = serde_json::from_value(json!({
            "role": "user", "content": slim
        }))
        .unwrap();
        assert!(m.flat_text().contains("Look at this"), "{}", m.flat_text());
        assert_eq!(names_from_content(m.content.as_ref().unwrap()), vec!["big.png".to_string()]);

        // A plain-text message is left alone (`None` = nothing changed), and the call site uses
        // that to skip the rewrite
        assert!(strip_payloads(&Value::String("hello".into())).is_none());
        assert!(strip_payloads(&json!([{"type": "text", "text": "text only"}])).is_none());

        let _ = std::fs::remove_dir_all(&d);
    }
}
