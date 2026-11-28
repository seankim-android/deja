//! Streaming SHA-256 build artifact fingerprinting (64 KiB chunks) and fail-closed in-process caching (`DEJA-53`).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use sha2::{Digest, Sha256};

use crate::canonical_json::cmp_utf16;
use crate::error::CoreError;

/// Fixed streaming buffer size (64 KiB) used when hashing build artifacts (`DEJA-53`).
pub const FINGERPRINT_CHUNK_BYTES: usize = 65_536;

fn resolve_build_path(root: &Path, build_path: &Path) -> PathBuf {
    if build_path.is_absolute() {
        build_path.to_path_buf()
    } else {
        root.join(build_path)
    }
}

fn stream_file_into(path: &Path, hasher: &mut Sha256, buf: &mut [u8]) -> Result<(), CoreError> {
    let mut file = File::open(path)?;
    loop {
        let read = file.read(buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(())
}

/// Compute the SHA-256 hex digest of a single build artifact file (e.g. Android APK) using 64 KiB streaming chunks.
pub fn fingerprint_file(root: &Path, build_path: &Path) -> Result<String, CoreError> {
    let target = resolve_build_path(root, build_path);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; FINGERPRINT_CHUNK_BYTES];
    stream_file_into(&target, &mut hasher, &mut buf)?;
    Ok(hex::encode(hasher.finalize()))
}

fn validate_tree_entry_name<'a>(name: &'a OsStr, label: &str) -> Result<&'a str, CoreError> {
    name.to_str().ok_or_else(|| {
        CoreError::Fingerprint(format!("{label} build tree contains a non-UTF-8 filename"))
    })
}

fn collect_tree_files(
    dir: &Path,
    prefix: &str,
    label: &str,
    out: &mut Vec<(String, PathBuf)>,
) -> Result<(), CoreError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let name = entry.file_name();
        let name_str = validate_tree_entry_name(&name, label)?;
        let rel_posix = if prefix.is_empty() {
            name_str.to_string()
        } else {
            format!("{prefix}/{name_str}")
        };
        let full_path = entry.path();
        if file_type.is_dir() {
            collect_tree_files(&full_path, &rel_posix, label, out)?;
        } else if file_type.is_file() {
            out.push((rel_posix, full_path));
        } else {
            return Err(CoreError::Fingerprint(format!(
                "{label} build tree cannot contain symbolic links or special files"
            )));
        }
    }
    Ok(())
}

