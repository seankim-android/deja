//! Workspace-confined Maestro flow closure collection, hashing, layout planning, and stored subflow verification.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

use crate::canonical_json::cmp_utf16;
use crate::contracts::StoredFlowFile;
use crate::error::CoreError;
use crate::flow_references::{FlowReferenceCommand, FlowReferenceKind, find_flow_references};

/// Maximum number of files in one flow closure (including the main flow).
pub const MAX_FILES: usize = 64;

/// Maximum call nesting depth in a flow closure.
pub const MAX_DEPTH: usize = 16;

/// Maximum byte size for a single YAML flow or JavaScript file (1 MiB).
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// Maximum byte size for a single `addMedia` binary file (8 MiB).
pub const MAX_MEDIA_BYTES: u64 = 8 * 1024 * 1024;

/// Maximum total byte size across all `addMedia` files in one closure (32 MiB).
pub const MAX_TOTAL_MEDIA_BYTES: usize = 32 * 1024 * 1024;

/// One file gathered into a [`FlowClosure`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureFile {
    /// Path from the closure root, with `/` separators.
    pub path: String,
    pub sha256: String,
    pub bytes: Vec<u8>,
    pub kind: FlowReferenceKind,
    /// Workspace-relative source path, with `/` separators.
    pub source: String,
}

/// A flow and every subflow, script, and media file it transitively reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowClosure {
    /// Main flow path from the closure root, with `/` separators.
    pub main: String,
    /// Every file in the closure (main included), sorted by `path` in UTF-16 order.
    pub files: Vec<ClosureFile>,
    /// Closure checksum over every sorted `path` and file `sha256`.
    pub sha256: String,
}

/// Planned `.deja/flows` write layout for a collected closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredClosurePlan {
    pub flow_path: String,
    pub flow_sha256: String,
    pub writes: Vec<(String, Vec<u8>)>,
    pub subflow_files: Vec<StoredFlowFile>,
}

/// Result of verifying stored subflows on disk against walk metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredClosureCheck {
    Ok,
    Legacy,
    Missing { path: String },
    Invalid { message: String },
}

/// Path + checksum view used by [`closure_sha256_of`].
pub trait PathChecksum {
    fn entry_path(&self) -> &str;
    fn entry_sha256(&self) -> &str;
}

impl PathChecksum for ClosureFile {
    fn entry_path(&self) -> &str {
        &self.path
    }
    fn entry_sha256(&self) -> &str {
        &self.sha256
    }
}

impl PathChecksum for StoredFlowFile {
    fn entry_path(&self) -> &str {
        &self.path
    }
    fn entry_sha256(&self) -> &str {
        &self.sha256
    }
}

impl PathChecksum for (&str, &str) {
    fn entry_path(&self) -> &str {
        self.0
    }
    fn entry_sha256(&self) -> &str {
        self.1
    }
}

impl PathChecksum for (String, String) {
    fn entry_path(&self) -> &str {
        &self.0
    }
    fn entry_sha256(&self) -> &str {
        &self.1
    }
}

/// Streaming read chunk size (64 KiB) used for bounded stored-file verification.
const STREAM_CHUNK_BYTES: usize = 65_536;

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Stream a file's SHA-256 in 64 KiB chunks, returning `Ok(None)` without loading unbounded bytes
/// if the file size exceeds `limit` (checked both before and during streaming).
fn stream_sha256_bounded(path: &Path, limit: u64) -> Result<Option<String>, CoreError> {
    let mut file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > limit {
        return Ok(None);
    }
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; STREAM_CHUNK_BYTES];
    let mut total: u64 = 0;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total = total.saturating_add(n as u64);
        if total > limit {
            return Ok(None);
        }
        hasher.update(&buf[..n]);
    }
    Ok(Some(hex::encode(hasher.finalize())))
}

/// Read up to `limit` bytes from a regular file, returning `Ok(None)` without allocating unbounded
/// memory if the file exceeds `limit`.
fn read_bounded_bytes(path: &Path, limit: u64) -> Result<Option<Vec<u8>>, CoreError> {
    let file = File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.len() > limit {
        return Ok(None);
    }
    let initial_cap = usize::try_from(meta.len().min(limit)).unwrap_or(0);
    let mut bytes = Vec::with_capacity(initial_cap);
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    if (bytes.len() as u64) > limit {
        return Ok(None);
    }
    Ok(Some(bytes))
}

