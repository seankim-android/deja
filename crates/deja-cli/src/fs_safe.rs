//! Zero-`unsafe` symlink-safe and atomic filesystem helpers for `deja-cli`.

use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

/// Normalize `.` and `..` path components lexically without hitting the filesystem.
#[must_use]
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() && !out.has_root() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Returns `true` when `candidate` is strictly inside `root` (and not equal to `root`).
#[must_use]
pub fn path_inside(root: &Path, candidate: &Path) -> bool {
    candidate != root && candidate.starts_with(root)
}

/// Returns `true` when an I/O error represents `ELOOP` (symlink refused by `O_NOFOLLOW`).
#[must_use]
pub fn is_eloop(err: &io::Error) -> bool {
    #[cfg(unix)]
    {
        if err.raw_os_error() == Some(libc::ELOOP) {
            return true;
        }
    }
    let _ = err;
    false
}

/// Open an existing regular file for reading without following symlinks (`O_NOFOLLOW` on Unix).
pub fn open_read_nofollow(path: &Path) -> io::Result<File> {
    let lmeta = fs::symlink_metadata(path)?;
    if lmeta.file_type().is_symlink() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("refusing to follow symbolic link: {}", path.display()),
        ));
    }
    if !lmeta.file_type().is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("expected a regular file: {}", path.display()),
        ));
    }
    let mut opts = OpenOptions::new();
    opts.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    let file = opts.open(path)?;
    let fmeta = file.metadata()?;
    if !fmeta.is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("expected a regular file: {}", path.display()),
        ));
    }
    Ok(file)
}

/// Read an entire file as UTF-8 string using `open_read_nofollow`, bounded to `max_bytes`.
pub fn read_to_string_nofollow_bounded(path: &Path, max_bytes: u64) -> io::Result<String> {
    let file = open_read_nofollow(path)?;
    let meta = file.metadata()?;
    if meta.len() > max_bytes {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "file {} exceeds maximum size of {max_bytes} bytes",
                path.display()
            ),
        ));
    }
    let mut buf = Vec::with_capacity(usize::try_from(meta.len().min(max_bytes)).unwrap_or(0));
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)?;
    if (buf.len() as u64) > max_bytes {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            format!(
                "file {} exceeds maximum size of {max_bytes} bytes",
                path.display()
            ),
        ));
    }
    String::from_utf8(buf).map_err(|err| {
        io::Error::new(
            ErrorKind::InvalidData,
            format!("file {} is not valid UTF-8: {err}", path.display()),
        )
    })
}

/// Create a brand-new regular file exclusively (`O_CREAT | O_EXCL | O_NOFOLLOW` on Unix).
pub fn create_new_nofollow(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)
}

/// Returns `true` when `path` is a symlink or escapes `canonical_root`.
pub fn is_symlink_or_escapes(canonical_root: &Path, path: &Path) -> io::Result<bool> {
    let normalized = if path.is_absolute() {
        lexical_normalize(path)
    } else {
        lexical_normalize(&canonical_root.join(path))
    };
    if normalized != canonical_root && !path_inside(canonical_root, &normalized) {
        return Ok(true);
    }
    match fs::symlink_metadata(&normalized) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Ok(true);
            }
            let canonical = fs::canonicalize(&normalized)?;
            Ok(canonical != canonical_root && !path_inside(canonical_root, &canonical))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

