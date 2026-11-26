use crate::embed;
use crate::error::{IosDriverError, Result};
use crate::simctl::SimctlEnv;
use serde::Serialize;
use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use tokio::process::Command;
use tracing::{debug, info};

static STAGING_NONCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolchainInfo {
    pub xcode_version: String,
    pub xcode_build: String,
    pub ios_sdk: String,
}

#[derive(Debug, Clone)]
pub struct RunnerBuildArtifact {
    pub cache_key: String,
    pub cache_dir: PathBuf,
    pub products_dir: PathBuf,
    pub template_xctestrun: PathBuf,
    pub toolchain: ToolchainInfo,
    pub source_sha256: String,
    pub was_cache_hit: bool,
    pub build_duration_ms: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ReadyMarker {
    cache_key: String,
    toolchain: ToolchainInfo,
    source_sha256: String,
}

/// Resolves the Déjà home directory: explicit override -> `$DEJA_HOME` -> `~/.deja`.
pub fn resolve_deja_home(override_home: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = override_home {
        return Ok(dir.to_path_buf());
    }
    if let Some(env_home) = std::env::var_os("DEJA_HOME")
        && !env_home.is_empty()
    {
        return Ok(PathBuf::from(env_home));
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .ok_or_else(|| {
            IosDriverError::InvalidPath(
                "neither DEJA_HOME nor HOME is set in the environment".to_string(),
            )
        })?;
    Ok(PathBuf::from(home).join(".deja"))
}

/// Computes the deterministic runner cache key `<xcode-build>-<ios-sdk>-<source-sha256>`.
pub fn build_cache_key(xcode_build: &str, ios_sdk: &str, source_sha256: &str) -> String {
    format!(
        "{}-{}-{}",
        sanitize_key_part(xcode_build),
        sanitize_key_part(ios_sdk),
        sanitize_key_part(source_sha256)
    )
}

fn sanitize_key_part(input: &str) -> String {
    input
        .trim()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Parses `xcodebuild -version` output into `(xcode_version, xcode_build)`.
pub fn parse_xcodebuild_version(output: &str) -> Result<(String, String)> {
    let mut xcode_version = None;
    let mut xcode_build = None;
    for line in output.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Xcode ") {
            let val = rest.trim();
            if !val.is_empty() {
                xcode_version = Some(val.to_string());
            }
        } else if let Some(rest) = trimmed.strip_prefix("Build version ") {
            let val = rest.trim();
            if !val.is_empty() {
                xcode_build = Some(val.to_string());
            }
        }
    }
    match (xcode_version, xcode_build) {
        (Some(v), Some(b)) => Ok((v, b)),
        _ => Err(IosDriverError::Toolchain(format!(
            "unexpected `xcodebuild -version` output: {}",
            output.trim()
        ))),
    }
}

/// Parses `xcrun --sdk iphonesimulator --show-sdk-version` output.
pub fn parse_sdk_version(output: &str) -> Result<String> {
    let version = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| {
            IosDriverError::Toolchain("empty iOS Simulator SDK version output".to_string())
        })?;
    Ok(version.to_string())
}

/// Detects active Xcode build version and iOS Simulator SDK version (honoring `DEVELOPER_DIR`).
pub async fn detect_toolchain(env: &SimctlEnv) -> Result<ToolchainInfo> {
    let mut xb_cmd = Command::new("xcodebuild");
    xb_cmd.arg("-version").stdin(Stdio::null());
    apply_env(&mut xb_cmd, env);
    let xb_out = xb_cmd
        .output()
        .await
        .map_err(|source| IosDriverError::CommandSpawn {
            program: "xcodebuild".to_string(),
            source,
        })?;
    if !xb_out.status.success() {
        return Err(IosDriverError::CommandFailed {
            program: "xcodebuild".to_string(),
            args: "-version".to_string(),
            status: xb_out.status.code(),
            stderr: String::from_utf8_lossy(&xb_out.stderr).trim().to_string(),
        });
    }
    let (xcode_version, xcode_build) =
        parse_xcodebuild_version(&String::from_utf8_lossy(&xb_out.stdout))?;

    let mut sdk_cmd = Command::new("xcrun");
    sdk_cmd
        .args(["--sdk", "iphonesimulator", "--show-sdk-version"])
        .stdin(Stdio::null());
    apply_env(&mut sdk_cmd, env);
    let sdk_out = sdk_cmd
        .output()
        .await
        .map_err(|source| IosDriverError::CommandSpawn {
            program: "xcrun".to_string(),
            source,
        })?;
    if !sdk_out.status.success() {
        return Err(IosDriverError::CommandFailed {
            program: "xcrun".to_string(),
            args: "--sdk iphonesimulator --show-sdk-version".to_string(),
            status: sdk_out.status.code(),
            stderr: String::from_utf8_lossy(&sdk_out.stderr).trim().to_string(),
        });
    }
    let ios_sdk = parse_sdk_version(&String::from_utf8_lossy(&sdk_out.stdout))?;

    Ok(ToolchainInfo {
        xcode_version,
        xcode_build,
        ios_sdk,
    })
}

