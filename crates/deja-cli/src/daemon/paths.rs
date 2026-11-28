//! Zero-`unsafe` daemon path resolution, strict `0o700`/`0o600` permission checks, exclusive
//! `daemon.lock` management, atomic socket binding, process-identity verification, and
//! peer-credential verification.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::fs_safe::lexical_normalize;

/// Unix socket filename inside the daemon directory.
pub const DAEMON_SOCKET_NAME: &str = "deja.sock";
/// Atomic metadata file written after the daemon socket is bound and published.
pub const DAEMON_METADATA_NAME: &str = "daemon.json";
/// Exclusive advisory lock file guarding single-daemon startup and shutdown lifecycle.
pub const DAEMON_LOCK_NAME: &str = "daemon.lock";
/// Stable private temporary directory owned by the daemon (`<daemon_dir>/tmp`, `0o700`).
pub const DAEMON_TMP_DIR_NAME: &str = "tmp";
/// Hard cap on Unix domain socket path bytes (`sockaddr_un.sun_path` is 104 bytes on macOS
/// including the NUL terminator, so valid paths must be strictly `< 104` bytes).
pub const MAX_UNIX_SOCKET_PATH_BYTES: usize = 103;
/// Hard upper bound when reading `daemon.json` (`64 KiB`).
pub const MAX_DAEMON_METADATA_BYTES: u64 = 64 * 1024;

/// Errors emitted when resolving, validating, locking, or connecting to daemon paths.
#[derive(Debug, Error)]
pub enum DaemonPathError {
    #[error("{0}")]
    InvalidConfig(String),
    #[error("daemon path security check failed: {0}")]
    InsecurePath(String),
    #[error("daemon path not found: {0}")]
    NotFound(String),
    #[error("daemon peer credential mismatch: {0}")]
    PeerPidMismatch(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl DaemonPathError {
    /// Returns `true` when this error reflects a fail-closed security violation (symlink, wrong
    /// owner UID, or overly permissive mode) that must NEVER be bypassed or auto-repaired.
    #[must_use]
    pub const fn is_security_violation(&self) -> bool {
        matches!(self, Self::InsecurePath(_))
    }
}

/// Resolved filesystem paths for a Déjà daemon instance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPaths {
    pub dir: PathBuf,
    pub socket: PathBuf,
    pub metadata: PathBuf,
    pub lock: PathBuf,
    pub tmp_dir: PathBuf,
}

impl DaemonPaths {
    /// Construct and validate [`DaemonPaths`] for an explicit daemon directory.
    pub fn from_dir(dir: PathBuf) -> Result<Self, DaemonPathError> {
        if dir.as_os_str().is_empty() {
            return Err(DaemonPathError::InvalidConfig(
                "daemon directory path must not be empty".to_string(),
            ));
        }
        if !dir.is_absolute() {
            return Err(DaemonPathError::InvalidConfig(format!(
                "daemon directory path must be absolute (got {})",
                dir.display()
            )));
        }
        let dir = lexical_normalize(&dir);
        let socket = dir.join(DAEMON_SOCKET_NAME);
        validate_socket_path_len(&socket)?;
        let metadata = dir.join(DAEMON_METADATA_NAME);
        let lock = dir.join(DAEMON_LOCK_NAME);
        let tmp_dir = dir.join(DAEMON_TMP_DIR_NAME);
        Ok(Self {
            dir,
            socket,
            metadata,
            lock,
            tmp_dir,
        })
    }

