use crate::error::{IosDriverError, Result};
use sha2::{Digest, Sha256};
use std::path::Path;

/// A single embedded source file from `drivers/ios/DejaXCTestRunner/`.
#[derive(Debug, Clone, Copy)]
pub struct EmbeddedFile {
    pub relative_path: &'static str,
    pub contents: &'static [u8],
}

/// All files required to build `DejaXCTestRunner.xcodeproj`, in deterministic lexicographical order.
pub const EMBEDDED_FILES: &[EmbeddedFile] = &[
    EmbeddedFile {
        relative_path: "DejaRunnerHost/DejaRunnerHostApp.swift",
        contents: include_bytes!(
            "../../../drivers/ios/DejaXCTestRunner/DejaRunnerHost/DejaRunnerHostApp.swift"
        ),
    },
    EmbeddedFile {
        relative_path: "DejaRunnerHost/Info.plist",
        contents: include_bytes!("../../../drivers/ios/DejaXCTestRunner/DejaRunnerHost/Info.plist"),
    },
    EmbeddedFile {
        relative_path: "DejaXCTestRunner.xcodeproj/project.pbxproj",
        contents: include_bytes!(
            "../../../drivers/ios/DejaXCTestRunner/DejaXCTestRunner.xcodeproj/project.pbxproj"
        ),
    },
    EmbeddedFile {
        relative_path: "DejaXCTestRunner.xcodeproj/xcshareddata/xcschemes/DejaXCTestRunner.xcscheme",
        contents: include_bytes!(
            "../../../drivers/ios/DejaXCTestRunner/DejaXCTestRunner.xcodeproj/xcshareddata/xcschemes/DejaXCTestRunner.xcscheme"
        ),
    },
    EmbeddedFile {
        relative_path: "DejaXCTestRunner/DejaXCTestRunnerAgent.swift",
        contents: include_bytes!(
            "../../../drivers/ios/DejaXCTestRunner/DejaXCTestRunner/DejaXCTestRunnerAgent.swift"
        ),
    },
    EmbeddedFile {
        relative_path: "DejaXCTestRunner/HttpServer.swift",
        contents: include_bytes!(
            "../../../drivers/ios/DejaXCTestRunner/DejaXCTestRunner/HttpServer.swift"
        ),
    },
    EmbeddedFile {
        relative_path: "DejaXCTestRunner/Info.plist",
        contents: include_bytes!(
            "../../../drivers/ios/DejaXCTestRunner/DejaXCTestRunner/Info.plist"
        ),
    },
    EmbeddedFile {
        relative_path: "DejaXCTestRunner/XcuiActions.swift",
        contents: include_bytes!(
            "../../../drivers/ios/DejaXCTestRunner/DejaXCTestRunner/XcuiActions.swift"
        ),
    },
];

/// Computes a deterministic SHA-256 hex digest over all embedded `DejaXCTestRunner` project files.
pub fn source_sha256() -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"deja-xctest-runner-v1\0");
    for file in EMBEDDED_FILES {
        hasher.update((file.relative_path.len() as u64).to_be_bytes());
        hasher.update(file.relative_path.as_bytes());
        hasher.update((file.contents.len() as u64).to_be_bytes());
        hasher.update(file.contents);
    }
    hex::encode(hasher.finalize())
}

/// Materializes all embedded `DejaXCTestRunner` source files under `dest_root`.
pub fn materialize_project(dest_root: &Path) -> Result<()> {
    std::fs::create_dir_all(dest_root).map_err(|source| IosDriverError::Io {
        context: format!("create runner project root {}", dest_root.display()),
        source,
    })?;

    for file in EMBEDDED_FILES {
        let target = dest_root.join(file.relative_path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|source| IosDriverError::Io {
                context: format!("create parent dir {}", parent.display()),
                source,
            })?;
        }
        std::fs::write(&target, file.contents).map_err(|source| IosDriverError::Io {
            context: format!("write embedded file {}", target.display()),
            source,
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{EMBEDDED_FILES, materialize_project, source_sha256};

    #[test]
    fn embedded_files_are_sorted_and_non_empty() {
        assert_eq!(EMBEDDED_FILES.len(), 8);
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
            "deja-ios-embed-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        materialize_project(&dir).expect("materialize should succeed");
        for file in EMBEDDED_FILES {
            let written = std::fs::read(dir.join(file.relative_path)).expect("file written");
            assert_eq!(written.as_slice(), file.contents);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