fn apply_env(cmd: &mut Command, env: &SimctlEnv) {
    if let Some(ref path) = env.env_path {
        cmd.env("PATH", path);
    }
    if let Some(ref dev_dir) = env.developer_dir {
        cmd.env("DEVELOPER_DIR", dev_dir);
    }
}

fn native_simulator_arch() -> &'static str {
    #[cfg(target_arch = "aarch64")]
    {
        "arm64"
    }
    #[cfg(target_arch = "x86_64")]
    {
        "x86_64"
    }
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        "arm64"
    }
}

fn find_xctestrun_in_products(products_dir: &Path) -> Result<PathBuf> {
    let entries = std::fs::read_dir(products_dir).map_err(|source| IosDriverError::Io {
        context: format!("read products directory {}", products_dir.display()),
        source,
    })?;
    let mut matches: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|entry| entry.path()))
        .filter(|p| p.extension().and_then(|ext| ext.to_str()) == Some("xctestrun"))
        .collect();
    matches.sort();
    matches
        .into_iter()
        .next()
        .ok_or_else(|| IosDriverError::InvalidXctestrun {
            path: products_dir.to_path_buf(),
            detail: "no `.xctestrun` file generated by `xcodebuild build-for-testing`".to_string(),
        })
}

fn check_cached_build(
    cache_key: &str,
    cache_dir: &Path,
    toolchain: &ToolchainInfo,
    source_sha256: &str,
) -> Option<RunnerBuildArtifact> {
    let marker = cache_dir.join("ready.marker");
    if !marker.is_file() {
        return None;
    }
    let products_dir = cache_dir.join("derived").join("Build").join("Products");
    let template_xctestrun = find_xctestrun_in_products(&products_dir).ok()?;
    Some(RunnerBuildArtifact {
        cache_key: cache_key.to_string(),
        cache_dir: cache_dir.to_path_buf(),
        products_dir,
        template_xctestrun,
        toolchain: toolchain.clone(),
        source_sha256: source_sha256.to_string(),
        was_cache_hit: true,
        build_duration_ms: 0,
    })
}

/// Ensures `DejaXCTestRunner` is built and cached under `<deja_home>/ios-runner/builds/<key>`.
///
/// Uses an exclusive `std::fs::File::lock()` advisory lock inside `spawn_blocking`, builds into
/// a private staging directory, writes `ready.marker`, and atomically renames into place.
pub async fn ensure_runner_built(deja_home: &Path, env: &SimctlEnv) -> Result<RunnerBuildArtifact> {
    let toolchain = detect_toolchain(env).await?;
    let source_sha256 = embed::source_sha256();
    let cache_key = build_cache_key(&toolchain.xcode_build, &toolchain.ios_sdk, &source_sha256);

    let runner_root = deja_home.join("ios-runner");
    let builds_dir = runner_root.join("builds");
    let locks_dir = runner_root.join("locks");
    let cache_dir = builds_dir.join(&cache_key);

    if let Some(hit) = check_cached_build(&cache_key, &cache_dir, &toolchain, &source_sha256) {
        debug!(cache_key = %cache_key, "DejaXCTestRunner cache hit");
        return Ok(hit);
    }

    std::fs::create_dir_all(&builds_dir).map_err(|source| IosDriverError::Io {
        context: format!("create builds dir {}", builds_dir.display()),
        source,
    })?;
    std::fs::create_dir_all(&locks_dir).map_err(|source| IosDriverError::Io {
        context: format!("create locks dir {}", locks_dir.display()),
        source,
    })?;

    let lock_path = locks_dir.join(format!("{cache_key}.lock"));
    let lock_file = tokio::task::spawn_blocking(move || -> Result<std::fs::File> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| IosDriverError::Io {
                context: format!("open runner build lock {}", lock_path.display()),
                source,
            })?;
        file.lock().map_err(|source| IosDriverError::Io {
            context: format!("acquire runner build lock {}", lock_path.display()),
            source,
        })?;
        Ok(file)
    })
    .await
    .map_err(|err| IosDriverError::Toolchain(format!("lock task failed: {err}")))??;

    // Re-check under the exclusive lock so concurrent builders reuse the freshly completed build.
    if let Some(hit) = check_cached_build(&cache_key, &cache_dir, &toolchain, &source_sha256) {
        drop(lock_file);
        debug!(cache_key = %cache_key, "DejaXCTestRunner cache hit after acquiring lock");
        return Ok(hit);
    }

    let nonce = STAGING_NONCE.fetch_add(1, Ordering::Relaxed);
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let staging_dir = builds_dir.join(format!(
        ".staging-{cache_key}-{}-{ts}-{nonce}",
        std::process::id()
    ));

    let build_result = build_into_staging(
        &staging_dir,
        &cache_dir,
        &cache_key,
        &toolchain,
        &source_sha256,
        env,
    )
    .await;

    if build_result.is_err() && staging_dir.exists() {
        let _ = std::fs::remove_dir_all(&staging_dir);
    }
    drop(lock_file);
    build_result
}