    /// Resolve daemon paths from environment variables:
    /// `DEJA_DAEMON_DIR` -> `$DEJA_HOME/daemon` -> `$HOME/.deja/daemon`.
    pub fn resolve(env: &BTreeMap<String, String>) -> Result<Self, DaemonPathError> {
        let dir = resolve_daemon_dir(env)?;
        Self::from_dir(dir)
    }
}

fn resolve_env_path(raw: &str) -> Result<PathBuf, DaemonPathError> {
    let trimmed = raw.trim();
    let candidate = PathBuf::from(trimmed);
    if candidate.is_absolute() {
        Ok(lexical_normalize(&candidate))
    } else {
        let cwd = std::env::current_dir().map_err(|err| {
            DaemonPathError::InvalidConfig(format!(
                "cannot resolve relative daemon path {trimmed:?}: failed to read current working directory: {err}"
            ))
        })?;
        Ok(lexical_normalize(&cwd.join(candidate)))
    }
}

/// Resolve the daemon directory path with strict precedence:
/// `DEJA_DAEMON_DIR` -> `$DEJA_HOME/daemon` -> `$HOME/.deja/daemon`.
///
/// Relative `DEJA_DAEMON_DIR` / `DEJA_HOME` / `HOME` values are resolved against the current
/// working directory before normalization.
pub fn resolve_daemon_dir(env: &BTreeMap<String, String>) -> Result<PathBuf, DaemonPathError> {
    if let Some(explicit) = env.get("DEJA_DAEMON_DIR").filter(|v| !v.trim().is_empty()) {
        return resolve_env_path(explicit);
    }
    if let Some(deja_home) = env.get("DEJA_HOME").filter(|v| !v.trim().is_empty()) {
        return Ok(lexical_normalize(
            &resolve_env_path(deja_home)?.join("daemon"),
        ));
    }
    if let Some(home) = env
        .get("HOME")
        .or_else(|| env.get("USERPROFILE"))
        .filter(|v| !v.trim().is_empty())
    {
        return Ok(lexical_normalize(
            &resolve_env_path(home)?.join(".deja").join("daemon"),
        ));
    }
    Err(DaemonPathError::InvalidConfig(
        "unable to resolve daemon directory: set DEJA_DAEMON_DIR, DEJA_HOME, or HOME".to_string(),
    ))
}

/// Validate that a Unix domain socket path fits strictly under macOS's 104-byte `sun_path` limit.
pub fn validate_socket_path_len(socket_path: &Path) -> Result<(), DaemonPathError> {
    let byte_len = socket_path.as_os_str().as_encoded_bytes().len();
    if byte_len > MAX_UNIX_SOCKET_PATH_BYTES {
        return Err(DaemonPathError::InvalidConfig(format!(
            "daemon Unix socket path is {byte_len} bytes (must be < 104 bytes for macOS sun_path): {}; set DEJA_DAEMON_DIR to a shorter directory",
            socket_path.display()
        )));
    }
    Ok(())
}

/// Metadata persisted atomically to `daemon.json` once the daemon socket is listening.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DaemonMetadata {
    pub version: String,
    pub pid: u32,
    pub uid: u32,
    pub socket_path: String,
    pub started_at: String,
    pub idle_timeout_secs: u64,
    /// OS process start timestamp (`ps -o lstart= -p <pid>` in `LC_ALL=C TZ=UTC0`) used to verify
    /// process identity against PID reuse before any fallback `SIGTERM`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proc_start_time: Option<String>,
}

/// Pure validator verifying a candidate process's UID, command-line args, and OS `lstart`
/// timestamp before sending a fallback `SIGTERM`.
pub fn verify_ps_identity_fields(
    uid_args_line: &str,
    lstart_line: &str,
    expected_uid: u32,
    expected_start_time: Option<&str>,
) -> Result<(), String> {
    let expected_start = expected_start_time
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "daemon metadata is missing `procStartTime`; refusing unverified fallback SIGTERM"
                .to_string()
        })?;

    let actual_start = lstart_line.trim();
    if actual_start.is_empty() {
        return Err("process OS start time is empty or process exited".to_string());
    }
    if actual_start != expected_start {
        return Err(format!(
            "process start time mismatch (expected {expected_start:?}, got {actual_start:?}); PID may have been reused"
        ));
    }

    let trimmed = uid_args_line.trim();
    let mut parts = trimmed.split_whitespace();
    let Some(uid_tok) = parts.next() else {
        return Err("empty process UID/args output from ps".to_string());
    };
    let actual_uid: u32 = uid_tok
        .parse()
        .map_err(|err| format!("invalid UID token {uid_tok:?} from ps: {err}"))?;
    if actual_uid != expected_uid {
        return Err(format!(
            "process UID mismatch (expected {expected_uid}, got {actual_uid})"
        ));
    }

    let args_tokens: Vec<&str> = parts.collect();
    let has_daemon = args_tokens.contains(&"daemon");
    let has_foreground = args_tokens.contains(&"--foreground");
    if !has_daemon || !has_foreground {
        return Err(format!(
            "process command line does not look like a Déjà foreground daemon: {:?}",
            args_tokens.join(" ")
        ));
    }

    Ok(())
}

