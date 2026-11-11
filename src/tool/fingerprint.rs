//! A cheap content fingerprint for "is this still the file I read?".
//! Why it exists: crate::tool::edit::EditFileTool accepts an optional `expected_hash` so a model can say "only...
//! # Why FNV-1a and not SHA-256.

use std::io::Read;
use std::path::Path;

const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// Files larger than this are not fingerprinted: the extra full read is not worth it, and a file that big...
pub(crate) const MAX_FINGERPRINT_BYTES: u64 = 4 * 1024 * 1024;

/// How the fingerprint reads in a tool result. One function so read_file's header and edit_file's
/// reply can never spell it differently.
pub(crate) fn unknown() -> &'static str {
    "-"
}

/// The fingerprint of a byte slice, as the 16 lowercase hex digits printed in tool results.
pub(crate) fn of_bytes(bytes: &[u8]) -> String {
    let mut h = OFFSET_BASIS;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(PRIME);
    }
    format!("{h:016x}")
}

/// The fingerprint of a whole file, streamed — memory stays constant regardless of file size.
pub(crate) fn of_file(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_FINGERPRINT_BYTES {
        return None;
    }
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 64 * 1024];
    let mut h = OFFSET_BASIS;
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        for b in &buf[..n] {
            h ^= *b as u64;
            h = h.wrapping_mul(PRIME);
        }
    }
    Some(format!("{h:016x}"))
}

/// Accept the several shapes a model might echo back: `abc123`, `fnv1a64:abc123`, `0xabc123`, and with stray...
pub(crate) fn normalize(s: &str) -> String {
    let t = s.trim().to_ascii_lowercase();
    let t = t.strip_prefix("fnv1a64:").unwrap_or(&t);
    let t = t.strip_prefix("fnv1a").unwrap_or(t);
    let t = t.strip_prefix("0x").unwrap_or(t);
    t.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published FNV-1a 64 test vectors.
    #[test]
    fn matches_the_published_vectors() {
        assert_eq!(of_bytes(b""), "cbf29ce484222325");
        assert_eq!(of_bytes(b"a"), "af63dc4c8601ec8c");
        assert_eq!(of_bytes(b"foobar"), "85944171f73967e8");
    }

    /// Content is what matters — the same bytes under a different name or path fingerprint alike, and a
    /// one-byte change is enough to move it.
    #[test]
    fn fingerprints_content_not_identity() {
        let a = of_bytes("héllo\n".as_bytes());
        let b = of_bytes("héllo\n".as_bytes());
        assert_eq!(a, b);
        assert_ne!(a, of_bytes("héllo!\n".as_bytes()));
    }

    #[test]
    fn normalizes_every_plausible_spelling() {
        let want = "af63dc4c8601ec8c";
        for s in [
            "af63dc4c8601ec8c",
            "  AF63DC4C8601EC8C  ",
            "fnv1a64:af63dc4c8601ec8c",
            "0xaf63dc4c8601ec8c",
        ] {
            assert_eq!(normalize(s), want, "{s:?}");
        }
    }

    #[test]
    fn oversized_files_are_not_fingerprinted() {
        let d = std::env::temp_dir().join("aox-fp-big");
        let _ = std::fs::create_dir_all(&d);
        let p = d.join("big.bin");
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(MAX_FINGERPRINT_BYTES + 1).unwrap();
        drop(f);
        assert_eq!(of_file(&p), None);
        let _ = std::fs::remove_dir_all(&d);
    }
}
