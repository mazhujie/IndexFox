//! Directory enumeration with format/size limits and conservative failure
//! reporting. Deletion is only allowed by callers after `complete == true`.

use std::collections::HashSet;
use std::fs::Metadata;
use std::path::{Component, Path, PathBuf};
use std::time::UNIX_EPOCH;

use walkdir::{DirEntry, WalkDir};

pub use indexfox_core::SourceKind;

#[derive(Debug, Clone)]
pub struct ScanOptions {
    pub extensions: Vec<String>,
    /// Ignored directory names. Entries containing `*` or `?` are simple glob
    /// patterns matched against each directory name (`*` matches any sequence,
    /// `?` matches one character); all other entries keep the original exact
    /// name behaviour.
    pub excluded_dirs: Vec<String>,
    /// Independent exclusion globs. Patterns without `/` or `\` match file
    /// and directory names; patterns containing either separator match the
    /// normalised path relative to the source root. `*` matches any sequence
    /// and `?` exactly one character.
    pub exclude_globs: Vec<String>,
    pub max_file_size_bytes: u64,
    /// `None` walks the whole tree, `Some(0)` only visits the root, and
    /// `Some(1)` is the non-recursive policy (direct children only).
    pub max_depth: Option<usize>,
}

impl Default for ScanOptions {
    fn default() -> Self {
        let config = indexfox_core::AppConfig::default();
        Self {
            extensions: config.text_extensions,
            excluded_dirs: config.excluded_dirs,
            exclude_globs: Vec::new(),
            max_file_size_bytes: config.max_file_size_bytes,
            max_depth: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    pub path: PathBuf,
    pub display_path: String,
    pub path_key: String,
    pub filename: String,
    pub ext: String,
    pub size_bytes: u64,
    pub mtime_ns: Option<i64>,
    /// Oversize files are metadata-only (`skipped_size`).
    pub oversize: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanIssue {
    pub path: String,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct ScanOutcome {
    pub files: Vec<ScannedFile>,
    pub issues: Vec<ScanIssue>,
    /// False means at least one part of the tree could not be enumerated; the
    /// caller must not reconcile (delete) anything under this source.
    pub complete: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ScanError {
    #[error("source root is not an accessible directory: {0}")]
    Unavailable(String),
    #[error("path must be relative to the source root: {0}")]
    OutsideRoot(String),
    #[error("path is excluded by directory ignore rules: {0}")]
    Excluded(String),
    #[error("path is not a regular file (symlinks are not followed): {0}")]
    NotAFile(String),
    #[error("file type is not in the index whitelist: {0}")]
    UnsupportedExtension(String),
    #[error("cannot inspect {path}: {message}")]
    Io { path: String, message: String },
    #[error("scan cancelled")]
    Cancelled,
}

pub type Result<T, E = ScanError> = std::result::Result<T, E>;

/// Scan `root` with the default, non-cancellable policy.
///
/// The caller may only reconcile (delete) records when the returned outcome
/// has `complete == true`.
pub fn scan_root(root: &Path, options: &ScanOptions) -> Result<ScanOutcome> {
    scan_root_controlled(root, options, &|| false)
}

/// Scan `root`, returning [`ScanError::Cancelled`] as soon as `should_stop`
/// reports true. A cancelled scan produces no outcome, so the caller must not
/// reconcile (delete) anything for it.
pub fn scan_root_controlled(
    root: &Path,
    options: &ScanOptions,
    should_stop: &dyn Fn() -> bool,
) -> Result<ScanOutcome> {
    if should_stop() {
        return Err(ScanError::Cancelled);
    }

    // Absolute paths keep path_key stable when the same tree is indexed as
    // `.`, a relative path, or an absolute path.
    let root = absolute_path(root)
        .map_err(|error| ScanError::Unavailable(format!("{}: {error}", root.display())))?;
    let root = root.as_path();
    let metadata = std::fs::metadata(root)
        .map_err(|error| ScanError::Unavailable(format!("{}: {error}", root.display())))?;
    if !metadata.is_dir() {
        return Err(ScanError::Unavailable(format!(
            "{} is not a directory",
            root.display()
        )));
    }
    let root_identity = root_identity(&metadata);

    let allowed: HashSet<String> = options
        .extensions
        .iter()
        .map(|e| e.to_ascii_lowercase())
        .collect();
    let excluded = ExcludedPatterns::new(&options.excluded_dirs);
    let exclude_globs = ExcludeGlobs::new(&options.exclude_globs);
    let glob_case_insensitive = exclude_glob_case_insensitive(root);

    let mut outcome = ScanOutcome {
        complete: true,
        ..ScanOutcome::default()
    };

    let walker = WalkDir::new(root)
        .follow_links(false)
        .max_depth(options.max_depth.unwrap_or(usize::MAX))
        .into_iter()
        .filter_entry(|entry| {
            !should_skip_entry(
                entry,
                root,
                &excluded,
                &exclude_globs,
                glob_case_insensitive,
            )
        });

    for entry in walker {
        if should_stop() {
            return Err(ScanError::Cancelled);
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                outcome.complete = false;
                outcome.issues.push(ScanIssue {
                    path: error
                        .path()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                    message: error.to_string(),
                });
                continue;
            }
        };

        // Symlinks, junctions and other reparse points are never followed.
        if !entry.file_type().is_file() || entry_is_link(&entry) {
            continue;
        }

        let path = entry.path();
        let (filename, ext) = file_identity(path);
        let relative = relative_path_text(root, path).unwrap_or_default();
        if exclude_globs.matches(&filename, &relative, glob_case_insensitive) {
            continue;
        }
        if !allowed.contains(&ext) {
            continue;
        }

        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                outcome.complete = false;
                outcome.issues.push(ScanIssue {
                    path: path.display().to_string(),
                    message: error.to_string(),
                });
                continue;
            }
        };
        let size_bytes = metadata.len();
        let mtime_ns = mtime_ns(&metadata);

        let display_path = path.display().to_string();
        outcome.files.push(ScannedFile {
            path: path.to_path_buf(),
            path_key: path_key(path),
            display_path,
            filename,
            ext,
            size_bytes,
            mtime_ns,
            oversize: size_bytes > options.max_file_size_bytes,
        });
    }

    // A root that disappeared or was replaced while we walked means the
    // enumeration is not trustworthy.
    if let Some(issue) = root_change_issue(root, root_identity) {
        outcome.complete = false;
        outcome.issues.push(issue);
    }

    outcome
        .files
        .sort_by(|left, right| left.path_key.cmp(&right.path_key));
    Ok(outcome)
}

/// Inspect exactly one path below `root` with the same whitelist, ignore and
/// symlink rules as `scan_root`. Intended for targeted single-file indexing:
/// it never enumerates the directory, so no reconcile/deletion is implied.
pub fn scan_file(root: &Path, relative: &Path, options: &ScanOptions) -> Result<ScannedFile> {
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(ScanError::OutsideRoot(relative.display().to_string()));
    }
    let root = absolute_path(root)
        .map_err(|error| ScanError::Unavailable(format!("{}: {error}", root.display())))?;
    let root_metadata = std::fs::metadata(&root)
        .map_err(|error| ScanError::Unavailable(format!("{}: {error}", root.display())))?;
    if !root_metadata.is_dir() {
        return Err(ScanError::Unavailable(format!(
            "{} is not a directory",
            root.display()
        )));
    }

    let candidate = absolute_path(&root.join(relative)).map_err(|error| ScanError::Io {
        path: relative.display().to_string(),
        message: error.to_string(),
    })?;
    if !candidate.starts_with(&root) {
        return Err(ScanError::OutsideRoot(relative.display().to_string()));
    }
    let relative = candidate.strip_prefix(&root).expect("prefix checked");
    let components: Vec<_> = relative.components().collect();
    if components.is_empty() {
        return Err(ScanError::NotAFile(root.display().to_string()));
    }

    let excluded = ExcludedPatterns::new(&options.excluded_dirs);
    for component in &components[..components.len() - 1] {
        let name = component.as_os_str().to_string_lossy();
        if excluded.matches(&name, cfg!(windows)) {
            return Err(ScanError::Excluded(relative.display().to_string()));
        }
    }

    let exclude_globs = ExcludeGlobs::new(&options.exclude_globs);
    let glob_case_insensitive = exclude_glob_case_insensitive(&root);
    let mut relative_text = String::new();
    for component in &components {
        let name = component.as_os_str().to_string_lossy();
        if !relative_text.is_empty() {
            relative_text.push('/');
        }
        relative_text.push_str(&name);
        if exclude_globs.matches(&name, &relative_text, glob_case_insensitive) {
            return Err(ScanError::Excluded(relative.display().to_string()));
        }
    }

    // Reject links anywhere below the root; scan_root never follows them, so a
    // targeted path must not sneak content in (or out) through a link.
    let mut current = root.clone();
    let mut target_metadata = None;
    for (index, component) in components.iter().enumerate() {
        current.push(component.as_os_str());
        let metadata = std::fs::symlink_metadata(&current).map_err(|error| ScanError::Io {
            path: current.display().to_string(),
            message: error.to_string(),
        })?;
        if metadata_is_link(&metadata) {
            return Err(ScanError::NotAFile(current.display().to_string()));
        }
        if index + 1 == components.len() {
            if !metadata.is_file() {
                return Err(ScanError::NotAFile(current.display().to_string()));
            }
            target_metadata = Some(metadata);
        } else if !metadata.is_dir() {
            return Err(ScanError::NotAFile(current.display().to_string()));
        }
    }
    let metadata = target_metadata.expect("file component visited");

    let (filename, ext) = file_identity(&candidate);
    let allowed: std::collections::HashSet<String> = options
        .extensions
        .iter()
        .map(|extension| extension.to_ascii_lowercase())
        .collect();
    if !allowed.contains(&ext) {
        return Err(ScanError::UnsupportedExtension(
            candidate.display().to_string(),
        ));
    }

    let size_bytes = metadata.len();
    Ok(ScannedFile {
        path: candidate.clone(),
        display_path: candidate.display().to_string(),
        path_key: path_key(&candidate),
        filename,
        ext,
        size_bytes,
        mtime_ns: mtime_ns(&metadata),
        oversize: size_bytes > options.max_file_size_bytes,
    })
}

fn file_identity(path: &Path) -> (String, String) {
    let declared_ext = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();
    // Makefile / Dockerfile / .gitignore style names have no extension.
    let ext = if declared_ext.is_empty() {
        filename.trim_start_matches('.').to_ascii_lowercase()
    } else {
        declared_ext
    };
    (filename, ext)
}

fn mtime_ns(metadata: &std::fs::Metadata) -> Option<i64> {
    metadata.modified().ok().and_then(|time| {
        time.duration_since(UNIX_EPOCH)
            .ok()
            .map(|duration| duration.as_nanos().min(i64::MAX as u128) as i64)
    })
}

struct ExcludedPatterns<'a> {
    exact: HashSet<&'a str>,
    globs: Vec<&'a str>,
}

