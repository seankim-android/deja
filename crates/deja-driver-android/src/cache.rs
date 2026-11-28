//! Prebuilt APK resolution (`DEJA_ANDROID_AGENT_APK` + optional `DEJA_ANDROID_AGENT_APK_SHA256`)
//! and on-demand zero-Gradle cached compilation under `$DEJA_HOME/drivers/android/<source_sha256>/`.

use std::collections::BTreeMap;
use std::fs::{self, File, TryLockError};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

use crate::embed;
use crate::error::{AndroidDriverError, Result};
use crate::process::{
    configure_process_group, open_lockfile_nofollow, open_read_nofollow, terminate_child_group_sync,
};

const ZIP_MAGIC: &[u8; 4] = b"PK\x03\x04";
const APK_FILENAME: &str = "deja-android-agent.apk";
const APK_SHA256_FILENAME: &str = "deja-android-agent.apk.sha256";
const READY_MARKER_FILENAME: &str = "ready.marker";
const DEBUG_KEYSTORE_FILENAME: &str = "debug.keystore";
const HASH_CHUNK_BYTES: usize = 65_536;

static STAGING_NONCE: AtomicU64 = AtomicU64::new(0);

/// Origin of the resolved `deja-android-agent.apk`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApkSource {
    /// Explicit prebuilt APK supplied via `DEJA_ANDROID_AGENT_APK`.
    PrebuiltEnv,
    /// Embedded prebuilt APK shipped inside `deja` (zero JDK / zero Android SDK required).
    EmbeddedPrebuilt,
    /// On-demand compiled and cached APK under `$DEJA_HOME/drivers/android/<source_sha256>/`.
    CachedBuild,
}

/// Resolved, SHA-256 verified `deja-android-agent.apk` ready for installation.
#[derive(Debug, Clone)]
pub struct ResolvedAgentApk {
    pub apk_path: PathBuf,
    pub sha256: String,
    pub source_sha256: String,
    pub source: ApkSource,
    pub was_cache_hit: bool,
    pub build_duration_ms: u64,
}

/// Discovered local Android SDK build tools + JDK executables required for zero-Gradle APK builds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AndroidSdkBuildTools {
    pub sdk_root: PathBuf,
    pub build_tools_dir: PathBuf,
    pub android_jar: PathBuf,
    pub javac: PathBuf,
    pub keytool: PathBuf,
}

/// Read-only agent APK readiness report used by `deja doctor` (never mutates disk or compiles).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentApkReadiness {
    PrebuiltReady {
        apk_path: PathBuf,
        sha256: String,
    },
    PrebuiltInvalid {
        detail: String,
    },
    EmbeddedPrebuiltReady {
        sha256: String,
    },
    CacheHit {
        apk_path: PathBuf,
        source_sha256: String,
    },
    CacheColdSdkReady {
        source_sha256: String,
        build_tools_version: String,
    },
    Unavailable {
        detail: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadyMarker {
    source_sha256: String,
    apk_sha256: String,
    build_tools: String,
    platform: String,
}

/// RAII cancellation guard for a staged APK compilation command and its temp directory.
struct BuildGuard {
    staging_dir: PathBuf,
    child: Option<Child>,
    pgid: Option<u32>,
    disarmed: bool,
}

impl BuildGuard {
    fn new(staging_dir: PathBuf) -> Self {
        Self {
            staging_dir,
            child: None,
            pgid: None,
            disarmed: false,
        }
    }

    async fn run_command(
        &mut self,
        mut cmd: Command,
        program_label: &str,
    ) -> Result<(ExitStatus, Vec<u8>, Vec<u8>)> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        configure_process_group(&mut cmd);

        let mut child = cmd
            .spawn()
            .map_err(|source| AndroidDriverError::CommandSpawn {
                program: program_label.to_string(),
                source,
            })?;
        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        self.pgid = child.id();
        self.child = Some(child);

        let read_stdout = async move {
            let mut buf = Vec::new();
            if let Some(mut out) = stdout_pipe {
                let _ = out.read_to_end(&mut buf).await;
            }
            buf
        };
        let read_stderr = async move {
            let mut buf = Vec::new();
            if let Some(mut err) = stderr_pipe {
                let _ = err.read_to_end(&mut buf).await;
            }
            buf
        };
        let wait_child = async {
            self.child
                .as_mut()
                .expect("child attached to BuildGuard")
                .wait()
                .await
        };

        let (status_res, stdout_bytes, stderr_bytes) =
            tokio::join!(wait_child, read_stdout, read_stderr);
        self.child = None;
        self.pgid = None;

        let status = status_res.map_err(|source| AndroidDriverError::CommandSpawn {
            program: program_label.to_string(),
            source,
        })?;
        Ok((status, stdout_bytes, stderr_bytes))
    }

    fn disarm(&mut self) {
        self.disarmed = true;
    }
}