/// Verify that `target.parent()` stays confined inside `canonical_root` before creating any
/// missing directories, then re-verify canonical confinement after creation.
pub fn ensure_confined_parent_dir(canonical_root: &Path, target: &Path) -> io::Result<PathBuf> {
    let normalized_target = lexical_normalize(target);
    if !path_inside(canonical_root, &normalized_target) {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("path escapes workspace: {}", target.display()),
        ));
    }
    let parent = normalized_target.parent().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("target has no parent directory: {}", target.display()),
        )
    })?;

    let mut ancestor = parent.to_path_buf();
    loop {
        match fs::symlink_metadata(&ancestor) {
            Ok(_) => {
                let canonical_ancestor = fs::canonicalize(&ancestor)?;
                if canonical_ancestor != canonical_root
                    && !path_inside(canonical_root, &canonical_ancestor)
                {
                    return Err(io::Error::new(
                        ErrorKind::PermissionDenied,
                        format!(
                            "parent ancestor escapes workspace via symlink: {}",
                            ancestor.display()
                        ),
                    ));
                }
                if !fs::metadata(&canonical_ancestor)?.is_dir() {
                    return Err(io::Error::new(
                        ErrorKind::InvalidInput,
                        format!("parent ancestor is not a directory: {}", ancestor.display()),
                    ));
                }
                break;
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                if ancestor == canonical_root {
                    return Err(io::Error::new(
                        ErrorKind::NotFound,
                        format!(
                            "workspace root does not exist: {}",
                            canonical_root.display()
                        ),
                    ));
                }
                let Some(next) = ancestor.parent() else {
                    return Err(io::Error::new(
                        ErrorKind::PermissionDenied,
                        format!("no safe ancestor inside workspace for {}", parent.display()),
                    ));
                };
                ancestor = next.to_path_buf();
            }
            Err(err) => return Err(err),
        }
    }

    fs::create_dir_all(parent)?;
    let canonical_parent = fs::canonicalize(parent)?;
    if canonical_parent != canonical_root && !path_inside(canonical_root, &canonical_parent) {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "created parent directory escapes workspace: {}",
                parent.display()
            ),
        ));
    }
    Ok(canonical_parent)
}

/// Generate a collision-resistant nonce for temporary sibling files.
fn temp_nonce() -> String {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}.{seq}.{nanos:x}", std::process::id())
}

/// Atomically write `bytes` to `target` using an `O_EXCL | O_NOFOLLOW` sibling temporary file
/// followed by `sync_all` and `rename`. Refuses to replace a symlink or directory.
pub fn write_atomic_nofollow_excl(target: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        // Refuse if parent itself is a symlink (unless it is a system directory outside our control?
        // Only check direct symlink on target; parent directory is created if missing).
        fs::create_dir_all(parent)?;
    }

    if let Ok(lmeta) = fs::symlink_metadata(target) {
        if lmeta.file_type().is_symlink() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("refusing to overwrite symbolic link: {}", target.display()),
            ));
        }
        if !lmeta.file_type().is_file() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "refusing to overwrite non-regular file: {}",
                    target.display()
                ),
            ));
        }
    }

    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "deja.out".to_string());
    let temp_path = target.with_file_name(format!(".{file_name}.{}.tmp", temp_nonce()));

    let write_res = (|| -> io::Result<()> {
        let mut f = create_new_nofollow(&temp_path)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(err) = write_res {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }

    // Re-check target immediately before rename to narrow symlink race window.
    if let Ok(lmeta) = fs::symlink_metadata(target)
        && lmeta.file_type().is_symlink()
    {
        let _ = fs::remove_file(&temp_path);
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("refusing to overwrite symbolic link: {}", target.display()),
        ));
    }

    if let Err(err) = fs::rename(&temp_path, target) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    Ok(())
}

struct TempFileGuard {
    path: Option<PathBuf>,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(p) = self.path.take() {
            let _ = fs::remove_file(p);
        }
    }
}

fn check_target_not_symlink_or_dir(target: &Path) -> io::Result<()> {
    if let Ok(lmeta) = fs::symlink_metadata(target) {
        if lmeta.file_type().is_symlink() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("refusing to overwrite symbolic link: {}", target.display()),
            ));
        }
        if !lmeta.file_type().is_file() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "refusing to overwrite non-regular file: {}",
                    target.display()
                ),
            ));
        }
    }
    Ok(())
}

fn stage_temp_file(target: &Path, bytes: &[u8]) -> io::Result<(PathBuf, TempFileGuard)> {
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    check_target_not_symlink_or_dir(target)?;

    let file_name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "deja.out".to_string());
    let temp_path = target.with_file_name(format!(".{file_name}.{}.tmp", temp_nonce()));
    let mut file = create_new_nofollow(&temp_path)?;
    let guard = TempFileGuard::new(temp_path.clone());
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok((temp_path, guard))
}