impl<'a> ExcludedPatterns<'a> {
    fn new(patterns: &'a [String]) -> Self {
        let mut exact = HashSet::new();
        let mut globs = Vec::new();
        for pattern in patterns {
            if pattern.contains('*') || pattern.contains('?') {
                globs.push(pattern.as_str());
            } else {
                exact.insert(pattern.as_str());
            }
        }
        Self { exact, globs }
    }

    fn matches(&self, name: &str, case_insensitive: bool) -> bool {
        if case_insensitive {
            self.exact
                .iter()
                .any(|pattern| pattern.eq_ignore_ascii_case(name))
                || self
                    .globs
                    .iter()
                    .any(|pattern| glob_match(pattern, name, true))
        } else {
            self.exact.contains(name)
                || self
                    .globs
                    .iter()
                    .any(|pattern| glob_match(pattern, name, false))
        }
    }
}

struct ExcludeGlobs<'a> {
    names: Vec<&'a str>,
    paths: Vec<String>,
}

impl<'a> ExcludeGlobs<'a> {
    fn new(patterns: &'a [String]) -> Self {
        let mut names = Vec::new();
        let mut paths = Vec::new();
        for pattern in patterns {
            if pattern.contains('/') || pattern.contains('\\') {
                paths.push(pattern.replace('\\', "/"));
            } else {
                names.push(pattern.as_str());
            }
        }
        Self { names, paths }
    }

