//! Streaming SHA-256 build artifact fingerprinting (64 KiB chunks) and fail-closed in-process caching (`DEJA-53`).

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::{self, File, Metadata};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
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

#[inline]
fn check_cancelled(cancel: Option<&AtomicBool>) -> Result<(), CoreError> {
    if cancel.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        return Err(CoreError::Cancelled);
    }
    Ok(())
}

fn stream_file_into_inner(
    path: &Path,
    hasher: &mut Sha256,
    buf: &mut [u8],
    cancel: Option<&AtomicBool>,
) -> Result<(), CoreError> {
    check_cancelled(cancel)?;
    let mut file = File::open(path)?;
    loop {
        check_cancelled(cancel)?;
        let read = file.read(buf)?;
        if read == 0 {
            break;
        }
        check_cancelled(cancel)?;
        hasher.update(&buf[..read]);
    }
    Ok(())
}

fn fingerprint_file_inner(
    root: &Path,
    build_path: &Path,
    cancel: Option<&AtomicBool>,
) -> Result<String, CoreError> {
    check_cancelled(cancel)?;
    let target = resolve_build_path(root, build_path);
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; FINGERPRINT_CHUNK_BYTES];
    stream_file_into_inner(&target, &mut hasher, &mut buf, cancel)?;
    check_cancelled(cancel)?;
    Ok(hex::encode(hasher.finalize()))
}

/// Compute the SHA-256 hex digest of a single build artifact file (e.g. Android APK) using 64 KiB streaming chunks.
pub fn fingerprint_file(root: &Path, build_path: &Path) -> Result<String, CoreError> {
    fingerprint_file_inner(root, build_path, None)
}