/// Compute the deterministic streaming SHA-256 digest of a build directory tree (iOS `.app` or Web static tree).
///
/// Format matches TypeScript `fingerprintTree`: UTF-16-sorted entries hashed as
/// `<rel_posix>\0<file_bytes>\0...`, rejecting symbolic links, special files, and empty trees.
pub fn fingerprint_tree(root: &Path, build_path: &Path, label: &str) -> Result<String, CoreError> {
    let directory = resolve_build_path(root, build_path);
    let mut entries = Vec::new();
    collect_tree_files(&directory, "", label, &mut entries)?;
    if entries.is_empty() {
        return Err(CoreError::Fingerprint(format!(
            "{label} build tree must contain at least one file"
        )));
    }
    entries.sort_by(|left, right| cmp_utf16(&left.0, &right.0));

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; FINGERPRINT_CHUNK_BYTES];
    for (rel_posix, full_path) in &entries {
        hasher.update(rel_posix.as_bytes());
        hasher.update([0u8]);
        stream_file_into(full_path, &mut hasher, &mut buf)?;
        hasher.update([0u8]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStatKey {
    rel_posix: String,
    size: u64,
    mtime_ns: i128,
    ctime_sec: i64,
    ctime_nsec: i64,
    dev: u64,
    ino: u64,
}

#[cfg(unix)]
fn extract_stat_key(rel_posix: String, meta: &Metadata) -> Option<FileStatKey> {
    use std::os::unix::fs::MetadataExt;
    let modified = meta.modified().ok()?;
    let mtime_ns = match modified.duration_since(UNIX_EPOCH) {
        Ok(dur) => i128::from(dur.as_secs()) * 1_000_000_000 + i128::from(dur.subsec_nanos()),
        Err(err) => {
            let dur = err.duration();
            -(i128::from(dur.as_secs()) * 1_000_000_000 + i128::from(dur.subsec_nanos()))
        }
    };
    Some(FileStatKey {
        rel_posix,
        size: meta.len(),
        mtime_ns,
        ctime_sec: meta.ctime(),
        ctime_nsec: meta.ctime_nsec(),
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

#[cfg(not(unix))]
fn extract_stat_key(_rel_posix: String, _meta: &Metadata) -> Option<FileStatKey> {
    // Fail closed when platform inode/device identity is unavailable.
    None
}

#[derive(Debug, Clone)]
struct CachedFingerprint {
    stats: Vec<FileStatKey>,
    digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CacheKey {
    File(PathBuf),
    Tree(PathBuf),
}

/// Fail-closed in-process build fingerprint cache (`DEJA-53`).
///
/// Reuses a previously computed SHA-256 digest only when every file's `(path, size, mtime_ns, dev, ino)`
/// signature matches identically before and after hashing; any stat difference or missing stat metadata
/// forces a full streaming recomputation.
#[derive(Debug, Default)]
pub struct BuildFingerprintCache {
    entries: HashMap<CacheKey, CachedFingerprint>,
    hits: u64,
    misses: u64,
}

impl BuildFingerprintCache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    #[must_use]
    pub fn hits(&self) -> u64 {
        self.hits
    }

    #[must_use]
    pub fn misses(&self) -> u64 {
        self.misses
    }

    /// Return a cached or freshly streamed SHA-256 fingerprint for a single file.
    pub fn fingerprint_file(
        &mut self,
        root: &Path,
        build_path: &Path,
    ) -> Result<String, CoreError> {
        let target = resolve_build_path(root, build_path);
        let cache_key =
            CacheKey::File(fs::canonicalize(&target).unwrap_or_else(|_| target.clone()));
        let pre_stat = fs::metadata(&target)
            .ok()
            .and_then(|meta| extract_stat_key(String::new(), &meta));

        if let Some(ref key_stat) = pre_stat
            && let Some(cached) = self.entries.get(&cache_key)
            && cached.stats.len() == 1
            && &cached.stats[0] == key_stat
        {
            self.hits += 1;
            return Ok(cached.digest.clone());
        }

        self.misses += 1;
        let digest = fingerprint_file(root, build_path)?;

        // Fail closed: cache only if post-hash stat matches pre-hash stat identically.
        let post_stat = fs::metadata(&target)
            .ok()
            .and_then(|meta| extract_stat_key(String::new(), &meta));
        if let (Some(pre), Some(post)) = (pre_stat, post_stat)
            && pre == post
        {
            self.entries.insert(
                cache_key,
                CachedFingerprint {
                    stats: vec![post],
                    digest: digest.clone(),
                },
            );
        } else {
            self.entries.remove(&cache_key);
        }
        Ok(digest)
    }

    /// Return a cached or freshly streamed SHA-256 fingerprint for a directory tree.
    pub fn fingerprint_tree(
        &mut self,
        root: &Path,
        build_path: &Path,
        label: &str,
    ) -> Result<String, CoreError> {
        let directory = resolve_build_path(root, build_path);
        let cache_key =
            CacheKey::Tree(fs::canonicalize(&directory).unwrap_or_else(|_| directory.clone()));

        let pre_stats = collect_tree_stat_snapshot(&directory, label)?;
        if let Some(ref pre) = pre_stats
            && let Some(cached) = self.entries.get(&cache_key)
            && &cached.stats == pre
        {
            self.hits += 1;
            return Ok(cached.digest.clone());
        }

        self.misses += 1;
        let digest = fingerprint_tree(root, build_path, label)?;

        let post_stats = collect_tree_stat_snapshot(&directory, label)?;
        if let (Some(pre), Some(post)) = (pre_stats, post_stats)
            && pre == post
        {
            self.entries.insert(
                cache_key,
                CachedFingerprint {
                    stats: post,
                    digest: digest.clone(),
                },
            );
        } else {
            self.entries.remove(&cache_key);
        }
        Ok(digest)
    }
}

fn collect_tree_stat_snapshot(
    directory: &Path,
    label: &str,
) -> Result<Option<Vec<FileStatKey>>, CoreError> {
    let mut entries = Vec::new();
    collect_tree_files(directory, "", label, &mut entries)?;
    if entries.is_empty() {
        return Err(CoreError::Fingerprint(format!(
            "{label} build tree must contain at least one file"
        )));
    }
    entries.sort_by(|left, right| cmp_utf16(&left.0, &right.0));

    let mut stats = Vec::with_capacity(entries.len());
    for (rel_posix, full_path) in entries {
        let meta = fs::symlink_metadata(&full_path)?;
        if !meta.file_type().is_file() {
            return Err(CoreError::Fingerprint(format!(
                "{label} build tree cannot contain symbolic links or special files"
            )));
        }
        let Some(key) = extract_stat_key(rel_posix, &meta) else {
            return Ok(None);
        };
        stats.push(key);
    }
    Ok(Some(stats))
}

#[cfg(all(test, unix))]
mod unit_tests {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    use super::validate_tree_entry_name;
    use crate::error::CoreError;

    #[test]
    fn rejects_non_utf8_tree_entry_names() {
        let bad = OsString::from_vec(vec![b'b', b'a', b'd', 0xff, b'.', b'b', b'i', b'n']);
        assert!(matches!(
            validate_tree_entry_name(&bad, "iOS"),
            Err(CoreError::Fingerprint(msg)) if msg.contains("non-UTF-8")
        ));
    }
}
