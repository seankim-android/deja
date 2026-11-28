//! Zero-`unsafe` symlink-safe, hardlink-rejecting, and workspace-confined filesystem helpers
//! for `deja-mcp`.

use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use deja_core::{FlowClosure, FlowReferenceKind, MAX_FILE_BYTES, MAX_MEDIA_BYTES};

/// Maximum report file size readable by `deja_inspect` (16 MiB).
pub const MAX_REPORT_BYTES: u64 = 16 * 1024 * 1024;

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

/// Convert a path inside `workspace_root` to a clean workspace-relative `/` display string.
#[must_use]
pub fn workspace_relative_display(workspace_root: &Path, path: &Path) -> String {
    let norm_ws = lexical_normalize(workspace_root);
    let norm_p = lexical_normalize(path);
    if let Ok(rel) = norm_p.strip_prefix(&norm_ws) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    if let (Ok(cws), Ok(cp)) = (fs::canonicalize(workspace_root), fs::canonicalize(path))
        && let Ok(rel) = cp.strip_prefix(&cws)
    {
        return rel.to_string_lossy().replace('\\', "/");
    }
    norm_p.to_string_lossy().into_owned()
}

/// Open an existing regular file for reading without following symlinks (`O_NOFOLLOW` on Unix),
/// and rejecting hardlinks (`nlink != 1` on Unix) when `reject_hardlinks` is `true`.
pub fn open_read_nofollow_regular(path: &Path, reject_hardlinks: bool) -> io::Result<File> {
    let lmeta = fs::symlink_metadata(path)?;
    if lmeta.file_type().is_symlink() {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("refusing to follow symbolic link: {}", path.display()),
        ));
    }
    if !lmeta.file_type().is_file() {
        return Err(io::Error::new(
            ErrorKind::InvalidInput,
            format!("expected a regular file: {}", path.display()),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if reject_hardlinks && lmeta.nlink() != 1 {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!("refusing to read hardlinked file: {}", path.display()),
            ));
        }
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
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if fmeta.dev() != lmeta.dev() || fmeta.ino() != lmeta.ino() {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "file changed concurrently while opening: {}",
                    path.display()
                ),
            ));
        }
        if reject_hardlinks && fmeta.nlink() != 1 {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!("refusing to read hardlinked file: {}", path.display()),
            ));
        }
    }
    #[cfg(not(unix))]
    {
        let _ = reject_hardlinks;
    }
    Ok(file)
}

/// Read an entire file opened via [`open_read_nofollow_regular`] bounded to `max_bytes`.
pub fn read_bytes_nofollow_bounded(
    path: &Path,
    max_bytes: u64,
    reject_hardlinks: bool,
) -> io::Result<Vec<u8>> {
    let file = open_read_nofollow_regular(path, reject_hardlinks)?;
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
    Ok(buf)
}

/// Validate that a caller-supplied relative path has no NUL bytes, backslashes, Windows drive
/// prefixes, root prefixes, or `..` traversal components, returning its clean normal segments.
pub fn parse_safe_relative_segments(raw: &str) -> Result<Vec<String>, String> {
    if raw.is_empty() || raw.trim().is_empty() {
        return Err("path must not be empty".to_string());
    }
    if raw.contains('\0') {
        return Err("path must not contain NUL bytes".to_string());
    }
    if raw.contains('\\') {
        return Err("path must use forward slashes and must not contain backslashes".to_string());
    }
    if raw.starts_with('/') {
        return Err(
            "path must be relative to the workspace root (absolute paths are rejected)".to_string(),
        );
    }
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err("path must be relative (drive prefixes are rejected)".to_string());
    }

    let mut segments = Vec::new();
    for component in Path::new(raw).components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => {
                let s = part
                    .to_str()
                    .ok_or_else(|| "path contains invalid UTF-8".to_string())?;
                if s.is_empty() {
                    continue;
                }
                segments.push(s.to_string());
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!(
                    "path `{raw}` escapes confinement (parent `..` and rooted paths are rejected)"
                ));
            }
        }
    }
    if segments.is_empty() {
        return Err("path must reference a file".to_string());
    }
    Ok(segments)
}

/// Verify that every path segment between `canonical_root` and `segments` resolves strictly
/// inside `canonical_root` with zero symbolic links along the path.
pub fn verify_no_symlink_components(
    canonical_root: &Path,
    segments: &[String],
) -> io::Result<PathBuf> {
    let mut cursor = canonical_root.to_path_buf();
    for seg in segments {
        cursor.push(seg);
        let lmeta = fs::symlink_metadata(&cursor)?;
        if lmeta.file_type().is_symlink() {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!("refusing to traverse symbolic link: {}", cursor.display()),
            ));
        }
    }
    let canonical = fs::canonicalize(&cursor)?;
    if !path_inside(canonical_root, &canonical) {
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!("path escapes confined directory: {}", cursor.display()),
        ));
    }
    Ok(cursor)
}