    fn matches(&self, name: &str, relative: &str, case_insensitive: bool) -> bool {
        self.names
            .iter()
            .any(|pattern| glob_match(pattern, name, case_insensitive))
            || self
                .paths
                .iter()
                .any(|pattern| glob_match(pattern, relative, case_insensitive))
    }
}

/// Windows/SMB sources fold case; WSL and POSIX paths keep it. The source root
/// decides the policy because a Windows host can index a case-sensitive WSL
/// tree.
fn exclude_glob_case_insensitive(root: &Path) -> bool {
    match source_kind(root) {
        SourceKind::Wsl => false,
        SourceKind::Smb => true,
        SourceKind::Local | SourceKind::Removable => cfg!(windows),
    }
}

fn relative_path_text(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?;
    let mut text = String::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => {
                if !text.is_empty() {
                    text.push('/');
                }
                text.push_str(&part.to_string_lossy());
            }
            Component::CurDir => {}
            _ => return None,
        }
    }
    Some(text)
}

/// Minimal glob: `*` matches any sequence (including empty) and `?` matches
/// exactly one character. No escaping or character classes; path separators
/// are ordinary characters.
fn glob_match(pattern: &str, text: &str, case_insensitive: bool) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star = None;
    let mut star_text = 0;

    while ti < text.len() {
        if pi < pattern.len()
            && (pattern[pi] == '?' || chars_equal(pattern[pi], text[ti], case_insensitive))
        {
            pi += 1;
            ti += 1;
        } else if pi < pattern.len() && pattern[pi] == '*' {
            star = Some(pi);
            star_text = ti;
            pi += 1;
        } else if let Some(star_pi) = star {
            star_text += 1;
            pi = star_pi + 1;
            ti = star_text;
        } else {
            return false;
        }
    }
    while pi < pattern.len() && pattern[pi] == '*' {
        pi += 1;
    }
    pi == pattern.len()
}