#[cfg(unix)]
mod unix_impl {
    use std::fs::{self, DirBuilder, File, OpenOptions, Permissions, TryLockError};
    use std::io::{self, ErrorKind, Read};
    use std::os::unix::fs::{
        DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt,
    };
    use std::path::{Path, PathBuf};
    use std::process::{Command, Output, Stdio};
    use std::sync::OnceLock;

    use tokio::net::{UnixListener, UnixStream};

    use super::{
        DAEMON_METADATA_NAME, DAEMON_SOCKET_NAME, DaemonMetadata, DaemonPathError, DaemonPaths,
        MAX_DAEMON_METADATA_BYTES, validate_socket_path_len, verify_ps_identity_fields,
    };
    use crate::fs_safe::{open_read_nofollow, write_atomic_nofollow_excl_with_mode};

    static CURRENT_UID: OnceLock<u32> = OnceLock::new();

    /// Determine the current process effective UID with zero `unsafe` code:
    /// uses `tokio::net::UnixStream::pair()?.peer_cred()?.uid()` when a Tokio runtime is active,
    /// with `/usr/bin/id -u` fallback outside a Tokio runtime, cached in a [`OnceLock`].
    pub fn current_uid() -> io::Result<u32> {
        if let Some(&uid) = CURRENT_UID.get() {
            return Ok(uid);
        }
        let uid = resolve_current_uid_uncached()?;
        let _ = CURRENT_UID.set(uid);
        Ok(uid)
    }