async fn build_into_staging(
    staging_dir: &Path,
    cache_dir: &Path,
    cache_key: &str,
    toolchain: &ToolchainInfo,
    source_sha256: &str,
    env: &SimctlEnv,
) -> Result<RunnerBuildArtifact> {
    let started = Instant::now();
    let source_dir = staging_dir.join("source");
    let derived_dir = staging_dir.join("derived");

    let source_dir_clone = source_dir.clone();
    tokio::task::spawn_blocking(move || embed::materialize_project(&source_dir_clone))
        .await
        .map_err(|err| IosDriverError::Toolchain(format!("materialize task failed: {err}")))??;

    let project_path = source_dir.join("DejaXCTestRunner.xcodeproj");
    let arch_setting = format!("ARCHS={}", native_simulator_arch());

    info!(
        cache_key = %cache_key,
        arch = native_simulator_arch(),
        "building DejaXCTestRunner via xcodebuild build-for-testing"
    );

    let mut cmd = Command::new("xcodebuild");
    cmd.arg("build-for-testing")
        .arg("-project")
        .arg(&project_path)
        .args(["-scheme", "DejaXCTestRunner", "-configuration", "Debug"])
        .args(["-destination", "generic/platform=iOS Simulator"])
        .arg("-derivedDataPath")
        .arg(&derived_dir)
        .arg(&arch_setting)
        .arg("ONLY_ACTIVE_ARCH=YES")
        .stdin(Stdio::null());
    apply_env(&mut cmd, env);

    let output = cmd
        .output()
        .await
        .map_err(|source| IosDriverError::CommandSpawn {
            program: "xcodebuild".to_string(),
            source,
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let combined = format!(
            "stderr:\n{}\nstdout tail:\n{}",
            tail_str(&stderr, 4_096),
            tail_str(&stdout, 4_096)
        );
        return Err(IosDriverError::CommandFailed {
            program: "xcodebuild".to_string(),
            args: "build-for-testing -scheme DejaXCTestRunner".to_string(),
            status: output.status.code(),
            stderr: combined,
        });
    }

    let staging_products = derived_dir.join("Build").join("Products");
    let _ = find_xctestrun_in_products(&staging_products)?;

    let marker = ReadyMarker {
        cache_key: cache_key.to_string(),
        toolchain: toolchain.clone(),
        source_sha256: source_sha256.to_string(),
    };
    let marker_bytes =
        serde_json::to_vec_pretty(&marker).map_err(|source| IosDriverError::Json {
            context: "serialize runner ready.marker".to_string(),
            source,
        })?;
    std::fs::write(staging_dir.join("ready.marker"), marker_bytes).map_err(|source| {
        IosDriverError::Io {
            context: format!("write ready.marker in {}", staging_dir.display()),
            source,
        }
    })?;

    if cache_dir.exists() {
        let _ = std::fs::remove_dir_all(cache_dir);
    }
    std::fs::rename(staging_dir, cache_dir).map_err(|source| IosDriverError::Io {
        context: format!(
            "atomically promote runner staging {} -> {}",
            staging_dir.display(),
            cache_dir.display()
        ),
        source,
    })?;

    let products_dir = cache_dir.join("derived").join("Build").join("Products");
    let template_xctestrun = find_xctestrun_in_products(&products_dir)?;
    let build_duration_ms = started.elapsed().as_millis() as u64;

    info!(
        cache_key = %cache_key,
        build_duration_ms,
        "DejaXCTestRunner cold build cached"
    );

    Ok(RunnerBuildArtifact {
        cache_key: cache_key.to_string(),
        cache_dir: cache_dir.to_path_buf(),
        products_dir,
        template_xctestrun,
        toolchain: toolchain.clone(),
        source_sha256: source_sha256.to_string(),
        was_cache_hit: false,
        build_duration_ms,
    })
}

/// Materializes a per-run `.xctestrun` file from the cached template:
/// - Expands `__TESTROOT__` -> `<cache_dir>/derived/Build/Products`
/// - Forces `TestTimeoutsEnabled = false`
/// - Forces `SystemAttachmentLifetime = keepNever`
/// - Forces `UserAttachmentLifetime = keepNever`
pub fn materialize_run_xctestrun(
    artifact: &RunnerBuildArtifact,
    dest_path: &Path,
) -> Result<PathBuf> {
    let template_bytes =
        std::fs::read(&artifact.template_xctestrun).map_err(|source| IosDriverError::Io {
            context: format!(
                "read cached xctestrun template {}",
                artifact.template_xctestrun.display()
            ),
            source,
        })?;
    let template_xml =
        String::from_utf8(template_bytes).map_err(|_| IosDriverError::InvalidXctestrun {
            path: artifact.template_xctestrun.clone(),
            detail: "expected UTF-8 XML plist".to_string(),
        })?;

    let test_root = artifact.products_dir.to_str().ok_or_else(|| {
        IosDriverError::InvalidPath(format!(
            "non-UTF-8 cached products path: {}",
            artifact.products_dir.display()
        ))
    })?;

    let rewritten = rewrite_xctestrun_xml(&template_xml, test_root).map_err(|detail| {
        IosDriverError::InvalidXctestrun {
            path: artifact.template_xctestrun.clone(),
            detail,
        }
    })?;

    std::fs::write(dest_path, rewritten.as_bytes()).map_err(|source| IosDriverError::Io {
        context: format!("write per-run xctestrun {}", dest_path.display()),
        source,
    })?;
    Ok(dest_path.to_path_buf())
}

/// Pure XML `.xctestrun` transformer (tested on both Linux and macOS).
pub fn rewrite_xctestrun_xml(xml: &str, test_root: &str) -> std::result::Result<String, String> {
    if !xml.contains("<plist") {
        return Err("missing <plist> root element".to_string());
    }
    let escaped_root = escape_xml_text(test_root);
    let mut result = xml.replace("__TESTROOT__", &escaped_root);
    result = set_plist_key_value(&result, "TestTimeoutsEnabled", "<false/>")?;
    result = set_plist_key_value(
        &result,
        "SystemAttachmentLifetime",
        "<string>keepNever</string>",
    )?;
    result = set_plist_key_value(
        &result,
        "UserAttachmentLifetime",
        "<string>keepNever</string>",
    )?;
    Ok(result)
}

fn escape_xml_text(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

fn set_plist_key_value(
    xml: &str,
    key: &str,
    replacement_value: &str,
) -> std::result::Result<String, String> {
    let needle = format!("<key>{key}</key>");
    if xml.contains(&needle) {
        let mut out = String::with_capacity(xml.len() + 64);
        let mut cursor = 0usize;
        while let Some(rel_idx) = xml[cursor..].find(&needle) {
            let key_start = cursor + rel_idx;
            let key_end = key_start + needle.len();
            out.push_str(&xml[cursor..key_end]);

            let after_key = &xml[key_end..];
            let ws_len = after_key
                .find('<')
                .ok_or_else(|| format!("missing XML value element after {needle}"))?;
            out.push_str(&after_key[..ws_len]);

            let value_slice = &after_key[ws_len..];
            let val_len = xml_simple_element_len(value_slice)
                .ok_or_else(|| format!("unsupported XML value element after {needle}"))?;
            out.push_str(replacement_value);
            cursor = key_end + ws_len + val_len;
        }
        out.push_str(&xml[cursor..]);
        return Ok(out);
    }

    // Insert inside the `<key>DejaXCTestRunner</key>\n...<dict>` target dictionary if absent.
    let target_anchor = "<key>DejaXCTestRunner</key>";
    let anchor_idx = xml
        .find(target_anchor)
        .ok_or_else(|| format!("missing `{target_anchor}` target dict to insert `{key}`"))?;
    let after_anchor = &xml[anchor_idx + target_anchor.len()..];
    let dict_rel = after_anchor
        .find("<dict>")
        .ok_or_else(|| format!("missing `<dict>` after `{target_anchor}`"))?;
    let insert_pos = anchor_idx + target_anchor.len() + dict_rel + "<dict>".len();

    let mut out = String::with_capacity(xml.len() + needle.len() + replacement_value.len() + 16);
    out.push_str(&xml[..insert_pos]);
    out.push_str("\n\t\t");
    out.push_str(&needle);
    out.push_str("\n\t\t");
    out.push_str(replacement_value);
    out.push_str(&xml[insert_pos..]);
    Ok(out)
}

fn xml_simple_element_len(slice: &str) -> Option<usize> {
    if !slice.starts_with('<') {
        return None;
    }
    let open_end = slice.find('>')?;
    if slice[..open_end].ends_with('/') {
        return Some(open_end + 1);
    }
    let tag_name = slice[1..open_end].split_whitespace().next()?;
    let close_tag = format!("</{tag_name}>");
    let close_idx = slice[open_end + 1..].find(&close_tag)?;
    Some(open_end + 1 + close_idx + close_tag.len())
}

fn tail_str(input: &str, max_bytes: usize) -> &str {
    if input.len() <= max_bytes {
        return input;
    }
    let mut start = input.len() - max_bytes;
    while !input.is_char_boundary(start) {
        start += 1;
    }
    &input[start..]
}

#[cfg(test)]
mod tests {
    use super::{
        build_cache_key, parse_sdk_version, parse_xcodebuild_version, rewrite_xctestrun_xml,
    };

    const SAMPLE_XCTESTRUN: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>DejaXCTestRunner</key>
	<dict>
		<key>SystemAttachmentLifetime</key>
		<string>deleteOnSuccess</string>
		<key>TestHostPath</key>
		<string>__TESTROOT__/Debug-iphonesimulator/DejaXCTestRunner-Runner.app</string>
		<key>TestTimeoutsEnabled</key>
		<true/>
		<key>UITargetAppPath</key>
		<string>__TESTROOT__/Debug-iphonesimulator/DejaRunnerHost.app</string>
		<key>UserAttachmentLifetime</key>
		<string>deleteOnSuccess</string>
	</dict>
</dict>
</plist>
"#;

    #[test]
    fn parses_xcodebuild_and_sdk_versions() {
        let (ver, build) =
            parse_xcodebuild_version("Xcode 27.2\nBuild version 27B52\n").expect("parse");
        assert_eq!(ver, "27.2");
        assert_eq!(build, "27B52");
        assert_eq!(parse_sdk_version("27.2\n").expect("sdk"), "27.2");
        assert!(parse_xcodebuild_version("garbage").is_err());
    }

    #[test]
    fn builds_clean_cache_key() {
        let key = build_cache_key("27B52", "27.2", "abcdef0123456789");
        assert_eq!(key, "27B52-27.2-abcdef0123456789");
    }

    #[test]
    fn rewrites_xctestrun_testroot_and_lifetimes() {
        let rewritten = rewrite_xctestrun_xml(
            SAMPLE_XCTESTRUN,
            "/var/tmp/deja&cache/derived/Build/Products",
        )
        .expect("rewrite");
        assert!(!rewritten.contains("__TESTROOT__"));
        assert!(rewritten.contains(
            "/var/tmp/deja&amp;cache/derived/Build/Products/Debug-iphonesimulator/DejaXCTestRunner-Runner.app"
        ));
        assert!(rewritten.contains("<key>TestTimeoutsEnabled</key>\n\t\t<false/>"));
        assert!(
            rewritten
                .contains("<key>SystemAttachmentLifetime</key>\n\t\t<string>keepNever</string>")
        );
        assert!(
            rewritten.contains("<key>UserAttachmentLifetime</key>\n\t\t<string>keepNever</string>")
        );
        assert!(!rewritten.contains("deleteOnSuccess"));
    }

    #[test]
    fn inserts_missing_xctestrun_keys() {
        let minimal = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
	<key>DejaXCTestRunner</key>
	<dict>
		<key>TestHostPath</key>
		<string>__TESTROOT__/Debug-iphonesimulator/DejaXCTestRunner-Runner.app</string>
	</dict>
</dict>
</plist>"#;

        let rewritten = rewrite_xctestrun_xml(minimal, "/cache/Products").expect("rewrite");
        assert!(rewritten.contains("<key>TestTimeoutsEnabled</key>\n\t\t<false/>"));
        assert!(
            rewritten
                .contains("<key>SystemAttachmentLifetime</key>\n\t\t<string>keepNever</string>")
        );
        assert!(
            rewritten.contains("<key>UserAttachmentLifetime</key>\n\t\t<string>keepNever</string>")
        );
    }
}
