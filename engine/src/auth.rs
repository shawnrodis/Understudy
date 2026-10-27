//! Bearer-token auth for the control API.
//!
//! On first run the engine generates a random token and stores it in the
//! data directory (`auth.token`). The same token is reused on later runs so
//! a paired controller (Muse, or the Flutter controller view) keeps working
//! without re-pairing. The token is never printed in full in logs and must
//! never be committed to a repo — it lives only on the controlled PC.
//!
//! Randomness comes from the OS where possible: `CryptGenRandom` on
//! Windows, `/dev/urandom` elsewhere, with a time/pid-seeded fallback that
//! is clearly marked. The listener is Tailscale-only (see `main.rs`), so
//! the token is a second layer, not the only layer.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

const TOKEN_BYTES: usize = 32;

/// Fill `buf` with OS randomness. Returns true on success.
fn os_random(buf: &mut [u8]) -> bool {
    #[cfg(windows)]
    {
        crate::win32::os_random(buf)
    }
    #[cfg(not(windows))]
    {
        use std::io::Read;
        match fs::File::open("/dev/urandom") {
            Ok(mut f) => f.read_exact(buf).is_ok(),
            Err(_) => false,
        }
    }
}

fn fallback_random(buf: &mut [u8]) {
    // xorshift64* seeded from time + pid. Only used if OS randomness fails.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15);
    let mut state = nanos ^ (std::process::id() as u64).wrapping_mul(0x2545_F491_4F6C_DD1D);
    if state == 0 {
        state = 0x9E37_79B9_7F4A_7C15;
    }
    for chunk in buf.chunks_mut(8) {
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let next = state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        chunk.copy_from_slice(&next.to_le_bytes()[..chunk.len()]);
    }
}

/// Generate a new token (hex, 64 chars).
pub fn generate_token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    if !os_random(&mut bytes) {
        fallback_random(&mut bytes);
    }
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Load the stored token, or generate + store one on first run.
/// Returns `(token, was_newly_generated)`.
pub fn load_or_create(data_dir: &Path) -> Result<(String, bool), String> {
    let path = data_dir.join("auth.token");
    if let Ok(existing) = fs::read_to_string(&path) {
        let token = existing.trim().to_string();
        if token.len() >= 32 {
            return Ok((token, false));
        }
    }
    fs::create_dir_all(data_dir).map_err(|e| format!("cannot create data dir: {}", e))?;
    let token = generate_token();
    fs::write(&path, &token).map_err(|e| format!("cannot store auth token: {}", e))?;
    Ok((token, true))
}

/// Rotate the bearer token: generate a fresh one, store it atomically
/// (temp file + rename, so a crash never leaves a half-written token),
/// and return it. The caller must swap the live in-memory token to the
/// new value in the same step — from that moment the old token is dead.
/// The new token is returned to the caller for storage only; it must
/// never be logged or sent back in an API response (the Flutter UI
/// re-reads `auth.token` from the data dir, which it shares as the same
/// user — that is the only re-pair path).
pub fn rotate_token(data_dir: &Path) -> Result<String, String> {
    let new_token = generate_token();
    fs::create_dir_all(data_dir).map_err(|e| format!("cannot create data dir: {}", e))?;
    let path = data_dir.join("auth.token");
    let tmp = data_dir.join("auth.token.tmp");
    fs::write(&tmp, &new_token).map_err(|e| format!("cannot store rotated token: {}", e))?;
    fs::rename(&tmp, &path).map_err(|e| format!("cannot store rotated token: {}", e))?;
    Ok(new_token)
}

/// Constant-ish comparison: compare lengths and bytes without early exit
/// on the first mismatch (best effort in safe Rust; timing attacks against
/// a Tailscale-local token are out of scope, but there is no reason to
/// leak the mismatch position).
pub fn tokens_equal(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    if ab.len() != bb.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..ab.len() {
        diff |= ab[i] ^ bb[i];
    }
    diff == 0
}

/// Parse an `Authorization: Bearer <token>` header value.
pub fn bearer_token(header_value: &str) -> Option<&str> {
    let v = header_value.trim();
    v.strip_prefix("Bearer ").map(str::trim).filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_tokens_are_long_hex_and_differ() {
        let a = generate_token();
        let b = generate_token();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(bearer_token("Bearer abc123"), Some("abc123"));
        assert_eq!(bearer_token("bearer abc123"), None);
        assert_eq!(bearer_token("Bearer "), None);
        assert_eq!(bearer_token("Basic abc"), None);
    }

    #[test]
    fn rotation_replaces_the_stored_token() {
        let dir = std::env::temp_dir().join(format!(
            "understudy-rotate-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let (old, _) = load_or_create(&dir).expect("create");
        let new = rotate_token(&dir).expect("rotate");
        assert_ne!(old, new);
        assert_eq!(new.len(), 64);
        let (loaded, was_new) = load_or_create(&dir).expect("reload");
        assert_eq!(loaded, new);
        assert!(!was_new);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn equality_is_exact() {
        assert!(tokens_equal("abc", "abc"));
        assert!(!tokens_equal("abc", "abd"));
        assert!(!tokens_equal("abc", "ab"));
    }
}