impl Drop for BuildGuard {
    fn drop(&mut self) {
        if self.disarmed {
            return;
        }
        self.disarmed = true;
        if let Some(mut child) = self.child.take() {
            terminate_child_group_sync(&mut child, self.pgid);
        }
        if self.staging_dir.exists() {
            let _ = fs::remove_dir_all(&self.staging_dir);
        }
    }
}

#[derive(Debug)]
struct AdvisoryFileLock(File);

impl Drop for AdvisoryFileLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

async fn acquire_advisory_lock_cancellable(
    lock_path: &Path,
    cancel: &CancellationToken,
) -> Result<AdvisoryFileLock> {
    let file = open_lockfile_nofollow(lock_path).map_err(|source| AndroidDriverError::Io {
        context: format!("open Android build cache lock {}", lock_path.display()),
        source,
    })?;
    loop {
        if cancel.is_cancelled() {
            return Err(AndroidDriverError::Cancelled);
        }
        match file.try_lock() {
            Ok(()) => return Ok(AdvisoryFileLock(file)),
            Err(TryLockError::WouldBlock) => {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(AndroidDriverError::Cancelled),
                    _ = tokio::time::sleep(Duration::from_millis(25)) => {}
                }
            }
            Err(TryLockError::Error(source)) => {
                return Err(AndroidDriverError::Io {
                    context: format!("acquire Android build cache lock {}", lock_path.display()),
                    source,
                });
            }
        }
    }
}

/// Resolves `$DEJA_HOME`: explicit override -> `env["DEJA_HOME"]` -> process `$DEJA_HOME` -> `~/.deja`.
pub fn resolve_deja_home(
    override_home: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> Result<PathBuf> {
    let raw = if let Some(dir) = override_home {
        dir.to_path_buf()
    } else if let Some(env_home) = env.get("DEJA_HOME").filter(|v| !v.trim().is_empty()) {
        PathBuf::from(env_home.trim())
    } else if let Some(proc_home) = std::env::var_os("DEJA_HOME").filter(|v| !v.is_empty()) {
        PathBuf::from(proc_home)
    } else {
        let home = env
            .get("HOME")
            .filter(|v| !v.trim().is_empty())
            .map(|v| PathBuf::from(v.trim()))
            .or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            })
            .ok_or_else(|| {
                AndroidDriverError::InvalidPath(
                    "neither DEJA_HOME nor HOME is set in the environment".to_string(),
                )
            })?;
        home.join(".deja")
    };

    if raw.is_absolute() {
        Ok(raw)
    } else {
        let cwd = std::env::current_dir().map_err(|source| AndroidDriverError::Io {
            context: "resolve current working directory for relative DEJA_HOME".to_string(),
            source,
        })?;
        Ok(cwd.join(raw))
    }
}

