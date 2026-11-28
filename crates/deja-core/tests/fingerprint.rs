//! Streaming build fingerprinting goldens (APK, iOS `.app`, Web static tree), multi-chunk
//! equivalence, symlink/empty rejection, and fail-closed `BuildFingerprintCache` tests (`DEJA-53`).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use deja_core::{
    BuildFingerprintCache, CoreError, FINGERPRINT_CHUNK_BYTES, fingerprint_file, fingerprint_tree,
};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(prefix: &str) -> Self {
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("deja-fp-test-{prefix}-{}-{id}", std::process::id()));
        fs::create_dir_all(&dir).expect("create temp dir");
        let canonical = fs::canonicalize(&dir).expect("canonicalize temp dir");
        Self { path: canonical }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, rel: &str, bytes: impl AsRef<[u8]>) {
        let target = self.path.join(rel);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).expect("create parent dirs");
        }
        fs::write(&target, bytes).expect("write fixture file");
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[test]
fn matches_typescript_sha256_goldens_for_apk_ios_app_and_web_tree() {
    assert_eq!(FINGERPRINT_CHUNK_BYTES, 65_536);

    let tmp = TempDir::new("goldens");

    // 1. Android APK
    tmp.write("app-debug.apk", "fake-apk-bytes-v1\n");
    assert_eq!(
        fingerprint_file(tmp.path(), Path::new("app-debug.apk")).expect("apk"),
        "a7640e3b1fc922f6ed5f2fbc6f5fcd0cf37e2804f5176db2d3b97e4769759b5f"
    );

    // 2. iOS .app tree
    tmp.write("Fixture.app/Info.plist", "<plist>one</plist>");
    tmp.write("Fixture.app/Fixture", "binary-one");
    tmp.write("Fixture.app/_CodeSignature/CodeResources", "signature-one");
    assert_eq!(
        fingerprint_tree(tmp.path(), Path::new("Fixture.app"), "iOS").expect("ios"),
        "05bfd91fb13244926bd54fbb909c2309b6b483103ea30ede476cd1e87122aa26"
    );

    // 3. Web static tree
    tmp.write("public/index.html", "<main>one</main>");
    tmp.write("public/assets/app.js", "const value = 1");
    assert_eq!(
        fingerprint_tree(tmp.path(), Path::new("public"), "Web").expect("web"),
        "e8203f92793c0bdfdba3e1b870ecd51aff622a02afd384a116f04a3ad1ec337e"
    );
}

#[test]
fn multi_chunk_streaming_over_256kib_matches_typescript_goldens() {
    let tmp = TempDir::new("multichunk");
    let big: Vec<u8> = (0..300_000_usize)
        .map(|i| ((i * 31 + 7) & 0xff) as u8)
        .collect();
    assert!(big.len() > 4 * FINGERPRINT_CHUNK_BYTES);

    tmp.write("big.apk", &big);
    assert_eq!(
        fingerprint_file(tmp.path(), Path::new("big.apk")).expect("big apk"),
        "1bb182f649a789fda17412a4839e9d5dd50b4068089bc334655f78e6db797370"
    );

    tmp.write("Big.app/Big.debug.dylib", &big);
    tmp.write("Big.app/Resources/manifest.txt", "hello\n");
    assert_eq!(
        fingerprint_tree(tmp.path(), Path::new("Big.app"), "iOS").expect("big tree"),
        "d0b7063baf776869861a47a15f6754b1e262d528e3548bba184fc0f62af21968"
    );
}

#[test]
fn fingerprint_tree_rejects_empty_trees_and_symlinks() {
    let tmp = TempDir::new("reject");
    fs::create_dir_all(tmp.path().join("empty")).expect("mkdir empty");
    let empty_err = fingerprint_tree(tmp.path(), Path::new("empty"), "Web").unwrap_err();
    assert!(matches!(empty_err, CoreError::Fingerprint(_)));
    assert!(empty_err.to_string().contains("at least one file"));

    #[cfg(unix)]
    {
        tmp.write("target.txt", "data");
        tmp.write("linked/real.txt", "ok");
        std::os::unix::fs::symlink(
            tmp.path().join("target.txt"),
            tmp.path().join("linked/sym.txt"),
        )
        .expect("symlink");
        let sym_err = fingerprint_tree(tmp.path(), Path::new("linked"), "iOS").unwrap_err();
        assert!(matches!(sym_err, CoreError::Fingerprint(_)));
        assert!(
            sym_err
                .to_string()
                .contains("symbolic links or special files")
        );
    }
}

