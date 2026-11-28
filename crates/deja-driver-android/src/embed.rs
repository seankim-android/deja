//! Embedded zero-Gradle Android UiAutomator2 HTTP instrumentation agent sources.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{AndroidDriverError, Result};

/// A single embedded source file from `drivers/android/deja-android-agent/`.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedFile {
    pub relative_path: &'static str,
    pub contents: &'static str,
    pub executable: bool,
}

/// All files required to compile `deja-android-agent.apk`, in deterministic lexicographical order.
pub const EMBEDDED_FILES: &[EmbeddedFile] = &[
    EmbeddedFile {
        relative_path: "AndroidManifest.xml",
        contents: include_str!("../../../drivers/android/deja-android-agent/AndroidManifest.xml"),
        executable: false,
    },
    EmbeddedFile {
        relative_path: "build-apk.sh",
        contents: include_str!("../../../drivers/android/deja-android-agent/build-apk.sh"),
        executable: true,
    },
    EmbeddedFile {
        relative_path: "src/dev/deja/agent/DejaAgentInstrumentation.java",
        contents: include_str!(
            "../../../drivers/android/deja-android-agent/src/dev/deja/agent/DejaAgentInstrumentation.java"
        ),
        executable: false,
    },
    EmbeddedFile {
        relative_path: "src/dev/deja/agent/HierarchySnapshot.java",
        contents: include_str!(
            "../../../drivers/android/deja-android-agent/src/dev/deja/agent/HierarchySnapshot.java"
        ),
        executable: false,
    },
    EmbeddedFile {
        relative_path: "src/dev/deja/agent/HttpServer.java",
        contents: include_str!(
            "../../../drivers/android/deja-android-agent/src/dev/deja/agent/HttpServer.java"
        ),
        executable: false,
    },
    EmbeddedFile {
        relative_path: "src/dev/deja/agent/UiActions.java",
        contents: include_str!(
            "../../../drivers/android/deja-android-agent/src/dev/deja/agent/UiActions.java"
        ),
        executable: false,
    },
];

/// Computes a deterministic 64-character lowercase SHA-256 hex digest over all embedded
/// `deja-android-agent` source files.
#[must_use]
pub fn source_sha256() -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"deja-android-agent-v1\0");
    for file in EMBEDDED_FILES {
        hasher.update((file.relative_path.len() as u64).to_be_bytes());
        hasher.update(file.relative_path.as_bytes());
        let bytes = file.contents.as_bytes();
        hasher.update((bytes.len() as u64).to_be_bytes());
        hasher.update(bytes);
    }
    hex::encode(hasher.finalize())
}

/// Materializes all embedded `deja-android-agent` source files under `dest_root`.
pub fn materialize_project(dest_root: &Path) -> Result<()> {
    std::fs::create_dir_all(dest_root).map_err(|source| AndroidDriverError::Io {
        context: format!("create Android agent project root {}", dest_root.display()),
        source,
    })?;

    for file in EMBEDDED_FILES {
        let target = dest_root.join(file.relative_path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| AndroidDriverError::Io {
                context: format!("create parent dir {}", parent.display()),
                source,
            })?;
        }
        std::fs::write(&target, file.contents.as_bytes()).map_err(|source| {
            AndroidDriverError::Io {
                context: format!("write embedded file {}", target.display()),
                source,
            }
        })?;
        #[cfg(unix)]
        if file.executable {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700)).map_err(
                |source| AndroidDriverError::Io {
                    context: format!("chmod embedded script {}", target.display()),
                    source,
                },
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{EMBEDDED_FILES, materialize_project, source_sha256};

    #[test]
    fn embedded_files_are_sorted_and_non_empty() {
        assert_eq!(EMBEDDED_FILES.len(), 6);
        for pair in EMBEDDED_FILES.windows(2) {
            assert!(
                pair[0].relative_path < pair[1].relative_path,
                "embedded files must be sorted: {} >= {}",
                pair[0].relative_path,
                pair[1].relative_path
            );
        }
        for file in EMBEDDED_FILES {
            assert!(
                !file.contents.is_empty(),
                "embedded file {} must not be empty",
                file.relative_path
            );
        }
    }

    #[test]
    fn source_sha256_is_deterministic_64_hex() {
        let first = source_sha256();
        let second = source_sha256();
        assert_eq!(first, second);
        assert_eq!(first.len(), 64);
        assert!(
            first
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        );
    }

    #[test]
    fn materialize_project_writes_all_files() {
        let dir = std::env::temp_dir().join(format!(
            "deja-android-embed-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        materialize_project(&dir).expect("materialize should succeed");
        for file in EMBEDDED_FILES {
            let written =
                std::fs::read_to_string(dir.join(file.relative_path)).expect("file written");
            assert_eq!(written, file.contents);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