/// Validates and computes the streaming SHA-256 of a regular, non-symlink APK file (`O_NOFOLLOW`),
/// confirming it starts with ZIP magic bytes (`PK\x03\x04`).
pub fn verify_and_hash_apk(apk_path: &Path) -> Result<String> {
    let mut file = open_read_nofollow(apk_path).map_err(|err| {
        AndroidDriverError::InvalidPath(format!(
            "Android APK must be an existing regular non-symlink file ({}): {err}",
            apk_path.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; HASH_CHUNK_BYTES];
    let mut first_four = Vec::with_capacity(4);
    let mut total = 0usize;

    loop {
        let read = file
            .read(&mut buf)
            .map_err(|source| AndroidDriverError::Io {
                context: format!("read Android APK {}", apk_path.display()),
                source,
            })?;
        if read == 0 {
            break;
        }
        if first_four.len() < 4 {
            let need = (4 - first_four.len()).min(read);
            first_four.extend_from_slice(&buf[..need]);
        }
        hasher.update(&buf[..read]);
        total = total.saturating_add(read);
    }

    if total < 4 || first_four.as_slice() != ZIP_MAGIC {
        return Err(AndroidDriverError::InvalidPath(format!(
            "file {} is not a valid APK/ZIP archive (missing PK\\x03\\x04 header)",
            apk_path.display()
        )));
    }
    Ok(hex::encode(hasher.finalize()))
}

fn validate_sha256_pin(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.len() != 64 || !trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AndroidDriverError::InvalidArgument(format!(
            "DEJA_ANDROID_AGENT_APK_SHA256 must be a 64-character hex SHA-256 digest (got {raw:?})"
        )));
    }
    Ok(trimmed.to_ascii_lowercase())
}

fn resolve_prebuilt_apk_from_env(
    env: &BTreeMap<String, String>,
) -> Result<Option<ResolvedAgentApk>> {
    let Some(raw_apk) = env
        .get("DEJA_ANDROID_AGENT_APK")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    else {
        return Ok(None);
    };

    let raw_path = PathBuf::from(raw_apk);
    let apk_path = if raw_path.is_absolute() {
        raw_path
    } else {
        std::env::current_dir()
            .map_err(|source| AndroidDriverError::Io {
                context: "resolve cwd for DEJA_ANDROID_AGENT_APK".to_string(),
                source,
            })?
            .join(raw_path)
    };

    let actual_sha = verify_and_hash_apk(&apk_path)?;
    if let Some(raw_pin) = env
        .get("DEJA_ANDROID_AGENT_APK_SHA256")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let expected = validate_sha256_pin(raw_pin)?;
        if actual_sha != expected {
            return Err(AndroidDriverError::ApkHashMismatch {
                path: apk_path,
                expected,
                actual: actual_sha,
            });
        }
    }

    Ok(Some(ResolvedAgentApk {
        apk_path,
        sha256: actual_sha,
        source_sha256: embed::source_sha256(),
        source: ApkSource::PrebuiltEnv,
        was_cache_hit: true,
        build_duration_ms: 0,
    }))
}

/// Discovers Android SDK build tools (`aapt2`, `d8`, `zipalign`, `apksigner`), `android.jar`,
/// and JDK tools (`javac`, `keytool`) from `env`.
pub fn discover_sdk_build_tools(env: &BTreeMap<String, String>) -> Result<AndroidSdkBuildTools> {
    let mut sdk_candidates: Vec<PathBuf> = Vec::new();
    for var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(val) = env.get(var).map(|s| s.trim()).filter(|s| !s.is_empty()) {
            sdk_candidates.push(PathBuf::from(val));
        }
    }
    if let Some(home) = env.get("HOME").map(|s| s.trim()).filter(|s| !s.is_empty()) {
        sdk_candidates.push(PathBuf::from(home).join("Library/Android/sdk"));
        sdk_candidates.push(PathBuf::from(home).join("Android/Sdk"));
    }

    let sdk_root = sdk_candidates
        .into_iter()
        .find(|p| p.is_dir())
        .ok_or_else(|| {
            AndroidDriverError::ApkUnavailable(
                "Android SDK root not found (set ANDROID_HOME or ANDROID_SDK_ROOT, or set DEJA_ANDROID_AGENT_APK to a prebuilt agent APK)".to_string(),
            )
        })?;

    let build_tools_dir = find_latest_build_tools_dir(&sdk_root.join("build-tools")).ok_or_else(
        || {
            AndroidDriverError::ApkUnavailable(format!(
                "no complete Android build-tools (aapt2, d8, zipalign, apksigner) found under {}; install Android SDK Build-Tools or set DEJA_ANDROID_AGENT_APK",
                sdk_root.join("build-tools").display()
            ))
        },
    )?;

    let android_jar = find_latest_android_jar(&sdk_root.join("platforms")).ok_or_else(|| {
        AndroidDriverError::ApkUnavailable(format!(
            "no Android SDK platform android.jar found under {}; install an Android SDK Platform (API 24+) or set DEJA_ANDROID_AGENT_APK",
            sdk_root.join("platforms").display()
        ))
    })?;

    let javac = find_jdk_tool("javac", env).ok_or_else(|| {
        AndroidDriverError::ApkUnavailable(
            "`javac` not found on PATH or JAVA_HOME; install a JDK (11+) to build the Déjà Android agent APK once, or set DEJA_ANDROID_AGENT_APK".to_string(),
        )
    })?;
    let keytool = find_jdk_tool("keytool", env).ok_or_else(|| {
        AndroidDriverError::ApkUnavailable(
            "`keytool` not found on PATH or JAVA_HOME; install a JDK (11+) to sign the Déjà Android agent APK once, or set DEJA_ANDROID_AGENT_APK".to_string(),
        )
    })?;

    Ok(AndroidSdkBuildTools {
        sdk_root,
        build_tools_dir,
        android_jar,
        javac,
        keytool,
    })
}