fn chars_equal(left: char, right: char, case_insensitive: bool) -> bool {
    if case_insensitive {
        left.eq_ignore_ascii_case(&right)
    } else {
        left == right
    }
}

fn should_skip_entry(
    entry: &DirEntry,
    root: &Path,
    excluded: &ExcludedPatterns<'_>,
    exclude_globs: &ExcludeGlobs<'_>,
    glob_case_insensitive: bool,
) -> bool {
    if entry.depth() == 0 || !entry.file_type().is_dir() {
        return false;
    }
    let name = entry.file_name().to_string_lossy();
    if excluded.matches(&name, false) {
        return true;
    }
    let relative = relative_path_text(root, entry.path()).unwrap_or_default();
    if exclude_globs.matches(&name, &relative, glob_case_insensitive) {
        return true;
    }
    entry.file_type().is_symlink() || path_is_reparse(entry.path())
}

fn entry_is_link(entry: &DirEntry) -> bool {
    entry.file_type().is_symlink() || path_is_reparse(entry.path())
}

fn metadata_is_link(metadata: &Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        metadata.file_type().is_symlink()
            || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

/// On Windows a directory junction is not always reported as a symlink by the
/// standard library, so inspect the reparse-point attribute as well. Other
/// platforms have no reparse points.
fn path_is_reparse(path: &Path) -> bool {
    #[cfg(windows)]
    {
        std::fs::symlink_metadata(path)
            .map(|metadata| metadata_is_link(&metadata))
            .unwrap_or(false)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

fn root_identity(metadata: &Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        // Creation time is the stable identity available on stable Rust. The
        // volume/file-index API is still unstable.
        Some((metadata.creation_time(), 0))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        None
    }
}

fn root_change_issue(root: &Path, expected: Option<(u64, u64)>) -> Option<ScanIssue> {
    let path = root.display().to_string();
    let metadata = match std::fs::metadata(root) {
        Ok(metadata) => metadata,
        Err(error) => {
            return Some(ScanIssue {
                path,
                message: format!("source root became inaccessible during scan: {error}"),
            })
        }
    };
    if !metadata.is_dir() {
        return Some(ScanIssue {
            path,
            message: "source root is no longer a directory".into(),
        });
    }
    let current = root_identity(&metadata);
    if current != expected {
        return Some(ScanIssue {
            path,
            message: "source root identity changed during scan".into(),
        });
    }
    None
}

/// Make a path absolute and lexically remove `.` components without
/// resolving symlinks. This keeps `path_key` stable for `.`, relative and
/// absolute spellings of the same root.
pub fn absolute_path(path: &Path) -> std::io::Result<PathBuf> {
    let absolute = std::path::absolute(path)?;
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        if !matches!(component, Component::CurDir) {
            normalized.push(component.as_os_str());
        }
    }
    Ok(normalized)
}

/// Classify a path by the source it represents.
///
/// WSL shares (`\\wsl.localhost\...` and `\\wsl$\...`) are [`SourceKind::Wsl`];
/// other UNC paths are [`SourceKind::Smb`]. On Windows, drive letters are
/// queried for removable/network media; a mapped network drive is `Smb`.
pub fn source_kind(path: &Path) -> SourceKind {
    let text = path.to_string_lossy();
    if let Some(kind) = unc_source_kind(&text) {
        return kind;
    }
    #[cfg(windows)]
    if let Some(letter) = windows_drive_letter(&text) {
        return drive_source_kind(letter);
    }
    SourceKind::Local
}

fn unc_source_kind(text: &str) -> Option<SourceKind> {
    let host = unc_host(text)?;
    if is_wsl_host(host) {
        Some(SourceKind::Wsl)
    } else {
        Some(SourceKind::Smb)
    }
}

fn is_wsl_unc(path: &Path) -> bool {
    let text = path.to_string_lossy();
    unc_host(&text).is_some_and(is_wsl_host)
}

fn unc_host(text: &str) -> Option<&str> {
    unc_rest(text)?
        .split(['\\', '/'])
        .find(|component| !component.is_empty())
}

fn is_wsl_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("wsl.localhost") || host.eq_ignore_ascii_case("wsl$")
}