    fn resolve_current_uid_uncached() -> io::Result<u32> {
        if tokio::runtime::Handle::try_current().is_ok()
            && let Ok((a, _b)) = UnixStream::pair()
            && let Ok(cred) = a.peer_cred()
        {
            return Ok(cred.uid());
        }
        let out = Command::new("/usr/bin/id")
            .arg("-u")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()?;
        if !out.status.success() {
            return Err(io::Error::other(format!(
                "`/usr/bin/id -u` exited with status {:?}",
                out.status.code()
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        text.trim().parse::<u32>().map_err(|err| {
            io::Error::new(
                ErrorKind::InvalidData,
                format!("failed to parse `/usr/bin/id -u` output {text:?}: {err}"),
            )
        })
    }

    fn run_ps(args: &[&str]) -> Option<Output> {
        for bin in ["/bin/ps", "/usr/bin/ps", "ps"] {
            let res = Command::new(bin)
                .args(args)
                .env("LC_ALL", "C")
                .env("TZ", "UTC0")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output();
            match res {
                Ok(out) => return Some(out),
                Err(err) if err.kind() == ErrorKind::NotFound => continue,
                Err(_) => return None,
            }
        }
        None
    }

    /// Query the OS process start timestamp (`ps -o lstart= -p <pid>` in `LC_ALL=C TZ=UTC0`)
    /// with zero `unsafe`.
    #[must_use]
    pub fn query_proc_start_time(pid: u32) -> Option<String> {
        if pid == 0 {
            return None;
        }
        let pid_str = pid.to_string();
        let out = run_ps(&["-o", "lstart=", "-p", &pid_str])?;
        if !out.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!text.is_empty()).then_some(text)
    }

    /// Query the current process's OS start timestamp for embedding in `daemon.json`.
    #[must_use]
    pub fn current_proc_start_time() -> Option<String> {
        query_proc_start_time(std::process::id())
    }

    /// Re-verify via `ps` that `pid` belongs to `expected_uid`, has exact matching `lstart`, and
    /// is running a Déjà foreground daemon before sending any fallback `SIGTERM`.
    pub fn verify_daemon_process_identity(
        pid: u32,
        expected_uid: u32,
        expected_start_time: Option<&str>,
    ) -> Result<(), String> {
        if pid == 0 {
            return Err("refusing to signal pid 0".to_string());
        }
        let pid_str = pid.to_string();
        let lstart_out = run_ps(&["-o", "lstart=", "-p", &pid_str]).ok_or_else(|| {
            "failed to execute `ps` to verify daemon process start time".to_string()
        })?;
        if !lstart_out.status.success() {
            return Err(format!(
                "process {pid} is not running or cannot be inspected"
            ));
        }
        let lstart_line = String::from_utf8_lossy(&lstart_out.stdout);

        let uid_args_out = run_ps(&["-o", "uid=,args=", "-p", &pid_str]).ok_or_else(|| {
            "failed to execute `ps` to verify daemon process UID and command".to_string()
        })?;
        if !uid_args_out.status.success() {
            return Err(format!("process {pid} exited during identity check"));
        }
        let uid_args_line = String::from_utf8_lossy(&uid_args_out.stdout);

        verify_ps_identity_fields(
            &uid_args_line,
            &lstart_line,
            expected_uid,
            expected_start_time,
        )
    }

    /// Validate that an existing daemon directory is a non-symlink directory owned by the current
    /// user with exact mode `0o700`. Never modifies permissions on an existing directory.
    pub fn validate_daemon_dir_secure(dir: &Path) -> Result<u32, DaemonPathError> {
        let my_uid = current_uid()?;
        let lmeta = match fs::symlink_metadata(dir) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Err(DaemonPathError::NotFound(dir.display().to_string()));
            }
            Err(err) => return Err(DaemonPathError::Io(err)),
        };
        if lmeta.file_type().is_symlink() {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon directory must not be a symbolic link: {}",
                dir.display()
            )));
        }
        if !lmeta.file_type().is_dir() {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon path is not a directory: {}",
                dir.display()
            )));
        }
        if lmeta.uid() != my_uid {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon directory {} is owned by uid {} (expected {my_uid})",
                dir.display(),
                lmeta.uid()
            )));
        }
        let mode = lmeta.mode() & 0o777;
        if mode != 0o700 {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon directory {} has mode {mode:04o} (expected 0700)",
                dir.display()
            )));
        }
        Ok(my_uid)
    }

    /// Ensure the daemon directory exists with mode `0o700` and passes strict ownership/symlink
    /// validation. Existing directories with insecure permissions or symlinks fail closed.
    pub fn ensure_daemon_dir_secure(dir: &Path) -> Result<u32, DaemonPathError> {
        match fs::symlink_metadata(dir) {
            Ok(_) => validate_daemon_dir_secure(dir),
            Err(err) if err.kind() == ErrorKind::NotFound => {
                if let Some(parent) = dir.parent()
                    && !parent.as_os_str().is_empty()
                    && !parent.exists()
                {
                    fs::create_dir_all(parent)?;
                }
                let mut builder = DirBuilder::new();
                builder.mode(0o700);
                match builder.create(dir) {
                    Ok(()) => {
                        fs::set_permissions(dir, Permissions::from_mode(0o700))?;
                    }
                    Err(create_err) if create_err.kind() == ErrorKind::AlreadyExists => {}
                    Err(create_err) => return Err(DaemonPathError::Io(create_err)),
                }
                validate_daemon_dir_secure(dir)
            }
            Err(err) => Err(DaemonPathError::Io(err)),
        }
    }

    /// Ensure `<daemon_dir>/tmp` exists with strict `0o700` permissions and current-user ownership
    /// so the persistent daemon never relies on a caller's ephemeral `TMPDIR`.
    pub fn ensure_daemon_tmp_dir_secure(paths: &DaemonPaths) -> Result<PathBuf, DaemonPathError> {
        ensure_daemon_dir_secure(&paths.dir)?;
        ensure_daemon_dir_secure(&paths.tmp_dir)?;
        Ok(paths.tmp_dir.clone())
    }

    /// Remove `<daemon_dir>/tmp` cleanly on daemon shutdown / idle exit if it is a real directory
    /// (never following symlinks).
    pub fn cleanup_daemon_tmp_dir(paths: &DaemonPaths) {
        if let Ok(lmeta) = fs::symlink_metadata(&paths.tmp_dir)
            && !lmeta.file_type().is_symlink()
            && lmeta.file_type().is_dir()
        {
            let _ = fs::remove_dir_all(&paths.tmp_dir);
        }
    }

    /// RAII exclusive advisory lock on `daemon.lock` (`O_NOFOLLOW`, `0o600`).
    /// Dropping [`DaemonLock`] releases the OS file lock immediately.
    #[derive(Debug)]
    pub struct DaemonLock {
        _file: File,
    }

    /// Acquire the exclusive `daemon.lock` non-blockingly (`try_lock`).
    /// Returns `Ok(None)` when another live daemon already holds the lock.
    pub fn acquire_daemon_lock(paths: &DaemonPaths) -> Result<Option<DaemonLock>, DaemonPathError> {
        let my_uid = ensure_daemon_dir_secure(&paths.dir)?;
        if let Ok(lmeta) = fs::symlink_metadata(&paths.lock) {
            validate_lock_metadata(&paths.lock, &lmeta, my_uid)?;
        }

        let mut opts = OpenOptions::new();
        opts.read(true)
            .write(true)
            .create(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW);
        let file = opts.open(&paths.lock).map_err(|err| {
            if crate::fs_safe::is_eloop(&err) {
                DaemonPathError::InsecurePath(format!(
                    "daemon lock must not be a symbolic link: {}",
                    paths.lock.display()
                ))
            } else {
                DaemonPathError::Io(err)
            }
        })?;
        let fmeta = file.metadata()?;
        validate_lock_metadata(&paths.lock, &fmeta, my_uid)?;

        match file.try_lock() {
            Ok(()) => Ok(Some(DaemonLock { _file: file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(err)) => Err(DaemonPathError::Io(err)),
        }
    }

    /// Probe whether `daemon.lock` is currently held by another process without creating files.
    pub fn probe_daemon_lock_held(paths: &DaemonPaths) -> Result<bool, DaemonPathError> {
        let my_uid = match validate_daemon_dir_secure(&paths.dir) {
            Ok(uid) => uid,
            Err(DaemonPathError::NotFound(_)) => return Ok(false),
            Err(err) => return Err(err),
        };
        let lmeta = match fs::symlink_metadata(&paths.lock) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(DaemonPathError::Io(err)),
        };
        validate_lock_metadata(&paths.lock, &lmeta, my_uid)?;

        let mut opts = OpenOptions::new();
        opts.read(true).write(true).custom_flags(libc::O_NOFOLLOW);
        let file = match opts.open(&paths.lock) {
            Ok(f) => f,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
            Err(err) if crate::fs_safe::is_eloop(&err) => {
                return Err(DaemonPathError::InsecurePath(format!(
                    "daemon lock must not be a symbolic link: {}",
                    paths.lock.display()
                )));
            }
            Err(err) => return Err(DaemonPathError::Io(err)),
        };
        let fmeta = file.metadata()?;
        validate_lock_metadata(&paths.lock, &fmeta, my_uid)?;

        match file.try_lock() {
            Ok(()) => {
                drop(file);
                Ok(false)
            }
            Err(TryLockError::WouldBlock) => Ok(true),
            Err(TryLockError::Error(err)) => Err(DaemonPathError::Io(err)),
        }
    }

    fn validate_lock_metadata(
        path: &Path,
        meta: &fs::Metadata,
        my_uid: u32,
    ) -> Result<(), DaemonPathError> {
        if meta.file_type().is_symlink() {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon lock must not be a symbolic link: {}",
                path.display()
            )));
        }
        if !meta.file_type().is_file() {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon lock is not a regular file: {}",
                path.display()
            )));
        }
        if meta.uid() != my_uid {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon lock {} is owned by uid {} (expected {my_uid})",
                path.display(),
                meta.uid()
            )));
        }
        let mode = meta.mode() & 0o777;
        if mode != 0o600 {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon lock {} has mode {mode:04o} (expected 0600)",
                path.display()
            )));
        }
        Ok(())
    }

    struct TempSocketGuard {
        path: Option<PathBuf>,
    }

    impl TempSocketGuard {
        fn new(path: PathBuf) -> Self {
            Self { path: Some(path) }
        }

        fn disarm(&mut self) {
            self.path = None;
        }
    }

    impl Drop for TempSocketGuard {
        fn drop(&mut self) {
            if let Some(p) = self.path.take() {
                let _ = fs::remove_file(p);
            }
        }
    }

    /// Atomically bind `deja.<pid>.sock.tmp`, `chmod 0o600`, and `rename` to `deja.sock`.
    /// Caller MUST hold [`DaemonLock`] and have validated `daemon_dir`.
    pub fn bind_daemon_socket(paths: &DaemonPaths) -> Result<UnixListener, DaemonPathError> {
        validate_daemon_dir_secure(&paths.dir)?;
        let tmp_sock = paths
            .dir
            .join(format!("deja.{}.sock.tmp", std::process::id()));
        validate_socket_path_len(&tmp_sock)?;

        if let Ok(lmeta) = fs::symlink_metadata(&tmp_sock) {
            if lmeta.file_type().is_symlink() {
                return Err(DaemonPathError::InsecurePath(format!(
                    "temporary daemon socket path must not be a symbolic link: {}",
                    tmp_sock.display()
                )));
            }
            fs::remove_file(&tmp_sock)?;
        }

        if let Ok(lmeta) = fs::symlink_metadata(&paths.socket) {
            if lmeta.file_type().is_symlink() {
                return Err(DaemonPathError::InsecurePath(format!(
                    "daemon socket path must not be a symbolic link: {}",
                    paths.socket.display()
                )));
            }
            if !lmeta.file_type().is_socket() {
                return Err(DaemonPathError::InsecurePath(format!(
                    "existing daemon socket path is not a Unix socket: {}",
                    paths.socket.display()
                )));
            }
        }

        let listener = UnixListener::bind(&tmp_sock)?;
        let mut guard = TempSocketGuard::new(tmp_sock.clone());
        fs::set_permissions(&tmp_sock, Permissions::from_mode(0o600))?;
        fs::rename(&tmp_sock, &paths.socket)?;
        guard.disarm();
        Ok(listener)
    }

    /// Atomically write `daemon.json` with `0o600` permissions using `O_EXCL | O_NOFOLLOW`.
    pub fn write_daemon_metadata(
        paths: &DaemonPaths,
        meta: &DaemonMetadata,
    ) -> Result<(), DaemonPathError> {
        let mut bytes = serde_json::to_vec_pretty(meta).map_err(|err| {
            DaemonPathError::Io(io::Error::new(ErrorKind::InvalidData, err.to_string()))
        })?;
        bytes.push(b'\n');
        write_atomic_nofollow_excl_with_mode(&paths.metadata, &bytes, Some(0o600)).map_err(|err| {
            if err.kind() == ErrorKind::InvalidInput || crate::fs_safe::is_eloop(&err) {
                DaemonPathError::InsecurePath(err.to_string())
            } else {
                DaemonPathError::Io(err)
            }
        })
    }

    /// Read and validate `daemon.json` (`open_read_nofollow`, bounded `64 KiB`, own UID).
    pub fn read_daemon_metadata(paths: &DaemonPaths) -> Result<DaemonMetadata, DaemonPathError> {
        let my_uid = current_uid()?;
        let lmeta = match fs::symlink_metadata(&paths.metadata) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Err(DaemonPathError::NotFound(
                    paths.metadata.display().to_string(),
                ));
            }
            Err(err) => return Err(DaemonPathError::Io(err)),
        };
        if lmeta.file_type().is_symlink() || !lmeta.file_type().is_file() {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon metadata must be a regular non-symlink file: {}",
                paths.metadata.display()
            )));
        }
        let file = open_read_nofollow(&paths.metadata).map_err(|err| {
            if err.kind() == ErrorKind::NotFound {
                DaemonPathError::NotFound(paths.metadata.display().to_string())
            } else if err.kind() == ErrorKind::InvalidInput || crate::fs_safe::is_eloop(&err) {
                DaemonPathError::InsecurePath(err.to_string())
            } else {
                DaemonPathError::Io(err)
            }
        })?;
        let fmeta = file.metadata()?;
        if fmeta.uid() != my_uid {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon metadata {} is owned by uid {} (expected {my_uid})",
                paths.metadata.display(),
                fmeta.uid()
            )));
        }
        if fmeta.len() > MAX_DAEMON_METADATA_BYTES {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon metadata {} exceeds {MAX_DAEMON_METADATA_BYTES} bytes",
                paths.metadata.display()
            )));
        }
        let mut buf = Vec::with_capacity(
            usize::try_from(fmeta.len().min(MAX_DAEMON_METADATA_BYTES)).unwrap_or(0),
        );
        file.take(MAX_DAEMON_METADATA_BYTES.saturating_add(1))
            .read_to_end(&mut buf)?;
        if (buf.len() as u64) > MAX_DAEMON_METADATA_BYTES {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon metadata {} exceeds {MAX_DAEMON_METADATA_BYTES} bytes",
                paths.metadata.display()
            )));
        }
        let meta: DaemonMetadata = serde_json::from_slice(&buf).map_err(|err| {
            DaemonPathError::Io(io::Error::new(
                ErrorKind::InvalidData,
                format!("invalid daemon metadata JSON: {err}"),
            ))
        })?;
        if meta.uid != my_uid {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon metadata uid {} does not match current uid {my_uid}",
                meta.uid
            )));
        }
        if meta.pid == 0 {
            return Err(DaemonPathError::Io(io::Error::new(
                ErrorKind::InvalidData,
                "daemon metadata pid must be > 0",
            )));
        }
        Ok(meta)
    }

    /// Perform all pre-connect filesystem checks (`daemon_dir` `0o700` own UID non-symlink,
    /// `deja.sock` socket `0o600` own UID non-symlink, `daemon.json` own UID `<= 64 KiB`), connect
    /// to the Unix socket, and verify `stream.peer_cred()?.uid() == my_uid` and PID match.
    pub async fn connect_verified(
        paths: &DaemonPaths,
    ) -> Result<(UnixStream, DaemonMetadata), DaemonPathError> {
        let my_uid = validate_daemon_dir_secure(&paths.dir)?;

        let smeta = match fs::symlink_metadata(&paths.socket) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Err(DaemonPathError::NotFound(
                    paths.socket.display().to_string(),
                ));
            }
            Err(err) => return Err(DaemonPathError::Io(err)),
        };
        if smeta.file_type().is_symlink() {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon socket must not be a symbolic link: {}",
                paths.socket.display()
            )));
        }
        if !smeta.file_type().is_socket() {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon socket path is not a Unix domain socket: {}",
                paths.socket.display()
            )));
        }
        if smeta.uid() != my_uid {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon socket {} is owned by uid {} (expected {my_uid})",
                paths.socket.display(),
                smeta.uid()
            )));
        }
        let smode = smeta.mode() & 0o777;
        if smode != 0o600 {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon socket {} has mode {smode:04o} (expected 0600)",
                paths.socket.display()
            )));
        }

        let meta = read_daemon_metadata(paths)?;
        let stream = UnixStream::connect(&paths.socket).await?;
        let cred = stream.peer_cred()?;
        if cred.uid() != my_uid {
            return Err(DaemonPathError::InsecurePath(format!(
                "daemon socket peer uid {} does not match current uid {my_uid}",
                cred.uid()
            )));
        }
        if let Some(peer_pid) = cred.pid()
            && (peer_pid <= 0 || (peer_pid as u32) != meta.pid)
        {
            return Err(DaemonPathError::PeerPidMismatch(format!(
                "daemon socket peer pid {peer_pid} does not match daemon.json pid {}",
                meta.pid
            )));
        }
        Ok((stream, meta))
    }

    /// Safely remove a file or socket if it exists and is NOT a symlink.
    pub fn remove_if_not_symlink(path: &Path) -> Result<bool, DaemonPathError> {
        let lmeta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(false),
            Err(err) => return Err(DaemonPathError::Io(err)),
        };
        if lmeta.file_type().is_symlink() {
            return Err(DaemonPathError::InsecurePath(format!(
                "refusing to unlink symbolic link: {}",
                path.display()
            )));
        }
        match fs::remove_file(path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == ErrorKind::NotFound => Ok(false),
            Err(err) => Err(DaemonPathError::Io(err)),
        }
    }

    /// Remove stale `deja.sock` and `daemon.json` files while holding [`DaemonLock`], returning
    /// the names of removed files. Refuses to touch symlinks or insecure directories.
    pub fn cleanup_stale_files(
        paths: &DaemonPaths,
        _lock: &DaemonLock,
    ) -> Result<Vec<String>, DaemonPathError> {
        validate_daemon_dir_secure(&paths.dir)?;
        let mut cleaned = Vec::new();
        if remove_if_not_symlink(&paths.socket)? {
            cleaned.push(DAEMON_SOCKET_NAME.to_string());
        }
        if remove_if_not_symlink(&paths.metadata)? {
            cleaned.push(DAEMON_METADATA_NAME.to_string());
        }
        Ok(cleaned)
    }
}