fn find_latest_build_tools_dir(base: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(base).ok()?;
    let mut dirs: Vec<(Vec<u64>, String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let has_all = ["aapt2", "d8", "zipalign", "apksigner"]
            .iter()
            .all(|bin| is_executable_file(&path.join(bin)));
        if !has_all {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        dirs.push((version_sort_key(&name), name, path));
    }
    dirs.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    dirs.pop().map(|(_, _, p)| p)
}

fn find_latest_android_jar(base: &Path) -> Option<PathBuf> {
    let entries = fs::read_dir(base).ok()?;
    let mut jars: Vec<(Vec<u64>, String, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let jar = path.join("android.jar");
        if !jar.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        jars.push((version_sort_key(&name), name, jar));
    }
    jars.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    jars.pop().map(|(_, _, p)| p)
}

fn version_sort_key(name: &str) -> Vec<u64> {
    name.split(|c: char| !c.is_ascii_digit())
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<u64>().ok())
        .collect()
}

fn find_jdk_tool(name: &str, env: &BTreeMap<String, String>) -> Option<PathBuf> {
    if let Some(java_home) = env
        .get("JAVA_HOME")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        let cand = PathBuf::from(java_home).join("bin").join(name);
        if is_executable_file(&cand) {
            return Some(cand);
        }
    }
    if let Some(path_var) = env.get("PATH").filter(|v| !v.is_empty()) {
        for dir in std::env::split_paths(path_var) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let cand = dir.join(name);
            if is_executable_file(&cand) {
                return Some(cand);
            }
        }
    }
    None
}

pub(crate) fn is_executable_file(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| {
        if !meta.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o111 != 0
        }
        #[cfg(not(unix))]
        {
            true
        }
    })
}

fn wants_source_build(env: &BTreeMap<String, String>) -> bool {
    env.get("DEJA_ANDROID_BUILD_AGENT_FROM_SOURCE")
        .map(|s| s.trim())
        .is_some_and(|v| {
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
        })
}

fn check_cached_apk(
    cache_dir: &Path,
    source_sha: &str,
    source: ApkSource,
    required_apk_sha: Option<&str>,
) -> Option<ResolvedAgentApk> {
    let marker = cache_dir.join(READY_MARKER_FILENAME);
    let apk_path = cache_dir.join(APK_FILENAME);
    let sha_path = cache_dir.join(APK_SHA256_FILENAME);
    if !marker.is_file() || !sha_path.is_file() {
        return None;
    }
    let expected_sha = fs::read_to_string(&sha_path)
        .ok()?
        .trim()
        .to_ascii_lowercase();
    if expected_sha.len() != 64 || !expected_sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    if let Some(pin) = required_apk_sha
        && expected_sha != pin
    {
        return None;
    }
    let actual_sha = verify_and_hash_apk(&apk_path).ok()?;
    if actual_sha != expected_sha {
        return None;
    }
    Some(ResolvedAgentApk {
        apk_path,
        sha256: actual_sha,
        source_sha256: source_sha.to_string(),
        source,
        was_cache_hit: true,
        build_duration_ms: 0,
    })
}

/// Read-only inspection of `deja-android-agent.apk` readiness for `deja doctor`.
/// Never creates directories, locks, or spawns build scripts.
#[must_use]
pub fn inspect_agent_apk_readiness(
    deja_home_override: Option<&Path>,
    env: &BTreeMap<String, String>,
) -> AgentApkReadiness {
    if env
        .get("DEJA_ANDROID_AGENT_APK")
        .is_some_and(|v| !v.trim().is_empty())
    {
        return match resolve_prebuilt_apk_from_env(env) {
            Ok(Some(resolved)) => AgentApkReadiness::PrebuiltReady {
                apk_path: resolved.apk_path,
                sha256: resolved.sha256,
            },
            Ok(None) => AgentApkReadiness::Unavailable {
                detail: "DEJA_ANDROID_AGENT_APK is empty".to_string(),
            },
            Err(err) => AgentApkReadiness::PrebuiltInvalid {
                detail: err.to_string(),
            },
        };
    }

    if !wants_source_build(env) {
        return AgentApkReadiness::EmbeddedPrebuiltReady {
            sha256: embed::PREBUILT_AGENT_APK_SHA256.to_string(),
        };
    }

    let source_sha = embed::source_sha256();
    if let Ok(deja_home) = resolve_deja_home(deja_home_override, env) {
        let cache_dir = deja_home.join("drivers").join("android").join(&source_sha);
        if let Some(hit) = check_cached_apk(&cache_dir, &source_sha, ApkSource::CachedBuild, None) {
            return AgentApkReadiness::CacheHit {
                apk_path: hit.apk_path,
                source_sha256: source_sha,
            };
        }
    }

    match discover_sdk_build_tools(env) {
        Ok(tools) => {
            let ver = tools
                .build_tools_dir
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".to_string());
            AgentApkReadiness::CacheColdSdkReady {
                source_sha256: source_sha,
                build_tools_version: ver,
            }
        }
        Err(err) => AgentApkReadiness::Unavailable {
            detail: err.to_string(),
        },
    }
}