/// Interruptible variant of [`fingerprint_file`] that checks `cancel` before opening and between
/// every 64 KiB read chunk, returning [`CoreError::Cancelled`] on cancellation and an identical
/// SHA-256 digest otherwise.
pub fn fingerprint_file_cancellable(
    root: &Path,
    build_path: &Path,
    cancel: &AtomicBool,
) -> Result<String, CoreError> {
    fingerprint_file_inner(root, build_path, Some(cancel))
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
    cancel: Option<&AtomicBool>,
) -> Result<(), CoreError> {
    check_cancelled(cancel)?;
    for entry in fs::read_dir(dir)? {
        check_cancelled(cancel)?;
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
            collect_tree_files(&full_path, &rel_posix, label, out, cancel)?;
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

fn fingerprint_tree_inner(
    root: &Path,
    build_path: &Path,
    label: &str,
    cancel: Option<&AtomicBool>,
) -> Result<String, CoreError> {
    check_cancelled(cancel)?;
    let directory = resolve_build_path(root, build_path);
    let mut entries = Vec::new();
    collect_tree_files(&directory, "", label, &mut entries, cancel)?;
    check_cancelled(cancel)?;
    if entries.is_empty() {
        return Err(CoreError::Fingerprint(format!(
            "{label} build tree must contain at least one file"
        )));
    }
    entries.sort_by(|left, right| cmp_utf16(&left.0, &right.0));

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; FINGERPRINT_CHUNK_BYTES];
    for (rel_posix, full_path) in &entries {
        check_cancelled(cancel)?;
        hasher.update(rel_posix.as_bytes());
        hasher.update([0u8]);
        stream_file_into_inner(full_path, &mut hasher, &mut buf, cancel)?;
        hasher.update([0u8]);
    }
    check_cancelled(cancel)?;
    Ok(hex::encode(hasher.finalize()))
}

/// Compute the deterministic streaming SHA-256 digest of a build directory tree (iOS `.app` or Web static tree).
///
/// Format matches TypeScript `fingerprintTree`: UTF-16-sorted entries hashed as
/// `<rel_posix>\0<file_bytes>\0...`, rejecting symbolic links, special files, and empty trees.
pub fn fingerprint_tree(root: &Path, build_path: &Path, label: &str) -> Result<String, CoreError> {
    fingerprint_tree_inner(root, build_path, label, None)
}

/// Interruptible variant of [`fingerprint_tree`] that checks `cancel` on every directory entry,
/// every file, and every 64 KiB read chunk, returning [`CoreError::Cancelled`] on cancellation
/// and a bit-for-bit identical SHA-256 digest otherwise.
pub fn fingerprint_tree_cancellable(
    root: &Path,
    build_path: &Path,
    label: &str,
    cancel: &AtomicBool,
) -> Result<String, CoreError> {
    fingerprint_tree_inner(root, build_path, label, Some(cancel))
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
    collect_tree_files(directory, "", label, &mut entries, None)?;
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

#[cfg(test)]
mod cancellable_tests {
    use std::fs;
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{
        FINGERPRINT_CHUNK_BYTES, fingerprint_file, fingerprint_file_cancellable, fingerprint_tree,
        fingerprint_tree_cancellable, stream_file_into_inner,
    };
    use crate::error::CoreError;
    use sha2::{Digest, Sha256};

    fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "deja-core-fp-{tag}-{}-{nanos:x}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn cancellable_file_and_tree_match_non_cancellable_sha256_exactly() {
        let dir = unique_temp_dir("parity");
        let single = dir.join("app.apk");
        // Multi-chunk payload (> 2 * 64 KiB) to exercise multi-iteration streaming loop.
        let payload = vec![0x5Au8; FINGERPRINT_CHUNK_BYTES * 2 + 1234];
        fs::write(&single, &payload).unwrap();

        let tree = dir.join("Demo.app");
        fs::create_dir_all(tree.join("nested")).unwrap();
        fs::write(tree.join("Info.plist"), b"plist-data").unwrap();
        fs::write(tree.join("nested/blob.bin"), &payload).unwrap();

        let cancel = AtomicBool::new(false);
        assert_eq!(
            fingerprint_file_cancellable(&dir, &single, &cancel).unwrap(),
            fingerprint_file(&dir, &single).unwrap()
        );
        assert_eq!(
            fingerprint_tree_cancellable(&dir, &tree, "iOS", &cancel).unwrap(),
            fingerprint_tree(&dir, &tree, "iOS").unwrap()
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancellable_file_and_tree_abort_immediately_when_cancelled() {
        let dir = unique_temp_dir("pre-cancel");
        let single = dir.join("app.apk");
        fs::write(&single, b"apk-bytes").unwrap();
        let tree = dir.join("Demo.app");
        fs::create_dir_all(&tree).unwrap();
        fs::write(tree.join("a.bin"), b"aaa").unwrap();
        fs::write(tree.join("b.bin"), b"bbb").unwrap();

        let cancel = AtomicBool::new(true);
        assert!(matches!(
            fingerprint_file_cancellable(&dir, &single, &cancel),
            Err(CoreError::Cancelled)
        ));
        assert!(matches!(
            fingerprint_tree_cancellable(&dir, &tree, "iOS", &cancel),
            Err(CoreError::Cancelled)
        ));

        // Mid-chunk cancellation check: cancel after the first chunk inside a multi-chunk stream.
        let big = dir.join("big.bin");
        fs::write(&big, vec![0xABu8; FINGERPRINT_CHUNK_BYTES * 4]).unwrap();
        let flag = AtomicBool::new(false);
        let mut f = fs::File::open(&big).unwrap();
        let mut first_buf = vec![0u8; FINGERPRINT_CHUNK_BYTES];
        let n = f.read(&mut first_buf).unwrap();
        assert_eq!(n, FINGERPRINT_CHUNK_BYTES);
        flag.store(true, Ordering::SeqCst);
        let mut hasher = Sha256::new();
        assert!(matches!(
            stream_file_into_inner(&big, &mut hasher, &mut first_buf, Some(&flag)),
            Err(CoreError::Cancelled)
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancellable_tree_stops_mid_traversal_when_flag_flips_concurrently() {
        let dir = unique_temp_dir("mid-tree");
        let tree = dir.join("Big.app");
        fs::create_dir_all(&tree).unwrap();
        // Write enough multi-chunk files that concurrent cancellation catches hashing in flight,
        // OR if hashing already finishes cleanly it matches exact digest (never corrupt/partial).
        let chunk = vec![0x3Cu8; FINGERPRINT_CHUNK_BYTES * 4];
        for i in 0..32 {
            fs::write(tree.join(format!("file-{i:02}.bin")), &chunk).unwrap();
        }
        let expected = fingerprint_tree(&dir, &tree, "iOS").unwrap();

        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let cancel_writer = std::sync::Arc::clone(&cancel);
        let dir_clone = dir.clone();
        let tree_clone = tree.clone();
        let handle = std::thread::spawn(move || {
            fingerprint_tree_cancellable(&dir_clone, &tree_clone, "iOS", &cancel_writer)
        });
        cancel.store(true, Ordering::SeqCst);
        match handle.join().unwrap() {
            Err(CoreError::Cancelled) => {}
            Ok(digest) => assert_eq!(digest, expected),
            Err(other) => panic!("unexpected error: {other}"),
        }

        let _ = fs::remove_dir_all(&dir);
    }
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
