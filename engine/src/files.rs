//! File transfer inside an explicit scope.
//!
//! The engine serves file reads/writes only under a set of allowed root
//! directories (default: the user's profile directory). Every requested
//! path is canonicalized (when it exists) or lexically normalized (when it
//! does not, for writes) and must stay under an allowed root — `..`
//! escapes and absolute paths outside the roots are rejected. All transfers
//! are action-logged by the API layer.

use std::fs;
use std::path::{Component, Path, PathBuf};

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Base64-encode bytes (standard alphabet, padded).
pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Base64-decode (standard alphabet, padding tolerated, whitespace ignored).
pub fn b64_decode(s: &str) -> Result<Vec<u8>, String> {
    let mut vals: Vec<u8> = Vec::new();
    for c in s.chars() {
        if c.is_whitespace() || c == '=' {
            continue;
        }
        let v = match c {
            'A'..='Z' => c as u8 - b'A',
            'a'..='z' => c as u8 - b'a' + 26,
            '0'..='9' => c as u8 - b'0' + 52,
            '+' => 62,
            '/' => 63,
            _ => return Err(format!("invalid base64 character: {}", c)),
        };
        vals.push(v);
    }
    let mut out = Vec::new();
    for chunk in vals.chunks(4) {
        if chunk.len() < 2 {
            return Err("truncated base64 input".to_string());
        }
        let n = ((chunk[0] as u32) << 18)
            | ((chunk[1] as u32) << 12)
            | ((chunk.get(2).copied().unwrap_or(0) as u32) << 6)
            | (chunk.get(3).copied().unwrap_or(0) as u32);
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

/// Lexically normalize a path (resolve `.` and `..` without touching disk).
/// Used for write targets that do not exist yet.
pub fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Resolve a requested path against the allowed roots. Returns the
/// canonical/normalized absolute path, or an error when it escapes scope.
pub fn resolve_in_scope(requested: &str, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let req = Path::new(requested);
    // Absolute request: use as-is. Relative request: anchor at first root.
    let candidate = if req.is_absolute() {
        req.to_path_buf()
    } else {
        match roots.first() {
            Some(root) => root.join(req),
            None => return Err("no file roots configured".to_string()),
        }
    };
    let normalized = match fs::canonicalize(&candidate) {
        Ok(c) => c,
        Err(_) => {
            // Target may not exist yet (uploads). Normalize the parent
            // when possible, then append the file name.
            let parent = candidate.parent().map(|p| p.to_path_buf());
            match parent.and_then(|p| fs::canonicalize(&p).ok()) {
                Some(cp) => match candidate.file_name() {
                    Some(name) => cp.join(name),
                    None => cp,
                },
                None => normalize_lexical(&candidate),
            }
        }
    };
    for root in roots {
        let canon_root = fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        if normalized.starts_with(&canon_root) {
            return Ok(normalized);
        }
    }
    Err(format!("path is outside the allowed file roots: {}", requested))
}

pub fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("cannot read {}: {}", path.display(), e))
}

pub fn write_file(path: &Path, data: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {}", parent.display(), e))?;
    }
    fs::write(path, data).map_err(|e| format!("cannot write {}: {}", path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        for data in [&b""[..], b"f", b"fo", b"foo", b"hello world", &[0, 255, 128, 1, 2, 3]] {
            let enc = b64_encode(data);
            assert_eq!(b64_decode(&enc).unwrap(), data);
        }
        assert_eq!(b64_encode(b"foo"), "Zm9v");
        assert!(b64_decode("!!!").is_err());
    }

    #[test]
    fn lexical_normalization() {
        assert_eq!(
            normalize_lexical(Path::new("a/b/../c/./d")),
            PathBuf::from("a/c/d")
        );
    }

    #[test]
    fn scope_rejects_escape() {
        let dir = std::env::temp_dir().join("understudy-scope-test");
        let _ = fs::create_dir_all(&dir);
        let roots = vec![dir.clone()];
        // A path inside the root resolves.
        let inside = dir.join("sub").join("file.txt");
        let _ = fs::create_dir_all(dir.join("sub"));
        let _ = fs::write(&inside, b"x");
        assert!(resolve_in_scope(&inside.to_string_lossy(), &roots).is_ok());
        // A traversal escaping the root is rejected.
        let escape = format!("{}/../outside.txt", dir.display());
        assert!(resolve_in_scope(&escape, &roots).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