/// Atomically stage BOTH files via `O_EXCL | O_NOFOLLOW` sibling temporaries first, then publish
/// `first_target` before `second_target`. If staging either file or publishing `first_target` fails,
/// all temporaries are removed and `second_target` is never published.
pub fn write_atomic_pair_nofollow_excl(
    first_target: &Path,
    first_bytes: &[u8],
    second_target: &Path,
    second_bytes: &[u8],
) -> io::Result<()> {
    let (first_tmp, mut first_guard) = stage_temp_file(first_target, first_bytes)?;
    let (second_tmp, mut second_guard) = stage_temp_file(second_target, second_bytes)?;

    check_target_not_symlink_or_dir(first_target)?;
    check_target_not_symlink_or_dir(second_target)?;

    fs::rename(&first_tmp, first_target)?;
    first_guard.disarm();

    if let Err(err) = fs::rename(&second_tmp, second_target) {
        let _ = fs::remove_file(first_target);
        return Err(err);
    }
    second_guard.disarm();
    Ok(())
}

/// Atomically write `bytes` to `target` strictly inside `canonical_root` with full ancestor
/// symlink confinement and `O_EXCL | O_NOFOLLOW` staging.
pub fn write_atomic_confined(canonical_root: &Path, target: &Path, bytes: &[u8]) -> io::Result<()> {
    let canonical_parent = ensure_confined_parent_dir(canonical_root, target)?;
    let file_name = target.file_name().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("target path has no file name: {}", target.display()),
        )
    })?;
    let confined_target = canonical_parent.join(file_name);
    write_atomic_nofollow_excl(&confined_target, bytes)
}

/// Platform file identity used to verify a file has not been swapped between check and delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(not(unix))]
    len: u64,
}

impl FileIdentity {
    #[must_use]
    pub fn from_metadata(meta: &Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                dev: meta.dev(),
                ino: meta.ino(),
            }
        }
        #[cfg(not(unix))]
        {
            Self { len: meta.len() }
        }
    }
}

/// Error returned when validating or removing gate evidence paths.
#[derive(Debug, thiserror::Error)]
pub enum GateEvidenceError {
    /// Policy / safety refusal -> CLI exit `2`.
    #[error("{0}")]
    Refused(String),
    /// Filesystem write/delete error -> CLI exit `11`.
    #[error("{0}")]
    Io(String),
}

/// Verified existing gate evidence files recorded prior to deletion.
#[derive(Debug, Clone)]
pub struct GateEvidencePrecheck {
    pub json_path: PathBuf,
    pub txt_path: PathBuf,
    pub existing_json: Option<FileIdentity>,
    pub existing_txt: Option<FileIdentity>,
}

/// Compute the sibling `.txt` evidence table path for a given JSON evidence path.
#[must_use]
pub fn sibling_evidence_txt_path(evidence_json: &Path) -> PathBuf {
    let raw = evidence_json.to_string_lossy();
    if let Some(stem) = raw.strip_suffix(".json") {
        PathBuf::from(format!("{stem}.txt"))
    } else {
        PathBuf::from(format!("{raw}.txt"))
    }
}