/// Strictly read a report JSON file confined inside the active store's reports directory:
/// - when `store_id == Some(id)`: `<canonical_workspace>/.deja/reports/stores/<id>/`
/// - when `store_id == None`: `<canonical_workspace>/.deja/reports/` (rejecting `.deja/reports/stores/...`)
///
/// Enforces `O_NOFOLLOW`, `!is_symlink()`, `is_file()`, Unix `nlink == 1`, no symlink ancestors,
/// and a `16 MiB` size cap.
pub fn read_confined_report_file(
    canonical_workspace: &Path,
    store_id: Option<&str>,
    report_rel: &str,
) -> Result<(PathBuf, String), (&'static str, String)> {
    let segments =
        parse_safe_relative_segments(report_rel).map_err(|msg| ("PATH_CONFINEMENT", msg))?;

    let reports_dir = match store_id {
        Some(id) => {
            if segments.len() < 5
                || segments[0] != ".deja"
                || segments[1] != "reports"
                || segments[2] != "stores"
                || segments[3] != id
            {
                return Err((
                    "PATH_CONFINEMENT",
                    format!(
                        "report path `{report_rel}` must stay strictly inside `.deja/reports/stores/{id}/`"
                    ),
                ));
            }
            canonical_workspace
                .join(".deja")
                .join("reports")
                .join("stores")
                .join(id)
        }
        None => {
            if segments.len() < 3
                || segments[0] != ".deja"
                || segments[1] != "reports"
                || segments[2] == "stores"
            {
                return Err((
                    "PATH_CONFINEMENT",
                    format!(
                        "report path `{report_rel}` must stay strictly inside `.deja/reports/` (outside per-store directories)"
                    ),
                ));
            }
            canonical_workspace.join(".deja").join("reports")
        }
    };

    let candidate =
        verify_no_symlink_components(canonical_workspace, &segments).map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                (
                    "REPORT_NOT_FOUND",
                    format!("report file not found: `{report_rel}`"),
                )
            } else {
                (
                    "PATH_CONFINEMENT",
                    format!("cannot safely access report `{report_rel}`: {err}"),
                )
            }
        })?;

    let canonical_reports = fs::canonicalize(&reports_dir).map_err(|err| {
        (
            "REPORT_NOT_FOUND",
            format!("reports directory not accessible: {err}"),
        )
    })?;
    let canonical_target = fs::canonicalize(&candidate).map_err(|err| {
        (
            "PATH_CONFINEMENT",
            format!("cannot resolve report `{report_rel}`: {err}"),
        )
    })?;
    if !path_inside(&canonical_reports, &canonical_target) {
        return Err((
            "PATH_CONFINEMENT",
            format!("report path `{report_rel}` escapes store reports directory"),
        ));
    }

    let bytes = read_bytes_nofollow_bounded(&candidate, MAX_REPORT_BYTES, true).map_err(|err| {
        let code = match err.kind() {
            ErrorKind::NotFound => "REPORT_NOT_FOUND",
            ErrorKind::PermissionDenied => "PATH_CONFINEMENT",
            _ => "REPORT_READ_ERROR",
        };
        (code, format!("cannot read report `{report_rel}`: {err}"))
    })?;

    let text = String::from_utf8(bytes).map_err(|err| {
        (
            "INVALID_REPORT",
            format!("report `{report_rel}` is not valid UTF-8: {err}"),
        )
    })?;

    Ok((candidate, text))
}

/// Strictly validate a candidate flow path supplied to `deja_remember`:
/// - workspace-relative only (no absolute paths, drive prefixes, backslashes, or `..`)
/// - `.yaml` or `.yml` extension
/// - outside `.deja/`
/// - no symlink ancestors, `!is_symlink()`, regular file, `nlink == 1`, `<= MAX_FILE_BYTES`.
pub fn validate_remember_flow_path(
    canonical_workspace: &Path,
    flow_rel: &str,
) -> Result<(PathBuf, String), (&'static str, String)> {
    let segments =
        parse_safe_relative_segments(flow_rel).map_err(|msg| ("PATH_CONFINEMENT", msg))?;

    if segments.first().map(String::as_str) == Some(".deja") {
        return Err((
            "PATH_CONFINEMENT",
            format!("candidate flow `{flow_rel}` must be outside `.deja/`"),
        ));
    }

    let leaf = segments.last().map(String::as_str).unwrap_or("");
    let lower_leaf = leaf.to_ascii_lowercase();
    if !lower_leaf.ends_with(".yaml") && !lower_leaf.ends_with(".yml") {
        return Err((
            "INVALID_INPUT",
            format!("candidate flow `{flow_rel}` must be a `.yaml` or `.yml` file"),
        ));
    }

    let candidate =
        verify_no_symlink_components(canonical_workspace, &segments).map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                (
                    "CANDIDATE_NOT_FOUND",
                    format!("candidate flow does not exist: `{flow_rel}`"),
                )
            } else {
                (
                    "PATH_CONFINEMENT",
                    format!("candidate flow `{flow_rel}` failed confinement check: {err}"),
                )
            }
        })?;

    let file = open_read_nofollow_regular(&candidate, true).map_err(|err| {
        let code = if err.kind() == ErrorKind::NotFound {
            "CANDIDATE_NOT_FOUND"
        } else {
            "PATH_CONFINEMENT"
        };
        (
            code,
            format!("cannot safely open candidate flow `{flow_rel}`: {err}"),
        )
    })?;
    let meta = file.metadata().map_err(|err| {
        (
            "PATH_CONFINEMENT",
            format!("cannot stat candidate flow `{flow_rel}`: {err}"),
        )
    })?;
    if meta.len() > MAX_FILE_BYTES {
        return Err((
            "CLOSURE_ERROR",
            format!("candidate flow `{flow_rel}` exceeds maximum size of {MAX_FILE_BYTES} bytes"),
        ));
    }

    let normalized_rel = segments.join("/");
    Ok((candidate, normalized_rel))
}