#[test]
fn build_fingerprint_cache_hits_invalidates_and_fails_closed() {
    let tmp = TempDir::new("cache");
    tmp.write("app.apk", "apk-v1");
    tmp.write("App.app/Info.plist", "<plist>1</plist>");
    tmp.write("App.app/App", "bin-1");

    let mut cache = BuildFingerprintCache::new();

    // File cache miss then hit.
    let f1 = cache
        .fingerprint_file(tmp.path(), Path::new("app.apk"))
        .expect("first file");
    assert_eq!(cache.misses(), 1);
    assert_eq!(cache.hits(), 0);

    let f2 = cache
        .fingerprint_file(tmp.path(), Path::new("app.apk"))
        .expect("cached file");
    assert_eq!(f1, f2);
    #[cfg(unix)]
    {
        assert_eq!(cache.hits(), 1);
        assert_eq!(cache.misses(), 1);
    }
    #[cfg(not(unix))]
    {
        assert_eq!(cache.hits(), 0);
        assert_eq!(cache.misses(), 2);
    }

    // Replace file with different size -> cache invalidated.
    let misses_before_edit = cache.misses();
    tmp.write("app.apk", "apk-version-2");
    let f3 = cache
        .fingerprint_file(tmp.path(), Path::new("app.apk"))
        .expect("updated file");
    assert_ne!(f3, f1);
    assert_eq!(cache.misses(), misses_before_edit + 1);

    // Atomic rename replacement with SAME byte length (new inode!) -> cache invalidated.
    let misses_before_rename = cache.misses();
    tmp.write("app.apk.new", "apk-version-3");
    fs::rename(tmp.path().join("app.apk.new"), tmp.path().join("app.apk")).expect("rename");
    let f4 = cache
        .fingerprint_file(tmp.path(), Path::new("app.apk"))
        .expect("renamed file");
    assert_ne!(f4, f3);
    assert_eq!(cache.misses(), misses_before_rename + 1);

    // Same-inode, same-size in-place rewrite with restored mtime -> invalidated via Unix ctime!
    let apk_path = tmp.path().join("app.apk");
    let orig_mtime = fs::metadata(&apk_path)
        .expect("stat apk")
        .modified()
        .expect("mtime");
    let misses_before_inplace = cache.misses();
    thread::sleep(Duration::from_millis(20));
    {
        let mut f = OpenOptions::new()
            .write(true)
            .truncate(false)
            .open(&apk_path)
            .expect("open in-place");
        f.write_all(b"apk-version-4").expect("rewrite in-place");
        f.sync_all().expect("sync");
        f.set_modified(orig_mtime).expect("restore mtime");
    }
    let f5 = cache
        .fingerprint_file(tmp.path(), Path::new("app.apk"))
        .expect("in-place rewritten file");
    assert_ne!(f5, f4);
    assert_eq!(cache.misses(), misses_before_inplace + 1);

    // Tree cache miss then hit, then invalidation on added file.
    let t1 = cache
        .fingerprint_tree(tmp.path(), Path::new("App.app"), "iOS")
        .expect("tree 1");
    let misses_before = cache.misses();
    let hits_before = cache.hits();

    let t2 = cache
        .fingerprint_tree(tmp.path(), Path::new("App.app"), "iOS")
        .expect("tree 2");
    assert_eq!(t1, t2);
    #[cfg(unix)]
    {
        assert_eq!(cache.hits(), hits_before + 1);
        assert_eq!(cache.misses(), misses_before);
    }
    #[cfg(not(unix))]
    {
        assert_eq!(cache.hits(), hits_before);
        assert_eq!(cache.misses(), misses_before + 1);
    }

    let misses_before_add = cache.misses();
    tmp.write("App.app/Extra.txt", "added");
    let t3 = cache
        .fingerprint_tree(tmp.path(), Path::new("App.app"), "iOS")
        .expect("tree 3");
    assert_ne!(t3, t1);
    assert_eq!(cache.misses(), misses_before_add + 1);

    // Fail-closed: introducing a symlink into a previously cached tree must reject immediately.
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(tmp.path().join("app.apk"), tmp.path().join("App.app/link"))
            .expect("symlink");
        assert!(matches!(
            cache.fingerprint_tree(tmp.path(), Path::new("App.app"), "iOS"),
            Err(CoreError::Fingerprint(_))
        ));
    }

    cache.clear();
}