/// Validate gate evidence JSON and sibling `.txt` paths before touching anything on disk.
///
/// Enforces:
/// - refuses any path inside `<workspace>/.deja/` outside `<workspace>/.deja/gate/`,
/// - refuses overwriting any forbidden path (goals file, manifest file),
/// - refuses symlinks or non-regular files at either target path,
/// - refuses clobbering any existing `.json` file that does not have `"kind": "deja-gate-evidence"`,
/// - refuses clobbering any existing `.txt` file unless the sibling `.json` exists, is valid
///   evidence, and re-rendering its table via `render_table` matches the `.txt` file byte-for-byte.
pub fn precheck_gate_evidence_paths<F>(
    canonical_workspace: &Path,
    evidence_json: &Path,
    evidence_txt: &Path,
    forbidden_paths: &[PathBuf],
    render_table: F,
) -> Result<GateEvidencePrecheck, GateEvidenceError>
where
    F: Fn(&serde_json::Value) -> Result<String, String>,
{
    let norm_json = lexical_normalize(evidence_json);
    let norm_txt = lexical_normalize(evidence_txt);

    let deja_dir = canonical_workspace.join(".deja");
    let gate_dir = deja_dir.join("gate");

    for target in [&norm_json, &norm_txt] {
        // Resolve nearest existing ancestor to detect symlinks inside workspace pointing into `.deja/`.
        let check_path = resolve_with_nearest_ancestor(target).unwrap_or_else(|_| target.clone());
        for candidate in [target.as_path(), check_path.as_path()] {
            if (candidate == deja_dir || path_inside(&deja_dir, candidate))
                && !path_inside(&gate_dir, candidate)
            {
                return Err(GateEvidenceError::Refused(format!(
                    "refusing to write gate evidence inside .deja/ outside .deja/gate/: {}",
                    target.display()
                )));
            }
        }
        for forbidden in forbidden_paths {
            let norm_forbidden = lexical_normalize(forbidden);
            let canon_forbidden = fs::canonicalize(forbidden).ok();
            if target == &norm_forbidden
                || canon_forbidden.as_deref() == Some(check_path.as_path())
                || canon_forbidden.as_deref() == Some(target.as_path())
            {
                return Err(GateEvidenceError::Refused(format!(
                    "refusing to overwrite protected input file as gate evidence: {}",
                    target.display()
                )));
            }
        }
        // Reject symlinked in-workspace ancestors so `.deja/gate/evil -> /etc` is caught.
        check_no_symlink_under_workspace(canonical_workspace, target)?;
    }

    let (existing_json, previous_json_val) = match fs::symlink_metadata(&norm_json) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(GateEvidenceError::Refused(format!(
                    "refusing to replace symbolic link gate evidence path: {}",
                    norm_json.display()
                )));
            }
            if !meta.file_type().is_file() {
                return Err(GateEvidenceError::Refused(format!(
                    "refusing to replace non-file gate evidence path: {}",
                    norm_json.display()
                )));
            }
            let text =
                read_to_string_nofollow_bounded(&norm_json, 8 * 1024 * 1024).map_err(|err| {
                    GateEvidenceError::Refused(format!(
                        "cannot read existing evidence file {}: {err}",
                        norm_json.display()
                    ))
                })?;
            let val: serde_json::Value = serde_json::from_str(&text).map_err(|_| {
                GateEvidenceError::Refused(format!(
                    "refusing to clobber non-evidence file (invalid JSON): {}",
                    norm_json.display()
                ))
            })?;
            let kind = val.get("kind").and_then(serde_json::Value::as_str);
            if kind != Some("deja-gate-evidence") {
                return Err(GateEvidenceError::Refused(format!(
                    "refusing to clobber existing file that is not deja-gate-evidence: {}",
                    norm_json.display()
                )));
            }
            (Some(FileIdentity::from_metadata(&meta)), Some(val))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => (None, None),
        Err(err) => {
            return Err(GateEvidenceError::Refused(format!(
                "cannot inspect gate evidence path {}: {err}",
                norm_json.display()
            )));
        }
    };

    let existing_txt = match fs::symlink_metadata(&norm_txt) {
        Ok(meta) => {
            if meta.file_type().is_symlink() {
                return Err(GateEvidenceError::Refused(format!(
                    "refusing to replace symbolic link gate text table: {}",
                    norm_txt.display()
                )));
            }
            if !meta.file_type().is_file() {
                return Err(GateEvidenceError::Refused(format!(
                    "refusing to replace non-file gate text table: {}",
                    norm_txt.display()
                )));
            }
            let Some(ref prev_json) = previous_json_val else {
                return Err(GateEvidenceError::Refused(format!(
                    "refusing to clobber existing text file without matching evidence JSON: {}",
                    norm_txt.display()
                )));
            };
            let expected_table = render_table(prev_json).map_err(|err| {
                GateEvidenceError::Refused(format!(
                    "existing evidence JSON cannot render text table ({err}); refusing to clobber {}",
                    norm_txt.display()
                ))
            })?;
            let actual_table = read_to_string_nofollow_bounded(&norm_txt, 8 * 1024 * 1024)
                .map_err(|err| {
                    GateEvidenceError::Refused(format!(
                        "cannot read existing evidence text table {}: {err}",
                        norm_txt.display()
                    ))
                })?;
            if actual_table != expected_table {
                return Err(GateEvidenceError::Refused(format!(
                    "refusing to clobber modified or unrelated text file beside gate evidence: {}",
                    norm_txt.display()
                )));
            }
            Some(FileIdentity::from_metadata(&meta))
        }
        Err(err) if err.kind() == ErrorKind::NotFound => None,
        Err(err) => {
            return Err(GateEvidenceError::Refused(format!(
                "cannot inspect gate text table path {}: {err}",
                norm_txt.display()
            )));
        }
    };

    Ok(GateEvidencePrecheck {
        json_path: norm_json,
        txt_path: norm_txt,
        existing_json,
        existing_txt,
    })
}