#[cfg(unix)]
pub use unix_impl::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_daemon_dir_precedence() {
        let mut env = BTreeMap::new();
        env.insert("HOME".to_string(), "/tmp/test-home".to_string());
        assert_eq!(
            resolve_daemon_dir(&env).unwrap(),
            PathBuf::from("/tmp/test-home/.deja/daemon")
        );

        env.insert("DEJA_HOME".to_string(), "/tmp/test-deja-home".to_string());
        assert_eq!(
            resolve_daemon_dir(&env).unwrap(),
            PathBuf::from("/tmp/test-deja-home/daemon")
        );

        env.insert("DEJA_DAEMON_DIR".to_string(), "/tmp/custom-d".to_string());
        assert_eq!(
            resolve_daemon_dir(&env).unwrap(),
            PathBuf::from("/tmp/custom-d")
        );
    }

    #[test]
    fn resolve_daemon_dir_supports_relative_deja_home_and_daemon_dir() {
        let cwd = std::env::current_dir().unwrap();

        let env_home = BTreeMap::from([("DEJA_HOME".to_string(), ".tmp-deja-home".to_string())]);
        assert_eq!(
            resolve_daemon_dir(&env_home).unwrap(),
            lexical_normalize(&cwd.join(".tmp-deja-home").join("daemon"))
        );

        let env_dir = BTreeMap::from([("DEJA_DAEMON_DIR".to_string(), ".tmp-deja-d".to_string())]);
        assert_eq!(
            resolve_daemon_dir(&env_dir).unwrap(),
            lexical_normalize(&cwd.join(".tmp-deja-d"))
        );
    }

    #[test]
    fn verify_ps_identity_fields_enforces_uid_lstart_and_daemon_args() {
        let lstart = "Sat Nov 28 11:03:31 2026";
        let valid_line = "  501 /usr/local/bin/deja daemon start --foreground --idle-timeout 300";

        assert!(verify_ps_identity_fields(valid_line, lstart, 501, Some(lstart)).is_ok());

        // Missing procStartTime -> refused.
        assert!(verify_ps_identity_fields(valid_line, lstart, 501, None).is_err());
        assert!(verify_ps_identity_fields(valid_line, lstart, 501, Some("   ")).is_err());

        // Mismatched lstart (PID reuse) -> refused.
        assert!(
            verify_ps_identity_fields(valid_line, "Sat Nov 28 11:05:00 2026", 501, Some(lstart),)
                .is_err()
        );

        // Mismatched UID -> refused.
        assert!(verify_ps_identity_fields(valid_line, lstart, 502, Some(lstart)).is_err());

        // Unrelated command line -> refused.
        assert!(
            verify_ps_identity_fields("501 /bin/sleep 999", lstart, 501, Some(lstart)).is_err()
        );
        assert!(
            verify_ps_identity_fields(
                "501 /usr/local/bin/deja run checkout",
                lstart,
                501,
                Some(lstart)
            )
            .is_err()
        );
    }

    #[test]
    fn socket_path_length_limit_enforced() {
        let ok_dir = PathBuf::from("/tmp/d");
        assert!(DaemonPaths::from_dir(ok_dir).is_ok());

        let too_long = PathBuf::from(format!("/tmp/{}", "a".repeat(100)));
        let err = DaemonPaths::from_dir(too_long).expect_err("must reject >= 104-byte socket path");
        assert!(err.to_string().contains("< 104"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn current_uid_succeeds_without_unsafe() {
        let uid = current_uid().expect("current_uid inside tokio runtime");
        assert_eq!(uid, current_uid().expect("cached uid"));
    }
}
