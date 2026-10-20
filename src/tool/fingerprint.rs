//! A cheap content **fingerprint** for "is this still the file I read?".
//!
//! Why it exists: [`crate::tool::edit::EditFileTool`] accepts an optional `expected_hash` so a model
//! can say "only edit the version I actually looked at". Without it, an edit computed against a stale
//! read silently lands on a file somebody else changed in between — the rewrite is still syntactically
//! valid, so nothing complains until much later.
//!
//! # Why FNV-1a and not SHA-256
//!
//! This is a **change detector, not a security primitive**: the value is printed into read_file's
//! header and echoed back by edit_file. Nobody can gain anything by forging it — a forged value only
//! makes the model's own edit land on a version it did not read. So there is no reason to pull in a
//! crypto hash crate for it; FNV-1a 64 is fifteen lines, has no dependencies, and is stable across
//! platforms and process restarts (which is the only property that matters here).
//!
//! ⚠️ Never describe this value to a user as a checksum for integrity purposes. It detects accidental
//! drift; it does not resist a deliberate collision.

use std::io::Read;
use std::path::Path;

const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const PRIME: u64 = 0x0000_0100_0000_01b3;

/// Files larger than this are **not** fingerprinted: the extra full read is not worth it, and a file
/// that big is not something `edit_file` will touch anyway (see its size cap). A missing fingerprint
/// is reported honestly (`hash -`), and the model simply omits `expected_hash`.
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

/// The fingerprint of a whole file, **streamed** — memory stays constant regardless of file size.
///
/// `None` means "could not fingerprint": the file is larger than [`MAX_FINGERPRINT_BYTES`], or it
/// could not be opened / read. That is deliberately not an error — the caller reports `-` and carries
/// on, because a fingerprint is an optional convenience, never the point of the call.
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

/// Accept the several shapes a model might echo back: `abc123`, `fnv1a64:abc123`, `0xabc123`, and
/// with stray whitespace or capitals.
///
/// Being forgiving here costs nothing and removes a whole class of "I gave you exactly the string you
/// printed" failures. It is **not** forgiving about the value itself: a real mismatch is still a
/// mismatch.
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

    /// The published FNV-1a 64 test vectors. Pinning them is what makes "stable across versions"
    /// checkable instead of a promise in a comment: if somebody swaps the algorithm, the fingerprints
    /// embedded in a running conversation stop matching and every `expected_hash` edit starts failing
    /// with "the file changed".
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