/// Delete previously verified evidence files only after confirming their `(dev, ino)` identity
/// has not changed and neither became a symlink.
pub fn remove_verified_gate_evidence(
    precheck: &GateEvidencePrecheck,
) -> Result<(), GateEvidenceError> {
    for (path, expected_id) in [
        (&precheck.json_path, precheck.existing_json),
        (&precheck.txt_path, precheck.existing_txt),
    ] {
        let Some(expected) = expected_id else {
            continue;
        };
        let meta = match fs::symlink_metadata(path) {
            Ok(m) => m,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(GateEvidenceError::Io(format!(
                    "cannot verify gate evidence before deletion ({}): {err}",
                    path.display()
                )));
            }
        };
        if meta.file_type().is_symlink() || !meta.file_type().is_file() {
            return Err(GateEvidenceError::Refused(format!(
                "gate evidence target changed to a symlink or non-file before deletion: {}",
                path.display()
            )));
        }
        if FileIdentity::from_metadata(&meta) != expected {
            return Err(GateEvidenceError::Refused(format!(
                "gate evidence file changed on disk between check and deletion: {}",
                path.display()
            )));
        }
        fs::remove_file(path).map_err(|err| {
            GateEvidenceError::Io(format!(
                "failed to remove previous gate evidence {}: {err}",
                path.display()
            ))
        })?;
    }
    Ok(())
}

pub fn resolve_with_nearest_ancestor(target: &Path) -> io::Result<PathBuf> {
    let mut cur = target.to_path_buf();
    let mut tail = Vec::new();
    loop {
        match fs::canonicalize(&cur) {
            Ok(mut base) => {
                for seg in tail.into_iter().rev() {
                    base.push(seg);
                }
                return Ok(base);
            }
            Err(err) if err.kind() == ErrorKind::NotFound => {
                let Some(name) = cur.file_name().map(std::ffi::OsStr::to_os_string) else {
                    return Err(err);
                };
                tail.push(name);
                let Some(parent) = cur.parent() else {
                    return Err(err);
                };
                cur = parent.to_path_buf();
            }
            Err(err) => return Err(err),
        }
    }
}

fn check_no_symlink_under_workspace(
    canonical_workspace: &Path,
    target: &Path,
) -> Result<(), GateEvidenceError> {
    if let Ok(rel) = target.strip_prefix(canonical_workspace) {
        let mut cursor = canonical_workspace.to_path_buf();
        for comp in rel.components() {
            cursor.push(comp);
            match fs::symlink_metadata(&cursor) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    return Err(GateEvidenceError::Refused(format!(
                        "refusing to traverse symbolic link for gate evidence: {}",
                        cursor.display()
                    )));
                }
                Ok(_) => {}
                Err(err) if err.kind() == ErrorKind::NotFound => break,
                Err(err) => {
                    return Err(GateEvidenceError::Refused(format!(
                        "cannot inspect gate evidence path {}: {err}",
                        cursor.display()
                    )));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("deja-fs-safe-{label}-{}", temp_nonce()));
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(&dir).unwrap()
    }

    #[test]
    fn write_atomic_and_read_nofollow_round_trip() {
        let dir = make_temp_dir("roundtrip");
        let file = dir.join("sub/test.json");
        write_atomic_confined(&dir, &file, br#"{"ok":true}"#).unwrap();
        let got = read_to_string_nofollow_bounded(&file, 1024).unwrap();
        assert_eq!(got, r#"{"ok":true}"#);
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn open_read_and_write_atomic_refuse_symlinks() {
        use std::os::unix::fs::symlink;
        let dir = make_temp_dir("symlink");
        let real = dir.join("real.txt");
        let link = dir.join("link.txt");
        fs::write(&real, b"hello").unwrap();
        symlink(&real, &link).unwrap();

        assert!(open_read_nofollow(&link).is_err());
        assert!(write_atomic_nofollow_excl(&link, b"new").is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