fn secret_ext_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)\.(?:pem|key|p12|pfx|jks|keystore|env)$").expect("valid secret ext regex")
    })
}

fn secret_prefix_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)^(?:id_rsa|id_ed25519|id_ecdsa|credentials)")
            .expect("valid secret prefix regex")
    })
}

fn windows_drive_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z]:[\\/]").expect("valid windows drive regex"))
}

fn sha256_pattern() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[a-f0-9]{64}$").expect("valid sha256 regex"))
}

/// Case-folded NFC spelling key used to reject macOS/Linux path aliasing collisions.
#[must_use]
pub fn spelling_key(path: &str) -> String {
    path.nfc().collect::<String>().to_lowercase()
}

fn secret_name(name: &str) -> bool {
    secret_ext_regex().is_match(name) || secret_prefix_regex().is_match(name)
}

fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(p) => out.push(p.as_os_str()),
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    // At root or empty relative path: leave root alone, preserve `..` if relative.
                    if !out.has_root() {
                        out.push("..");
                    }
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

fn inside(root: &Path, candidate: &Path) -> bool {
    candidate != root && candidate.starts_with(root)
}

fn posix_relative(from: &Path, to: &Path) -> String {
    if let Ok(rel) = to.strip_prefix(from) {
        return rel
            .components()
            .filter_map(|c| match c {
                Component::Normal(os) => Some(os.to_string_lossy().into_owned()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("/");
    }

    let from_parts: Vec<String> = from
        .components()
        .filter_map(|c| match c {
            Component::Normal(os) => Some(os.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let to_parts: Vec<String> = to
        .components()
        .filter_map(|c| match c {
            Component::Normal(os) => Some(os.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();

    let common = from_parts
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();

    let mut rel = Vec::new();
    for _ in common..from_parts.len() {
        rel.push("..".to_string());
    }
    rel.extend_from_slice(&to_parts[common..]);
    rel.join("/")
}

fn secret_looking(root: &Path, path: &Path, allowed_hidden: &HashSet<String>) -> bool {
    let rel = posix_relative(root, path);
    let segments: Vec<&str> = rel.split('/').collect();
    let has_disallowed_hidden = segments
        .iter()
        .any(|seg| seg.starts_with('.') && !allowed_hidden.contains(*seg));
    let has_secret_name = segments.last().is_some_and(|name| secret_name(name));
    has_disallowed_hidden || has_secret_name
}

fn common_directory(directories: &[PathBuf]) -> PathBuf {
    let mut common = directories[0].clone();
    for dir in &directories[1..] {
        while dir != &common && !dir.starts_with(&common) {
            match common.parent() {
                Some(parent) if parent != common => common = parent.to_path_buf(),
                _ => break,
            }
        }
    }
    common
}

/// Compute the closure checksum over sorted `(path, sha256)` pairs using UTF-16 path order.
#[must_use]
pub fn closure_sha256_of<T: PathChecksum>(files: &[T]) -> String {
    let mut sorted: Vec<&T> = files.iter().collect();
    sorted.sort_by(|left, right| cmp_utf16(left.entry_path(), right.entry_path()));
    let mut hasher = Sha256::new();
    for file in sorted {
        hasher.update(file.entry_path().as_bytes());
        hasher.update(b"\n");
        hasher.update(file.entry_sha256().as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(hasher.finalize())
}

struct ClosureCollector {
    root: PathBuf,
    main_path: PathBuf,
    allowed_hidden: HashSet<String>,
    found: HashMap<PathBuf, (Vec<u8>, FlowReferenceKind)>,
    found_order: Vec<PathBuf>,
    edges: HashMap<PathBuf, Vec<PathBuf>>,
    media_bytes: usize,
}

impl ClosureCollector {
    fn read_confined(&self, path: &Path, kind: FlowReferenceKind) -> Result<Vec<u8>, CoreError> {
        let canonical = match fs::canonicalize(path) {
            Ok(p) => p,
            Err(err) if err.kind() == ErrorKind::NotFound => {
                return Err(CoreError::MissingFlowFile {
                    path: posix_relative(&self.root, path),
                });
            }
            Err(err) => return Err(CoreError::Io(err)),
        };
        if !inside(&self.root, &canonical) {
            return Err(CoreError::FlowClosure(format!(
                "reference escapes the workspace: {}",
                posix_relative(&self.root, path)
            )));
        }
        if path != self.main_path
            && (secret_looking(&self.root, path, &self.allowed_hidden)
                || secret_looking(&self.root, &canonical, &self.allowed_hidden))
        {
            return Err(CoreError::FlowClosure(format!(
                "reference looks like a secret or hidden file and would be copied into .deja/flows: {}",
                posix_relative(&self.root, path)
            )));
        }
        let limit = match kind {
            FlowReferenceKind::Media => MAX_MEDIA_BYTES,
            FlowReferenceKind::Flow | FlowReferenceKind::Script => MAX_FILE_BYTES,
        };
        let Some(bytes) = read_bounded_bytes(&canonical, limit)? else {
            return Err(CoreError::FlowClosure(format!(
                "referenced path is not a regular file under {} MiB: {}",
                limit / 1024 / 1024,
                posix_relative(&self.root, path)
            )));
        };
        Ok(bytes)
    }

    fn visit(
        &mut self,
        path: PathBuf,
        kind: FlowReferenceKind,
        stack: &mut Vec<PathBuf>,
    ) -> Result<(), CoreError> {
        if stack.contains(&path) {
            return Err(CoreError::FlowClosure(format!(
                "flows run each other in a cycle: {}",
                posix_relative(&self.root, &path)
            )));
        }
        if stack.len() >= MAX_DEPTH {
            return Err(CoreError::FlowClosure(format!(
                "flows are nested more than {MAX_DEPTH} deep"
            )));
        }
        if let Some((_, earlier_kind)) = self.found.get(&path) {
            if *earlier_kind != kind {
                return Err(CoreError::FlowClosure(format!(
                    "one file is used as a {} and as a {}: {}",
                    earlier_kind.as_str(),
                    kind.as_str(),
                    posix_relative(&self.root, &path)
                )));
            }
            return Ok(());
        }

        let candidate_key = spelling_key(&posix_relative(&self.root, &path));
        if let Some(same_name) = self
            .found_order
            .iter()
            .find(|other| spelling_key(&posix_relative(&self.root, other)) == candidate_key)
        {
            return Err(CoreError::FlowClosure(format!(
                "one file is spelled two ways, differing only in case or Unicode normalization: {} and {}",
                posix_relative(&self.root, same_name),
                posix_relative(&self.root, &path)
            )));
        }

        let bytes = self.read_confined(&path, kind)?;
        if kind == FlowReferenceKind::Media {
            self.media_bytes = self.media_bytes.saturating_add(bytes.len());
            if self.media_bytes > MAX_TOTAL_MEDIA_BYTES {
                return Err(CoreError::FlowClosure(format!(
                    "media files add up to more than {} MiB",
                    MAX_TOTAL_MEDIA_BYTES / 1024 / 1024
                )));
            }
        }
        self.found.insert(path.clone(), (bytes, kind));
        self.found_order.push(path.clone());
        if self.found.len() > MAX_FILES {
            return Err(CoreError::FlowClosure(format!(
                "flow runs more than {MAX_FILES} files"
            )));
        }

        if kind != FlowReferenceKind::Flow {
            return Ok(());
        }

        let utf8 = String::from_utf8_lossy(&self.found[&path].0).into_owned();
        let references = match find_flow_references(&utf8) {
            Ok(refs) => refs,
            Err(err @ (CoreError::FlowReference(_) | CoreError::FlowFileCommand(_))) => {
                return Err(CoreError::FlowClosure(format!(
                    "{}: {err}",
                    posix_relative(&self.root, &path)
                )));
            }
            Err(other) => return Err(other),
        };

        let parent_dir = path
            .parent()
            .ok_or_else(|| CoreError::FlowClosure("flow has no parent directory".to_string()))?
            .to_path_buf();

        let current_path = path.clone();
        stack.push(path);
        for reference in references {
            if reference.path.contains('\0') {
                stack.pop();
                return Err(CoreError::FlowClosure(
                    "reference contains a NUL byte".to_string(),
                ));
            }
            if reference.path.contains('$') || reference.path.contains('{') {
                stack.pop();
                return Err(CoreError::FlowClosure(format!(
                    "reference uses a variable, so its file cannot be known: {}",
                    reference.path
                )));
            }
            if reference.path.starts_with('/')
                || windows_drive_regex().is_match(&reference.path)
                || reference.path.contains('\\')
            {
                stack.pop();
                return Err(CoreError::FlowClosure(format!(
                    "reference must be a relative path: {}",
                    reference.path
                )));
            }
            let target = lexical_normalize(&parent_dir.join(&reference.path));
            if !inside(&self.root, &target) {
                stack.pop();
                return Err(CoreError::FlowClosure(format!(
                    "reference escapes the workspace: {}",
                    reference.path
                )));
            }
            self.edges
                .entry(current_path.clone())
                .or_default()
                .push(target.clone());
            if let Err(err) = self.visit(target, reference.kind, stack) {
                stack.pop();
                return Err(err);
            }
        }
        stack.pop();
        Ok(())
    }

    fn validate_max_depth(&self, start: &Path) -> Result<(), CoreError> {
        fn longest_path_from<'a>(
            node: &'a Path,
            edges: &'a HashMap<PathBuf, Vec<PathBuf>>,
            memo: &mut HashMap<&'a Path, usize>,
            visiting: &mut HashSet<&'a Path>,
            root: &Path,
        ) -> Result<usize, CoreError> {
            if let Some(&depth) = memo.get(node) {
                return Ok(depth);
            }
            if !visiting.insert(node) {
                return Err(CoreError::FlowClosure(format!(
                    "flows run each other in a cycle: {}",
                    posix_relative(root, node)
                )));
            }
            let mut best = 0_usize;
            if let Some(children) = edges.get(node) {
                for child in children {
                    let sub = longest_path_from(child, edges, memo, visiting, root)?;
                    best = best.max(sub.saturating_add(1));
                }
            }
            visiting.remove(node);
            memo.insert(node, best);
            Ok(best)
        }

        let mut memo = HashMap::new();
        let mut visiting = HashSet::new();
        let deepest = longest_path_from(start, &self.edges, &mut memo, &mut visiting, &self.root)?;
        if deepest >= MAX_DEPTH {
            return Err(CoreError::FlowClosure(format!(
                "flows are nested more than {MAX_DEPTH} deep"
            )));
        }
        Ok(())
    }
}

/// Collect a flow and every file it transitively runs (`runFlow`, `retry`, `runScript`, `addMedia`),
/// enforcing lexical and canonical workspace confinement, secret/hidden path checks, and size/count caps.
pub fn collect_flow_closure(root: &Path, main: &Path) -> Result<FlowClosure, CoreError> {
    let canonical_root = fs::canonicalize(root)?;
    let initial_main = if main.is_relative() {
        lexical_normalize(&canonical_root.join(main))
    } else if let Ok(rel) = main.strip_prefix(root) {
        lexical_normalize(&canonical_root.join(rel))
    } else {
        lexical_normalize(main)
    };

    let rel_main = posix_relative(&canonical_root, &initial_main);
    let main_segments: Vec<&str> = rel_main.split('/').collect();
    let allowed_hidden: HashSet<String> = if main_segments.len() > 1 {
        main_segments[..main_segments.len() - 1]
            .iter()
            .filter(|seg| seg.starts_with('.') && **seg != "." && **seg != "..")
            .map(|seg| (*seg).to_string())
            .collect()
    } else {
        HashSet::new()
    };

    let mut collector = ClosureCollector {
        root: canonical_root.clone(),
        main_path: initial_main.clone(),
        allowed_hidden,
        found: HashMap::new(),
        found_order: Vec::new(),
        edges: HashMap::new(),
        media_bytes: 0,
    };

    let mut stack = Vec::new();
    collector.visit(initial_main.clone(), FlowReferenceKind::Flow, &mut stack)?;
    collector.validate_max_depth(&initial_main)?;

    let dirs: Vec<PathBuf> = collector
        .found_order
        .iter()
        .map(|p| p.parent().unwrap_or(&canonical_root).to_path_buf())
        .collect();
    let base = common_directory(&dirs);

    let mut files: Vec<ClosureFile> = collector
        .found
        .into_iter()
        .map(|(abs_path, (bytes, kind))| {
            let sha256 = sha256_hex(&bytes);
            ClosureFile {
                path: posix_relative(&base, &abs_path),
                sha256,
                bytes,
                kind,
                source: posix_relative(&canonical_root, &abs_path),
            }
        })
        .collect();
    files.sort_by(|left, right| cmp_utf16(&left.path, &right.path));

    let sha256 = closure_sha256_of(&files);
    Ok(FlowClosure {
        main: posix_relative(&base, &initial_main),
        files,
        sha256,
    })
}

/// Plan the `.deja/flows` storage layout for a collected closure.
///
/// Single-file flows retain the flat `.deja/flows/<goal>/<platform>-<sha>.yaml` layout;
/// multi-file closures live under `.deja/flows/<goal>/<platform>-<closureSha>/`.
#[must_use]
pub fn plan_stored_closure(
    closure: &FlowClosure,
    goal_slug: &str,
    platform_slug: &str,
) -> StoredClosurePlan {
    let main = closure
        .files
        .iter()
        .find(|file| file.path == closure.main)
        .expect("closure always contains its main flow");

    if closure.files.len() == 1 {
        let flow_path = format!(
            ".deja/flows/{goal_slug}/{platform_slug}-{}.yaml",
            main.sha256
        );
        return StoredClosurePlan {
            flow_path: flow_path.clone(),
            flow_sha256: main.sha256.clone(),
            writes: vec![(flow_path, main.bytes.clone())],
            subflow_files: Vec::new(),
        };
    }

    let folder = format!(".deja/flows/{goal_slug}/{platform_slug}-{}", closure.sha256);
    let mut writes = Vec::with_capacity(closure.files.len());
    let mut subflow_files = Vec::with_capacity(closure.files.len().saturating_sub(1));

    for file in &closure.files {
        let dest = format!("{folder}/{}", file.path);
        writes.push((dest.clone(), file.bytes.clone()));
        if file.path != closure.main {
            subflow_files.push(StoredFlowFile {
                path: dest,
                sha256: file.sha256.clone(),
            });
        }
    }

    StoredClosurePlan {
        flow_path: format!("{folder}/{}", closure.main),
        flow_sha256: main.sha256.clone(),
        writes,
        subflow_files,
    }
}

/// Validate raw JSON `subflowFiles` metadata, returning `None` if malformed or unsafe.
#[must_use]
pub fn read_stored_flow_files(value: &serde_json::Value) -> Option<Vec<StoredFlowFile>> {
    let entries = value.as_array()?;
    let mut files = Vec::with_capacity(entries.len());
    for entry in entries {
        let obj = entry.as_object()?;
        let path = obj.get("path")?.as_str()?;
        let checksum = obj.get("sha256")?.as_str()?;
        if !path.starts_with(".deja/flows/")
            || path.starts_with('/')
            || path.contains('\\')
            || path.split('/').any(|seg| seg == "..")
            || !sha256_pattern().is_match(checksum)
        {
            return None;
        }
        files.push(StoredFlowFile {
            path: path.to_string(),
            sha256: checksum.to_string(),
        });
    }
    Some(files)
}

/// Validate lexical confinement, non-symlink regular-file type, and canonical confinement within
/// `.deja/flows` before reading any stored flow or subflow file.
fn confine_stored_file(
    canonical_root: &Path,
    flow_root: &Path,
    rel_path: &str,
    label: &str,
    missing_is_legacy: bool,
) -> Result<Result<PathBuf, StoredClosureCheck>, CoreError> {
    if Path::new(rel_path).is_absolute() || rel_path.contains('\\') {
        return Ok(Err(StoredClosureCheck::Invalid {
            message: format!("stored {label} must stay under .deja/flows: {rel_path}"),
        }));
    }
    let candidate = lexical_normalize(&canonical_root.join(rel_path));
    if !inside(flow_root, &candidate) {
        return Ok(Err(StoredClosureCheck::Invalid {
            message: format!("stored {label} must stay under .deja/flows: {rel_path}"),
        }));
    }
    let lmeta = match fs::symlink_metadata(&candidate) {
        Ok(meta) => meta,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Ok(Err(if missing_is_legacy {
                StoredClosureCheck::Legacy
            } else {
                StoredClosureCheck::Missing {
                    path: rel_path.to_string(),
                }
            }));
        }
        Err(_) => {
            return Ok(Err(StoredClosureCheck::Invalid {
                message: format!("stored {label} cannot be resolved: {rel_path}"),
            }));
        }
    };
    if lmeta.file_type().is_symlink() || !lmeta.file_type().is_file() {
        return Ok(Err(StoredClosureCheck::Invalid {
            message: format!("stored {label} must be a regular file (not a symlink): {rel_path}"),
        }));
    }
    let canonical = match fs::canonicalize(&candidate) {
        Ok(p) => p,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Ok(Err(if missing_is_legacy {
                StoredClosureCheck::Legacy
            } else {
                StoredClosureCheck::Missing {
                    path: rel_path.to_string(),
                }
            }));
        }
        Err(_) => {
            return Ok(Err(StoredClosureCheck::Invalid {
                message: format!("stored {label} cannot be resolved: {rel_path}"),
            }));
        }
    };
    let canonical_flow_root = match fs::canonicalize(flow_root) {
        Ok(p) => p,
        Err(_) => {
            return Ok(Err(StoredClosureCheck::Invalid {
                message: format!("stored {label} cannot be resolved: {rel_path}"),
            }));
        }
    };
    let is_file = fs::metadata(&canonical).is_ok_and(|m| m.is_file());
    if !inside(&canonical_flow_root, &canonical) || !is_file {
        return Ok(Err(StoredClosureCheck::Invalid {
            message: format!("canonical stored {label} escapes .deja/flows: {rel_path}"),
        }));
    }
    Ok(Ok(canonical))
}

/// Verify that every stored subflow exists under `.deja/flows`, matches its checksum,
/// and covers every reference read by the stored YAML files.
pub fn check_stored_subflows(
    root: &Path,
    flow_path: &str,
    subflow_files: Option<&[StoredFlowFile]>,
    legacy_closure: bool,
) -> Result<StoredClosureCheck, CoreError> {
    let canonical_root = fs::canonicalize(root)?;
    let flow_root = lexical_normalize(&canonical_root.join(".deja/flows"));

    let Some(subflows) = subflow_files.filter(|_| !legacy_closure) else {
        let main_canonical =
            match confine_stored_file(&canonical_root, &flow_root, flow_path, "flow", true)? {
                Ok(path) => path,
                Err(status) => return Ok(status),
            };
        let Some(bytes) = read_bounded_bytes(&main_canonical, MAX_FILE_BYTES)? else {
            return Ok(StoredClosureCheck::Invalid {
                message: format!(
                    "stored flow exceeds {} MiB limit: {flow_path}",
                    MAX_FILE_BYTES / 1024 / 1024
                ),
            });
        };
        let contents = String::from_utf8_lossy(&bytes);
        return match find_flow_references(&contents) {
            Ok(refs) if !refs.is_empty() => Ok(StoredClosureCheck::Legacy),
            Ok(_) => Ok(StoredClosureCheck::Ok),
            Err(CoreError::FlowReference(_) | CoreError::FlowFileCommand(_)) => {
                Ok(StoredClosureCheck::Legacy)
            }
            Err(other) => Err(other),
        };
    };

    let main_canonical =
        match confine_stored_file(&canonical_root, &flow_root, flow_path, "flow", false)? {
            Ok(path) => path,
            Err(status) => return Ok(status),
        };

    let mut canonical_by_path: HashMap<String, PathBuf> =
        HashMap::with_capacity(subflows.len() + 1);
    canonical_by_path.insert(flow_path.to_string(), main_canonical);

    for file in subflows {
        let canonical =
            match confine_stored_file(&canonical_root, &flow_root, &file.path, "subflow", false)? {
                Ok(path) => path,
                Err(status) => return Ok(status),
            };
        let Some(digest) = stream_sha256_bounded(&canonical, MAX_MEDIA_BYTES)? else {
            return Ok(StoredClosureCheck::Invalid {
                message: format!(
                    "stored subflow exceeds {} MiB limit: {}",
                    MAX_MEDIA_BYTES / 1024 / 1024,
                    file.path
                ),
            });
        };
        if digest != file.sha256 {
            return Ok(StoredClosureCheck::Invalid {
                message: format!("stored subflow checksum does not match {}", file.path),
            });
        }
        canonical_by_path.insert(file.path.clone(), canonical);
    }

    let listed: HashSet<&str> = subflows.iter().map(|f| f.path.as_str()).collect();
    // Walk the actual reference graph starting from the main flow so extensionless subflows are
    // always inspected as YAML, while binary media or scripts (even with a `.yaml` name) are never
    // parsed as YAML.
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut visited_flows: HashSet<String> = HashSet::new();
    let mut seen_kinds: HashMap<String, FlowReferenceKind> = HashMap::new();
    queue.push_back(flow_path.to_string());
    visited_flows.insert(flow_path.to_string());
    seen_kinds.insert(flow_path.to_string(), FlowReferenceKind::Flow);

    while let Some(path) = queue.pop_front() {
        let canonical = match canonical_by_path.get(&path) {
            Some(p) => p.clone(),
            None => match confine_stored_file(&canonical_root, &flow_root, &path, "flow", false)? {
                Ok(p) => p,
                Err(status) => return Ok(status),
            },
        };
        let Some(bytes) = read_bounded_bytes(&canonical, MAX_FILE_BYTES)? else {
            return Ok(StoredClosureCheck::Invalid {
                message: format!(
                    "stored flow exceeds {} MiB limit: {path}",
                    MAX_FILE_BYTES / 1024 / 1024
                ),
            });
        };
        let contents = String::from_utf8_lossy(&bytes);
        let references = match find_flow_references(&contents) {
            Ok(refs) => refs,
            Err(CoreError::FlowFileCommand(_)) => return Ok(StoredClosureCheck::Legacy),
            Err(CoreError::FlowReference(msg)) => {
                return Ok(StoredClosureCheck::Invalid {
                    message: format!("stored flow {path} cannot be read for references: {msg}"),
                });
            }
            Err(other) => return Err(other),
        };
        let absolute = lexical_normalize(&canonical_root.join(&path));
        let parent = absolute.parent().unwrap_or(&canonical_root);
        for reference in references {
            let target = posix_relative(
                &canonical_root,
                &lexical_normalize(&parent.join(&reference.path)),
            );
            if !listed.contains(target.as_str()) {
                if matches!(
                    reference.command,
                    FlowReferenceCommand::AddMedia | FlowReferenceCommand::Retry
                ) {
                    return Ok(StoredClosureCheck::Legacy);
                }
                return Ok(StoredClosureCheck::Invalid {
                    message: format!(
                        "stored flow {path} runs {}, which the walk metadata does not list",
                        reference.path
                    ),
                });
            }
            if let Some(&earlier_kind) = seen_kinds.get(&target) {
                if earlier_kind != reference.kind {
                    return Ok(StoredClosureCheck::Invalid {
                        message: format!(
                            "stored flow {path} uses {target} as a {} and as a {}",
                            earlier_kind.as_str(),
                            reference.kind.as_str()
                        ),
                    });
                }
            } else {
                seen_kinds.insert(target.clone(), reference.kind);
            }
            if reference.kind == FlowReferenceKind::Flow && visited_flows.insert(target.clone()) {
                queue.push_back(target);
            }
        }
    }

    Ok(StoredClosureCheck::Ok)
}