async fn materialize_embedded_prebuilt_apk(
    deja_home: &Path,
    source_sha: &str,
    cancel: &CancellationToken,
) -> Result<ResolvedAgentApk> {
    let actual_embedded_sha = hex::encode(Sha256::digest(embed::PREBUILT_AGENT_APK));
    if actual_embedded_sha != embed::PREBUILT_AGENT_APK_SHA256 {
        return Err(AndroidDriverError::ApkHashMismatch {
            path: PathBuf::from("<embedded:deja-android-agent.apk>"),
            expected: embed::PREBUILT_AGENT_APK_SHA256.to_string(),
            actual: actual_embedded_sha,
        });
    }

    let android_root = deja_home.join("drivers").join("android");
    let cache_dir = android_root.join(format!("prebuilt-{}", embed::PREBUILT_AGENT_APK_SHA256));
    let locks_dir = android_root.join("locks");

    if let Some(hit) = check_cached_apk(
        &cache_dir,
        source_sha,
        ApkSource::EmbeddedPrebuilt,
        Some(embed::PREBUILT_AGENT_APK_SHA256),
    ) {
        debug!(
            apk_sha = %embed::PREBUILT_AGENT_APK_SHA256,
            "embedded prebuilt deja-android-agent.apk cache hit"
        );
        return Ok(hit);
    }

    fs::create_dir_all(&locks_dir).map_err(|source| AndroidDriverError::Io {
        context: format!("create Android cache locks dir {}", locks_dir.display()),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&locks_dir, fs::Permissions::from_mode(0o700));
    }

    let lock_path = locks_dir.join(format!(
        "prebuilt-{}.lock",
        embed::PREBUILT_AGENT_APK_SHA256
    ));
    let _lock = acquire_advisory_lock_cancellable(&lock_path, cancel).await?;

    if let Some(hit) = check_cached_apk(
        &cache_dir,
        source_sha,
        ApkSource::EmbeddedPrebuilt,
        Some(embed::PREBUILT_AGENT_APK_SHA256),
    ) {
        return Ok(hit);
    }

    let nonce = STAGING_NONCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_dir = android_root.join(format!(
        ".staging-prebuilt-{}-{nanos:x}-{nonce}",
        std::process::id()
    ));
    let mut guard = BuildGuard::new(staging_dir.clone());
    fs::create_dir_all(&staging_dir).map_err(|source| AndroidDriverError::Io {
        context: format!(
            "create Android prebuilt stage dir {}",
            staging_dir.display()
        ),
        source,
    })?;

    let staged_apk = staging_dir.join(APK_FILENAME);
    fs::write(&staged_apk, embed::PREBUILT_AGENT_APK).map_err(|source| AndroidDriverError::Io {
        context: format!("write embedded prebuilt APK {}", staged_apk.display()),
        source,
    })?;
    let verified_sha = verify_and_hash_apk(&staged_apk)?;
    if verified_sha != embed::PREBUILT_AGENT_APK_SHA256 {
        return Err(AndroidDriverError::ApkHashMismatch {
            path: staged_apk,
            expected: embed::PREBUILT_AGENT_APK_SHA256.to_string(),
            actual: verified_sha,
        });
    }
    fs::write(
        staging_dir.join(APK_SHA256_FILENAME),
        format!("{verified_sha}\n"),
    )
    .map_err(|source| AndroidDriverError::Io {
        context: "write prebuilt Android agent APK sha256 sidecar".to_string(),
        source,
    })?;
    let marker = ReadyMarker {
        source_sha256: source_sha.to_string(),
        apk_sha256: verified_sha.clone(),
        build_tools: "embedded-prebuilt".to_string(),
        platform: "android-36".to_string(),
    };
    let marker_json =
        serde_json::to_vec_pretty(&marker).map_err(|source| AndroidDriverError::Json {
            context: "serialize prebuilt Android agent ready.marker".to_string(),
            source,
        })?;
    fs::write(staging_dir.join(READY_MARKER_FILENAME), marker_json).map_err(|source| {
        AndroidDriverError::Io {
            context: "write prebuilt Android agent ready.marker".to_string(),
            source,
        }
    })?;

    if cache_dir.exists() {
        let _ = fs::remove_dir_all(&cache_dir);
    }
    fs::rename(&staging_dir, &cache_dir).map_err(|source| AndroidDriverError::Io {
        context: format!(
            "promote prebuilt Android agent cache dir {}",
            cache_dir.display()
        ),
        source,
    })?;
    guard.disarm();

    info!(
        apk_sha = %verified_sha,
        "materialized embedded prebuilt deja-android-agent.apk"
    );

    Ok(ResolvedAgentApk {
        apk_path: cache_dir.join(APK_FILENAME),
        sha256: verified_sha,
        source_sha256: source_sha.to_string(),
        source: ApkSource::EmbeddedPrebuilt,
        was_cache_hit: false,
        build_duration_ms: 0,
    })
}