/// Enforce `O_NOFOLLOW`, `!is_symlink()`, `is_file()`, `nlink == 1`, and size bounds on every
/// file referenced in a collected [`FlowClosure`].
pub fn verify_closure_files_nofollow(
    canonical_workspace: &Path,
    closure: &FlowClosure,
) -> Result<(), String> {
    for file in &closure.files {
        let segments = parse_safe_relative_segments(&file.source).map_err(|msg| {
            format!(
                "closure file `{}` failed path confinement: {msg}",
                file.source
            )
        })?;
        let path = verify_no_symlink_components(canonical_workspace, &segments).map_err(|err| {
            format!(
                "closure file `{}` failed symlink confinement: {err}",
                file.source
            )
        })?;
        let opened = open_read_nofollow_regular(&path, true).map_err(|err| {
            format!(
                "closure file `{}` failed O_NOFOLLOW/hardlink check: {err}",
                file.source
            )
        })?;
        let limit = match file.kind {
            FlowReferenceKind::Media => MAX_MEDIA_BYTES,
            FlowReferenceKind::Flow | FlowReferenceKind::Script => MAX_FILE_BYTES,
        };
        let meta = opened
            .metadata()
            .map_err(|err| format!("cannot stat closure file `{}`: {err}", file.source))?;
        if meta.len() > limit {
            return Err(format!(
                "closure file `{}` exceeds maximum size of {limit} bytes",
                file.source
            ));
        }
    }
    Ok(())
}

/// Create a brand-new regular file exclusively (`O_CREAT | O_EXCL | O_NOFOLLOW` on Unix).
fn create_new_nofollow(path: &Path) -> io::Result<File> {
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path)
}

fn temp_nonce() -> String {
    let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{}.{seq}.{nanos:x}", std::process::id())
}

fn ensure_confined_parent_dir(canonical_root: &Path, target: &Path) -> io::Result<PathBuf> {
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

/// Atomically write `bytes` to `target` strictly inside `canonical_root` with ancestor symlink
/// confinement and `O_EXCL | O_NOFOLLOW` staging.
pub fn write_atomic_confined(canonical_root: &Path, target: &Path, bytes: &[u8]) -> io::Result<()> {
    let canonical_parent = ensure_confined_parent_dir(canonical_root, target)?;
    let file_name = target.file_name().ok_or_else(|| {
        io::Error::new(
            ErrorKind::InvalidInput,
            format!("target path has no file name: {}", target.display()),
        )
    })?;
    let confined_target = canonical_parent.join(file_name);

    if let Ok(lmeta) = fs::symlink_metadata(&confined_target) {
        if lmeta.file_type().is_symlink() {
            return Err(io::Error::new(
                ErrorKind::PermissionDenied,
                format!(
                    "refusing to overwrite symbolic link: {}",
                    confined_target.display()
                ),
            ));
        }
        if !lmeta.file_type().is_file() {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!(
                    "refusing to overwrite non-regular file: {}",
                    confined_target.display()
                ),
            ));
        }
    }

    let name_str = file_name.to_string_lossy();
    let temp_path = confined_target.with_file_name(format!(".{name_str}.{}.tmp", temp_nonce()));
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

    if let Ok(lmeta) = fs::symlink_metadata(&confined_target)
        && lmeta.file_type().is_symlink()
    {
        let _ = fs::remove_file(&temp_path);
        return Err(io::Error::new(
            ErrorKind::PermissionDenied,
            format!(
                "refusing to overwrite symbolic link: {}",
                confined_target.display()
            ),
        ));
    }

    if let Err(err) = fs::rename(&temp_path, &confined_target) {
        let _ = fs::remove_file(&temp_path);
        return Err(err);
    }
    Ok(())
}