fn unc_rest(text: &str) -> Option<&str> {
    let rest = text
        .strip_prefix("\\\\")
        .or_else(|| text.strip_prefix("//"))?;
    // Extended-length UNC: \\?\UNC\server\share
    if let Some(extended) = rest.strip_prefix("?\\").or_else(|| rest.strip_prefix("?/")) {
        let bytes = extended.as_bytes();
        if bytes.len() >= 4
            && extended
                .get(..3)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("UNC"))
            && matches!(bytes[3], b'\\' | b'/')
        {
            return Some(&extended[4..]);
        }
        // Device or extended drive path (\\?\C:\...), not a share.
        return None;
    }
    // Device namespace (\\.\...) is not a share.
    if rest.starts_with(".\\") || rest.starts_with("./") {
        return None;
    }
    Some(rest)
}

#[cfg(windows)]
fn windows_drive_letter(text: &str) -> Option<u8> {
    let bytes = text.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Some(bytes[0]);
    }
    let rest = text
        .strip_prefix("\\\\?\\")
        .or_else(|| text.strip_prefix("//?/"))?;
    let bytes = rest.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':').then(|| bytes[0])
}

#[cfg(windows)]
fn drive_source_kind(letter: u8) -> SourceKind {
    const DRIVE_REMOVABLE: u32 = 2;
    const DRIVE_REMOTE: u32 = 4;
    let root = [
        letter.to_ascii_uppercase() as u16,
        u16::from(b':'),
        u16::from(b'\\'),
        0,
    ];
    // GetDriveTypeW only inspects the volume root; it does not enumerate or
    // open the directory being indexed.
    match unsafe { GetDriveTypeW(root.as_ptr()) } {
        DRIVE_REMOVABLE => SourceKind::Removable,
        DRIVE_REMOTE => SourceKind::Smb,
        _ => SourceKind::Local,
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetDriveTypeW(root_path_name: *const u16) -> u32;
}

/// Windows paths are compared case-insensitively with lowercased text, except
/// WSL UNC paths, which keep Linux case sensitivity. POSIX paths keep their
/// case and separators.
pub fn path_key(path: &Path) -> String {
    path_key_with_policy(path, cfg!(windows))
}

fn path_key_with_policy(path: &Path, windows_case_fold: bool) -> String {
    let text = path.to_string_lossy();
    if windows_case_fold && !is_wsl_unc(path) {
        text.to_lowercase()
    } else {
        text.into_owned()
    }
}

/// Comparison key for source roots and files. Like [`path_key`], but also
/// makes the path absolute (lexically) and normalises Windows separators to
/// `\`. It never touches the filesystem, so an unavailable/offline root still
/// yields a stable key for overlap checks.
pub fn canonical_path_key(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut key = if is_windows_style_path(&text) {
        // `std::path::absolute` would treat a UNC path as relative on POSIX,
        // so leave Windows-style paths in their own namespace.
        text.replace('/', "\\")
    } else {
        let absolute = absolute_path(path).unwrap_or_else(|_| path.to_path_buf());
        absolute.to_string_lossy().into_owned()
    };
    if cfg!(windows) && !is_wsl_unc(path) {
        key = key.to_lowercase();
    }
    trim_trailing_separators(&mut key);
    key
}

/// True when `candidate` is `root` or lives below it. Uses
/// [`canonical_path_key`], so Windows/SMB roots compare case-insensitively on
/// Windows while WSL and POSIX roots stay case-sensitive.
pub fn path_within(root: &Path, candidate: &Path) -> bool {
    let root_key = canonical_path_key(root);
    let candidate_key = canonical_path_key(candidate);
    if root_key.is_empty() || candidate_key.is_empty() {
        return root_key == candidate_key;
    }
    if root_key == candidate_key {
        return true;
    }
    let separator = if root_key.contains('\\') && !root_key.contains('/') {
        '\\'
    } else {
        '/'
    };
    let mut prefix = root_key;
    if !prefix.ends_with(separator) {
        prefix.push(separator);
    }
    candidate_key.starts_with(&prefix)
}

/// Roots overlap when either one contains the other. Used by the pipeline to
/// avoid indexing the same logical tree through two sources.
pub fn paths_overlap(left: &Path, right: &Path) -> bool {
    path_within(left, right) || path_within(right, left)
}

fn is_windows_style_path(text: &str) -> bool {
    let bytes = text.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':')
        || text.starts_with("\\\\")
        || text.starts_with("//")
}