/// Resolves `deja-android-agent.apk` using the following precedence:
///
/// 1. Explicit `DEJA_ANDROID_AGENT_APK` (+ optional `DEJA_ANDROID_AGENT_APK_SHA256` pin).
/// 2. Default zero-JDK embedded prebuilt APK materialized into `$DEJA_HOME/drivers/android/prebuilt-<sha256>/`.
/// 3. Opt-in source compilation (`DEJA_ANDROID_BUILD_AGENT_FROM_SOURCE=1`) into
///    `$DEJA_HOME/drivers/android/<source_sha256>/` with a persistent signing keystore.
pub async fn resolve_or_build_agent_apk(
    deja_home: &Path,
    env: &BTreeMap<String, String>,
    cancel: &CancellationToken,
) -> Result<ResolvedAgentApk> {
    if cancel.is_cancelled() {
        return Err(AndroidDriverError::Cancelled);
    }

    if let Some(prebuilt) = resolve_prebuilt_apk_from_env(env)? {
        return Ok(prebuilt);
    }

    let source_sha = embed::source_sha256();
    if !wants_source_build(env) {
        return materialize_embedded_prebuilt_apk(deja_home, &source_sha, cancel).await;
    }

    let android_root = deja_home.join("drivers").join("android");
    let cache_dir = android_root.join(&source_sha);
    let locks_dir = android_root.join("locks");

    if let Some(hit) = check_cached_apk(&cache_dir, &source_sha, ApkSource::CachedBuild, None) {
        debug!(source_sha = %source_sha, "deja-android-agent APK cache hit");
        return Ok(hit);
    }

    fs::create_dir_all(&locks_dir).map_err(|source| AndroidDriverError::Io {
        context: format!("create Android cache locks dir {}", locks_dir.display()),
        source,
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&locks_dir, fs::Permissions::from_mode(0o700));
    }

    let lock_path = locks_dir.join(format!("{source_sha}.lock"));
    let _lock = acquire_advisory_lock_cancellable(&lock_path, cancel).await?;

    if let Some(hit) = check_cached_apk(&cache_dir, &source_sha, ApkSource::CachedBuild, None) {
        debug!(source_sha = %source_sha, "deja-android-agent APK cache hit after lock");
        return Ok(hit);
    }

    let tools = discover_sdk_build_tools(env)?;
    let started = std::time::Instant::now();
    let nonce = STAGING_NONCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_dir = android_root.join(format!(
        ".staging-{source_sha}-{}-{nanos:x}-{nonce}",
        std::process::id()
    ));
    let src_dir = staging_dir.join("src");
    let out_apk = staging_dir.join(APK_FILENAME);
    let persistent_keystore = android_root.join(DEBUG_KEYSTORE_FILENAME);

    let mut guard = BuildGuard::new(staging_dir.clone());
    embed::materialize_project(&src_dir)?;

    let script = src_dir.join("build-apk.sh");
    let mut cmd = Command::new("/bin/bash");
    cmd.arg(&script)
        .arg("--sdk-root")
        .arg(&tools.sdk_root)
        .arg("--build-tools")
        .arg(&tools.build_tools_dir)
        .arg("--platform")
        .arg(&tools.android_jar)
        .arg("--keystore")
        .arg(&persistent_keystore)
        .arg("--out")
        .arg(&out_apk);

    for var in ["PATH", "JAVA_HOME", "HOME", "TMPDIR"] {
        if let Some(val) = env.get(var) {
            cmd.env(var, val);
        }
    }

    let (status, stdout, stderr) = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(AndroidDriverError::Cancelled),
        res = guard.run_command(cmd, "build-apk.sh") => res?,
    };

    if !status.success() {
        let stderr_str = String::from_utf8_lossy(&stderr).trim().to_string();
        let stdout_str = String::from_utf8_lossy(&stdout).trim().to_string();
        let detail = if stderr_str.is_empty() {
            stdout_str
        } else {
            stderr_str
        };
        return Err(AndroidDriverError::CommandFailed {
            program: "build-apk.sh".to_string(),
            args: format!("--out {}", out_apk.display()),
            status: status.code(),
            stderr: detail,
        });
    }

    let apk_sha = verify_and_hash_apk(&out_apk)?;

    // Build final staging cache payload directory and atomically swap into place.
    let final_stage = staging_dir.join("ready");
    fs::create_dir_all(&final_stage).map_err(|source| AndroidDriverError::Io {
        context: format!("create final stage dir {}", final_stage.display()),
        source,
    })?;
    fs::rename(&out_apk, final_stage.join(APK_FILENAME)).map_err(|source| {
        AndroidDriverError::Io {
            context: "stage built Android agent APK".to_string(),
            source,
        }
    })?;
    fs::write(
        final_stage.join(APK_SHA256_FILENAME),
        format!("{apk_sha}\n"),
    )
    .map_err(|source| AndroidDriverError::Io {
        context: "write Android agent APK sha256 sidecar".to_string(),
        source,
    })?;
    let marker = ReadyMarker {
        source_sha256: source_sha.clone(),
        apk_sha256: apk_sha.clone(),
        build_tools: tools
            .build_tools_dir
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
        platform: tools
            .android_jar
            .parent()
            .and_then(Path::file_name)
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    let marker_json =
        serde_json::to_vec_pretty(&marker).map_err(|source| AndroidDriverError::Json {
            context: "serialize Android agent ready.marker".to_string(),
            source,
        })?;
    fs::write(final_stage.join(READY_MARKER_FILENAME), marker_json).map_err(|source| {
        AndroidDriverError::Io {
            context: "write Android agent ready.marker".to_string(),
            source,
        }
    })?;

    if cache_dir.exists() {
        let _ = fs::remove_dir_all(&cache_dir);
    }
    fs::rename(&final_stage, &cache_dir).map_err(|source| AndroidDriverError::Io {
        context: format!("promote Android agent cache dir {}", cache_dir.display()),
        source,
    })?;
    let _ = fs::remove_dir_all(&staging_dir);
    guard.disarm();

    let build_duration_ms = started.elapsed().as_millis() as u64;
    info!(
        source_sha = %source_sha,
        apk_sha = %apk_sha,
        build_duration_ms,
        "built and cached deja-android-agent.apk"
    );

    Ok(ResolvedAgentApk {
        apk_path: cache_dir.join(APK_FILENAME),
        sha256: apk_sha,
        source_sha256: source_sha,
        source: ApkSource::CachedBuild,
        was_cache_hit: false,
        build_duration_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        AgentApkReadiness, ApkSource, inspect_agent_apk_readiness, resolve_or_build_agent_apk,
        verify_and_hash_apk,
    };
    use crate::error::AndroidDriverError;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio_util::sync::CancellationToken;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "deja-android-cache-{tag}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn prebuilt_apk_env_override_verifies_pin_and_rejects_symlinks_and_mismatch() {
        let dir = temp_dir("prebuilt");
        let apk = dir.join("agent.apk");
        fs::write(&apk, b"PK\x03\x04dummy-apk-payload").unwrap();
        let sha = verify_and_hash_apk(&apk).unwrap();

        let mut env = BTreeMap::new();
        env.insert(
            "DEJA_ANDROID_AGENT_APK".to_string(),
            apk.to_string_lossy().into_owned(),
        );
        env.insert("DEJA_ANDROID_AGENT_APK_SHA256".to_string(), sha.clone());

        let cancel = CancellationToken::new();
        let resolved = resolve_or_build_agent_apk(&dir, &env, &cancel)
            .await
            .expect("matching pin should resolve");
        assert_eq!(resolved.source, ApkSource::PrebuiltEnv);
        assert_eq!(resolved.sha256, sha);
        assert!(resolved.was_cache_hit);

        // Wrong pin must fail closed with ApkHashMismatch.
        env.insert("DEJA_ANDROID_AGENT_APK_SHA256".to_string(), "0".repeat(64));
        let err = resolve_or_build_agent_apk(&dir, &env, &cancel)
            .await
            .expect_err("wrong SHA-256 pin must fail closed");
        assert!(matches!(err, AndroidDriverError::ApkHashMismatch { .. }));

        // Symlink APK must fail closed.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let link = dir.join("symlink.apk");
            symlink(&apk, &link).unwrap();
            let mut sym_env = BTreeMap::new();
            sym_env.insert(
                "DEJA_ANDROID_AGENT_APK".to_string(),
                link.to_string_lossy().into_owned(),
            );
            assert!(
                resolve_or_build_agent_apk(&dir, &sym_env, &cancel)
                    .await
                    .is_err()
            );
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn default_zero_jdk_env_materializes_embedded_prebuilt_apk_and_hits_cache() {
        let dir = temp_dir("embedded-prebuilt");
        let empty_env = BTreeMap::new();
        let cancel = CancellationToken::new();

        // Read-only readiness reports EmbeddedPrebuiltReady on a JDK-free / SDK-free host.
        assert!(matches!(
            inspect_agent_apk_readiness(Some(&dir), &empty_env),
            AgentApkReadiness::EmbeddedPrebuiltReady { ref sha256 }
                if sha256 == crate::embed::PREBUILT_AGENT_APK_SHA256
        ));

        // First resolution materializes the embedded prebuilt APK with zero JDK/SDK.
        let first = resolve_or_build_agent_apk(&dir, &empty_env, &cancel)
            .await
            .expect("embedded prebuilt APK must materialize without JDK or Android SDK");
        assert_eq!(first.source, ApkSource::EmbeddedPrebuilt);
        assert_eq!(first.sha256, crate::embed::PREBUILT_AGENT_APK_SHA256);
        assert!(!first.was_cache_hit);
        assert_eq!(
            crate::apk::extract_apk_package_id_from_path(&first.apk_path).unwrap(),
            crate::process::AGENT_PACKAGE_ID
        );

        // Second resolution hits the verified cache without rewriting.
        let second = resolve_or_build_agent_apk(&dir, &empty_env, &cancel)
            .await
            .expect("second resolution hits materialized prebuilt cache");
        assert_eq!(second.source, ApkSource::EmbeddedPrebuilt);
        assert!(second.was_cache_hit);
        assert_eq!(second.apk_path, first.apk_path);

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn source_build_opt_in_fails_closed_when_no_sdk_present() {
        let dir = temp_dir("no-sdk");
        let mut env = BTreeMap::new();
        env.insert(
            "DEJA_ANDROID_BUILD_AGENT_FROM_SOURCE".to_string(),
            "1".to_string(),
        );
        let cancel = CancellationToken::new();
        let err = resolve_or_build_agent_apk(&dir, &env, &cancel)
            .await
            .expect_err("source build opt-in must fail closed without SDK");
        assert!(matches!(err, AndroidDriverError::ApkUnavailable(_)));
        assert!(err.to_string().contains("DEJA_ANDROID_AGENT_APK"));

        let readiness = inspect_agent_apk_readiness(Some(&dir), &env);
        assert!(matches!(readiness, AgentApkReadiness::Unavailable { .. }));

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn cached_source_build_hit_requires_valid_marker_and_matching_sidecar() {
        let dir = temp_dir("cache-hit");
        let source_sha = crate::embed::source_sha256();
        let cache_dir = dir.join("drivers").join("android").join(&source_sha);
        fs::create_dir_all(&cache_dir).unwrap();
        let apk = cache_dir.join("deja-android-agent.apk");
        fs::write(&apk, b"PK\x03\x04cached-apk-bytes").unwrap();
        let sha = verify_and_hash_apk(&apk).unwrap();
        fs::write(
            cache_dir.join("deja-android-agent.apk.sha256"),
            format!("{sha}\n"),
        )
        .unwrap();
        fs::write(cache_dir.join("ready.marker"), b"{}").unwrap();

        let mut env = BTreeMap::new();
        env.insert(
            "DEJA_ANDROID_BUILD_AGENT_FROM_SOURCE".to_string(),
            "1".to_string(),
        );
        let cancel = CancellationToken::new();
        let hit = resolve_or_build_agent_apk(&dir, &env, &cancel)
            .await
            .expect("valid cache must hit without SDK");
        assert_eq!(hit.source, ApkSource::CachedBuild);
        assert!(hit.was_cache_hit);
        assert_eq!(hit.sha256, sha);
        assert!(matches!(
            inspect_agent_apk_readiness(Some(&dir), &env),
            AgentApkReadiness::CacheHit { .. }
        ));

        // Tampered APK invalidates cache.
        fs::write(&apk, b"PK\x03\x04tampered-bytes").unwrap();
        assert!(matches!(
            inspect_agent_apk_readiness(Some(&dir), &env),
            AgentApkReadiness::Unavailable { .. }
        ));

        let _ = fs::remove_dir_all(&dir);
    }
}