fn trim_trailing_separators(key: &mut String) {
    while key.len() > 1 {
        let last = key.as_bytes()[key.len() - 1];
        if last != b'/' && last != b'\\' {
            break;
        }
        // Keep drive roots such as `C:\` and the POSIX root `/`.
        if key.len() == 3 && key.as_bytes()[1] == b':' {
            break;
        }
        key.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let sequence = NEXT_TEMP.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "indexfox-source-{tag}-{}-{sequence}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn txt_options() -> ScanOptions {
        ScanOptions {
            extensions: vec!["txt".into(), "md".into(), "dockerfile".into()],
            excluded_dirs: Vec::new(),
            exclude_globs: Vec::new(),
            max_file_size_bytes: 1024,
            max_depth: None,
        }
    }

    fn filenames(outcome: &ScanOutcome) -> Vec<&str> {
        outcome
            .files
            .iter()
            .map(|file| file.filename.as_str())
            .collect()
    }

    #[test]
    fn absolute_path_removes_dot_components() {
        let cwd = std::env::current_dir().unwrap();
        let dotted = cwd.join(".").join("sub");
        let normalized = absolute_path(&dotted).unwrap();
        assert_eq!(normalized, cwd.join("sub"));
    }

    #[test]
    fn scans_whitelisted_files_and_marks_oversize() {
        let dir = temp_dir("scan");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("a.txt"), b"hello").unwrap();
        fs::write(dir.join("b.bin"), b"nope").unwrap();
        fs::write(dir.join("Dockerfile"), b"FROM scratch\n").unwrap();
        fs::write(dir.join("sub/c.md"), b"big").unwrap();
        fs::create_dir_all(dir.join("node_modules")).unwrap();
        fs::write(dir.join("node_modules/d.txt"), b"ignored").unwrap();

        let options = ScanOptions {
            extensions: vec!["txt".into(), "md".into(), "dockerfile".into()],
            excluded_dirs: vec!["node_modules".into()],
            exclude_globs: Vec::new(),
            max_file_size_bytes: 4,
            max_depth: None,
        };
        let outcome = scan_root(&dir, &options).unwrap();
        let names: Vec<_> = outcome
            .files
            .iter()
            .map(|f| (f.filename.as_str(), f.oversize))
            .collect();
        assert_eq!(
            names,
            vec![("Dockerfile", true), ("a.txt", true), ("c.md", false)]
        );
        assert!(outcome.complete);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn wildcard_exclusions_match_directory_names_only() {
        let dir = temp_dir("glob");
        for name in ["keep", "skip.cache", "tmp1", "tmp10", "exact"] {
            fs::create_dir_all(dir.join(name)).unwrap();
            fs::write(dir.join(name).join("file.txt"), b"x").unwrap();
        }
        let options = ScanOptions {
            excluded_dirs: vec!["*.cache".into(), "tmp?".into(), "exact".into()],
            ..txt_options()
        };
        let outcome = scan_root(&dir, &options).unwrap();
        let mut names = filenames(&outcome);
        names.sort_unstable();
        assert_eq!(names, vec!["file.txt", "file.txt"]);
        let parents: Vec<_> = outcome
            .files
            .iter()
            .map(|file| {
                file.path
                    .parent()
                    .unwrap()
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
            })
            .collect();
        assert!(parents.iter().any(|parent| parent == "keep"));
        assert!(parents.iter().any(|parent| parent == "tmp10"));
        assert!(outcome.complete);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn exclude_globs_match_files_and_directories() {
        let dir = temp_dir("exclude-glob");
        fs::write(dir.join("keep.txt"), b"x").unwrap();
        fs::write(dir.join("debug.log"), b"x").unwrap();
        fs::create_dir_all(dir.join("logs")).unwrap();
        fs::write(dir.join("logs/keep.txt"), b"x").unwrap();
        fs::write(dir.join("logs/old.log"), b"x").unwrap();
        fs::create_dir_all(dir.join("skip.cache")).unwrap();
        fs::write(dir.join("skip.cache/kept.txt"), b"x").unwrap();

        let options = ScanOptions {
            extensions: vec!["txt".into(), "log".into()],
            exclude_globs: vec!["*.log".into(), "*.cache".into()],
            ..txt_options()
        };
        let outcome = scan_root(&dir, &options).unwrap();
        let mut names = filenames(&outcome);
        names.sort_unstable();
        assert_eq!(names, vec!["keep.txt", "keep.txt"]);
        assert!(matches!(
            scan_file(&dir, Path::new("debug.log"), &options),
            Err(ScanError::Excluded(_))
        ));
        assert!(scan_file(&dir, Path::new("keep.txt"), &options).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn exclude_globs_with_separators_match_relative_paths() {
        let dir = temp_dir("exclude-relative");
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::create_dir_all(dir.join("other")).unwrap();
        fs::write(dir.join("sub/drop.txt"), b"x").unwrap();
        fs::write(dir.join("sub/keep.md"), b"x").unwrap();
        fs::write(dir.join("other/drop.txt"), b"x").unwrap();

        let options = ScanOptions {
            exclude_globs: vec!["sub/*.txt".into()],
            ..txt_options()
        };
        let outcome = scan_root(&dir, &options).unwrap();
        let mut relative: Vec<_> = outcome
            .files
            .iter()
            .map(|file| {
                file.path
                    .strip_prefix(&dir)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        relative.sort_unstable();
        assert_eq!(relative, vec!["other/drop.txt", "sub/keep.md"]);
        assert!(matches!(
            scan_file(&dir, Path::new("sub/drop.txt"), &options),
            Err(ScanError::Excluded(_))
        ));
        assert!(scan_file(&dir, Path::new("other/drop.txt"), &options).is_ok());
        assert!(matches!(
            scan_file(&dir, Path::new("../outside.txt"), &options),
            Err(ScanError::OutsideRoot(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn exclude_globs_fold_case_only_when_policy_allows() {
        let patterns = vec!["*.LOG".to_string()];
        let globs = ExcludeGlobs::new(&patterns);
        assert!(globs.matches("debug.log", "debug.log", true));
        assert!(!globs.matches("debug.log", "debug.log", false));
        assert!(!exclude_glob_case_insensitive(Path::new(
            r"\\wsl.localhost\Ubuntu\home"
        )));
        assert!(exclude_glob_case_insensitive(Path::new(
            r"\\nas\share\docs"
        )));
    }

    #[test]
    fn max_depth_one_is_non_recursive() {
        let dir = temp_dir("depth");
        fs::write(dir.join("top.txt"), b"top").unwrap();
        fs::create_dir_all(dir.join("sub/deep")).unwrap();
        fs::write(dir.join("sub/nested.txt"), b"nested").unwrap();
        fs::write(dir.join("sub/deep/deep.txt"), b"deep").unwrap();

        let options = ScanOptions {
            max_depth: Some(1),
            ..txt_options()
        };
        let outcome = scan_root(&dir, &options).unwrap();
        assert_eq!(filenames(&outcome), vec!["top.txt"]);
        assert!(outcome.complete);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_matcher_is_minimal_and_bounded() {
        assert!(glob_match("*.cache", "x.cache", false));
        assert!(glob_match("tmp?", "tmp1", false));
        assert!(!glob_match("tmp?", "tmp10", false));
        assert!(glob_match("*", "", false));
        assert!(glob_match("a*b*c", "aXXbYYc", false));
        assert!(!glob_match("a*b*c", "aXXbYY", false));
        assert!(glob_match("*.TXT", "x.txt", true));
    }

    #[test]
    fn source_kind_classifies_wsl_smb_and_local_paths() {
        assert_eq!(
            source_kind(Path::new(r"\\wsl.localhost\Ubuntu\home")),
            SourceKind::Wsl
        );
        assert_eq!(
            source_kind(Path::new(r"\\wsl$\Ubuntu\home")),
            SourceKind::Wsl
        );
        assert_eq!(
            source_kind(Path::new(r"\\?\UNC\wsl.localhost\Ubuntu")),
            SourceKind::Wsl
        );
        assert_eq!(source_kind(Path::new(r"\\nas\share\docs")), SourceKind::Smb);
        assert_eq!(source_kind(Path::new("//nas/share/docs")), SourceKind::Smb);
        assert_eq!(
            source_kind(Path::new(r"\\?\UNC\nas\share")),
            SourceKind::Smb
        );
        assert_eq!(source_kind(Path::new("/srv/data")), SourceKind::Local);
        // An extended drive path must not be mistaken for a UNC share. The
        // actual drive class is the host's business (and can be removable).
        assert_ne!(source_kind(Path::new(r"\\?\C:\Users")), SourceKind::Wsl);
        #[cfg(not(windows))]
        assert_eq!(source_kind(Path::new(r"\\?\C:\Users")), SourceKind::Local);
    }

    #[test]
    fn path_key_keeps_wsl_unc_case_but_folds_windows_smb() {
        let wsl = Path::new(r"\\wsl.localhost\Ubuntu\Home\File.TXT");
        assert_eq!(
            path_key_with_policy(wsl, true),
            r"\\wsl.localhost\Ubuntu\Home\File.TXT"
        );

        let smb = Path::new(r"\\nas\Share\Docs\File.TXT");
        assert_eq!(
            path_key_with_policy(smb, true),
            r"\\nas\share\docs\file.txt"
        );

        let posix = Path::new("/Home/File.TXT");
        assert_eq!(path_key_with_policy(posix, false), "/Home/File.TXT");
        assert_eq!(path_key_with_policy(posix, true), "/home/file.txt");
    }

    #[test]
    fn overlap_helpers_are_component_aware_and_never_touch_disk() {
        assert!(path_within(
            Path::new("/srv/data"),
            Path::new("/srv/data/sub")
        ));
        assert!(path_within(Path::new("/srv/data"), Path::new("/srv/data")));
        assert!(!path_within(
            Path::new("/srv/data"),
            Path::new("/srv/database")
        ));
        assert!(!path_within(
            Path::new("/srv/data"),
            Path::new("/srv/data2")
        ));
        assert!(paths_overlap(Path::new("/srv"), Path::new("/srv/data")));
        assert!(!paths_overlap(Path::new("/srv/a"), Path::new("/srv/b")));

        assert!(path_within(
            Path::new(r"\\nas\share"),
            Path::new(r"\\nas\share\docs")
        ));
        assert!(!path_within(
            Path::new(r"\\wsl.localhost\Ubuntu\Home"),
            Path::new(r"\\wsl.localhost\Ubuntu\home\file.txt")
        ));

        // A missing path still gets a lexical key; callers decide whether it
        // is reachable before using it as a source.
        assert!(!canonical_path_key(Path::new("/definitely/not/here")).is_empty());
        assert!(!canonical_path_key(Path::new(r"\\nas\share\")).ends_with('\\'));
    }

    #[test]
    fn cancellation_returns_cancelled_error() {
        let dir = temp_dir("cancel");
        fs::write(dir.join("a.txt"), b"a").unwrap();
        fs::write(dir.join("b.txt"), b"b").unwrap();

        let calls = Cell::new(0);
        let result = scan_root_controlled(&dir, &txt_options(), &|| {
            calls.set(calls.get() + 1);
            calls.get() >= 2
        });
        assert!(matches!(result, Err(ScanError::Cancelled)));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn inaccessible_root_fails_before_walking() {
        let dir = temp_dir("missing");
        let _ = fs::remove_dir_all(&dir);
        let result = scan_root(&dir, &txt_options());
        assert!(matches!(result, Err(ScanError::Unavailable(_))));
    }

    #[cfg(unix)]
    #[test]
    fn root_disappearing_mid_scan_marks_incomplete() {
        let dir = temp_dir("vanish");
        fs::write(dir.join("a.txt"), b"a").unwrap();
        fs::write(dir.join("b.txt"), b"b").unwrap();

        let calls = Cell::new(0);
        let outcome = scan_root_controlled(&dir, &txt_options(), &|| {
            let current = calls.get() + 1;
            calls.set(current);
            if current == 2 {
                let _ = fs::remove_dir_all(&dir);
            }
            false
        })
        .unwrap();
        assert!(!outcome.complete);
        assert!(!outcome.issues.is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn replaced_root_is_detected_by_identity() {
        let temp = temp_dir("replace");
        let dir = temp.join("root");
        fs::create_dir(&dir).unwrap();
        let metadata = fs::metadata(&dir).unwrap();
        let identity = root_identity(&metadata);
        assert!(identity.is_some());
        // Keep the old directory alive so its inode cannot be reused.
        fs::rename(&dir, temp.join("old-root")).unwrap();
        fs::create_dir(&dir).unwrap();
        assert!(root_change_issue(&dir, identity).is_some());
        let _ = fs::remove_dir_all(&temp);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_skipped_and_targeted_links_are_rejected() {
        use std::os::unix::fs::symlink;

        let dir = temp_dir("symlink");
        let outside = temp_dir("symlink-outside");
        fs::write(dir.join("real.txt"), b"real").unwrap();
        fs::write(outside.join("outside.txt"), b"outside").unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        symlink(&outside, dir.join("linked_dir")).unwrap();
        symlink(outside.join("outside.txt"), dir.join("linked_file.txt")).unwrap();
        symlink(&dir, dir.join("sub/loop")).unwrap();

        let outcome = scan_root(&dir, &txt_options()).unwrap();
        assert_eq!(filenames(&outcome), vec!["real.txt"]);
        assert!(outcome.complete);

        assert!(matches!(
            scan_file(&dir, Path::new("linked_dir/outside.txt"), &txt_options()),
            Err(ScanError::NotAFile(_))
        ));
        assert!(matches!(
            scan_file(&dir, Path::new("linked_file.txt"), &txt_options()),
            Err(ScanError::NotAFile(_))
        ));

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn scan_file_applies_wildcard_exclusions_to_parents() {
        let dir = temp_dir("scan-file");
        fs::create_dir_all(dir.join("skip.cache")).unwrap();
        fs::write(dir.join("skip.cache/file.txt"), b"x").unwrap();
        let options = ScanOptions {
            excluded_dirs: vec!["*.cache".into()],
            ..txt_options()
        };
        assert!(matches!(
            scan_file(&dir, Path::new("skip.cache/file.txt"), &options),
            Err(ScanError::Excluded(_))
        ));
        let _ = fs::remove_dir_all(&dir);
    }
}
