//! Archive-bomb defenses + safe `.tar.gz` extraction.
//!
//! Refuses, per the v2 Security Contract addendum:
//! - entries whose path resolves outside the destination directory
//!   (`..`, absolute paths, FIFOs/devices, out-of-tree symlinks),
//! - hardlinks (no in-tree hardlink ergonomics worth the risk),
//! - total uncompressed size above `ExtractLimits::total_bytes`,
//! - a single entry above `ExtractLimits::per_entry_bytes`,
//! - archives with > 10 000 entries.
//!
//! The size caps scale with the asset the GitHub API declared (see
//! [`ExtractLimits::for_asset`]), because llama.cpp's own blobs keep
//! growing: `libggml-hip.so` alone declared 1.09 GiB at `b11804` and a
//! flat 1 GiB per-entry cap refused the whole ROCm release (issue #119).
//!
//! Symlinks are allowed *only* when the resolved target lives inside
//! the extraction root. llama.cpp release tarballs rely on this for
//! the SONAME chain (`libmtmd.so -> libmtmd.so.0 ->
//! libmtmd.so.0.0.<build>`); without it the dynamic linker can't
//! resolve the shared libs at runtime.
//!
//! Extracted binaries end up `chmod 0700` regardless of the archive
//! entry mode (parent dir is also `0700` per the wizard's
//! `mkdir-with-mode` rule).

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use tar::EntryType;

use super::InstallError;

pub const MAX_ENTRIES: usize = 10_000;
/// Floor for the uncompressed-total cap, used when the archive's own
/// asset size is unknown. `ExtractLimits::for_asset` raises it for
/// archives whose declared size justifies more.
pub const MIN_TOTAL_UNCOMPRESSED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// Most uncompressed bytes an archive may claim per compressed byte it
/// ships. Measured on `b11804` assets: the ROCm build unpacks 4.9×
/// (one 1.09 GiB `libggml-hip.so`), the Windows ROCm zip 4.6×, a CUDA
/// build 1.4× and its `cudart-` bundle 1.5×. 16× leaves headroom for
/// upstream growth while still tying the bomb bound to bytes actually
/// downloaded from the pinned release host.
const MAX_UNPACK_RATIO: u64 = 16;

/// The entry-count and size caps applied to one archive.
///
/// (A per-entry compression-ratio guard is deliberately not
/// included: tar's high-level reader does not expose per-entry
/// compressed bytes when the gz stream wraps the whole archive,
/// so any ratio computed from declared-size / archive-compressed-
/// length would not actually measure per-entry compression and
/// would give a false sense of defense. The absolute caps cover
/// the same threat surface.)
#[derive(Debug, Clone, Copy)]
pub struct ExtractLimits {
  pub max_entries: usize,
  pub total_bytes: u64,
  /// One entry may claim the whole archive budget, so this tracks
  /// `total_bytes` rather than a fixed fraction of it.
  pub per_entry_bytes: u64,
}

impl Default for ExtractLimits {
  fn default() -> Self {
    Self::for_asset(None)
  }
}

impl ExtractLimits {
  /// Caps for an archive whose GitHub asset row declares `asset_bytes`
  /// (compressed). Unknown declared size falls back to
  /// `MIN_TOTAL_UNCOMPRESSED_BYTES`.
  pub fn for_asset(asset_bytes: Option<u64>) -> Self {
    let total = asset_bytes
      .map(|bytes| bytes.saturating_mul(MAX_UNPACK_RATIO))
      .unwrap_or(0)
      .max(MIN_TOTAL_UNCOMPRESSED_BYTES);
    Self {
      max_entries: MAX_ENTRIES,
      total_bytes: total,
      per_entry_bytes: total,
    }
  }
}

#[derive(Debug, Clone)]
pub struct ExtractedBinary {
  /// Final on-disk path of the extracted `llama-server` binary. Mode
  /// is `0700` on Unix; non-Unix targets fall back to whatever
  /// `tempfile::persist` preserved.
  pub path: PathBuf,
}

/// Stream the `.tar.gz` body in `archive_bytes` into a unique tmp dir
/// under `dest_root`. On success the tmp dir is atomic-renamed to
/// `dest_root.join(version_dir_name)` and the path of the resolved
/// `llama-server` entry is returned. Refuses every adversarial shape
/// listed in the module-level docs.
pub fn safe_extract_tar_gz<R: Read>(
  archive: R,
  dest_root: &Path,
  version_dir_name: &str,
  limits: ExtractLimits,
) -> Result<ExtractedBinary, InstallError> {
  std::fs::create_dir_all(dest_root).map_err(|e| InstallError::Io(e.to_string()))?;
  if let Some(existing) = existing_install(dest_root, version_dir_name)? {
    return Ok(existing);
  }
  let final_dir = dest_root.join(version_dir_name);

  let tmp = tempfile::Builder::new()
    .prefix(&format!("{version_dir_name}.tmp."))
    .tempdir_in(dest_root)
    .map_err(|e| InstallError::Io(e.to_string()))?;
  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o700));
  }
  let mut caps = Caps::new(limits);

  let gz = GzDecoder::new(archive);
  let mut tar = tar::Archive::new(gz);
  // Disable libtar's default permission setting (we override mode
  // explicitly) and disable symlink/hardlink restoration (refused).
  tar.set_preserve_permissions(false);
  tar.set_unpack_xattrs(false);
  tar.set_overwrite(false);

  let mut found_binary: Option<PathBuf> = None;

  for entry in tar
    .entries()
    .map_err(|e| InstallError::Io(format!("tar read: {e}")))?
  {
    caps.count()?;
    let mut entry = entry.map_err(|e| InstallError::Io(format!("tar entry: {e}")))?;
    let (entry_path, entry_path_str) = entry_path(&entry)?;
    let entry_type = entry.header().entry_type();

    // Hardlinks remain refused — release tarballs don't use them and
    // they're harder to bound safely (a hardlink to an already-extracted
    // SUID binary would inherit its mode).
    if entry_type == EntryType::Link {
      return Err(unsafe_entry(&entry_path_str, "hardlink entries refused"));
    }
    // Refuse anything that isn't a regular file, directory, or symlink.
    if !matches!(
      entry_type,
      EntryType::Regular | EntryType::Directory | EntryType::Symlink
    ) {
      return Err(unsafe_entry(
        &entry_path_str,
        format!("unsupported entry type {entry_type:?}"),
      ));
    }
    let safe_rel =
      safe_relative_path(&entry_path).map_err(|reason| unsafe_entry(&entry_path_str, reason))?;
    let target = tmp.path().join(&safe_rel);

    if entry_type == EntryType::Directory {
      std::fs::create_dir_all(&target).map_err(|e| InstallError::Io(e.to_string()))?;
      continue;
    }

    if entry_type == EntryType::Symlink {
      let link_name = entry
        .link_name()
        .map_err(|e| unsafe_entry(&entry_path_str, format!("symlink target unreadable: {e}")))?
        .ok_or_else(|| unsafe_entry(&entry_path_str, "symlink with no target"))?;
      let safe_link = safe_symlink_target(&safe_rel, &link_name)
        .map_err(|reason| unsafe_entry(&entry_path_str, reason))?;
      if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| InstallError::Io(e.to_string()))?;
      }
      #[cfg(unix)]
      {
        std::os::unix::fs::symlink(&safe_link, &target)
          .map_err(|e| InstallError::Io(format!("symlink {entry_path_str}: {e}")))?;
      }
      #[cfg(not(unix))]
      {
        // No symlink ergonomics on non-Unix; skip the entry rather
        // than fail — Windows/macOS-arm releases don't ship SONAME
        // chains.
        let _ = safe_link;
      }
      continue;
    }

    caps.charge(&entry_path_str, entry.header().size().unwrap_or(0))?;
    // Ensure parent dir exists; we already validated path safety.
    if let Some(parent) = target.parent() {
      std::fs::create_dir_all(parent).map_err(|e| InstallError::Io(e.to_string()))?;
    }
    let mut out = std::fs::File::create(&target).map_err(|e| InstallError::Io(e.to_string()))?;
    copy_capped(&mut entry, &mut out, caps.limits.per_entry_bytes)?;
    drop(out);
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      if let Some(name) = safe_rel.file_name().and_then(|n| n.to_str()) {
        if name == "llama-server" {
          let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o700));
          found_binary = Some(target.clone());
        }
      }
    }
    #[cfg(not(unix))]
    {
      if safe_rel.file_name().and_then(|n| n.to_str()) == Some("llama-server") {
        found_binary = Some(target.clone());
      }
    }
  }

  let binary = found_binary.ok_or_else(|| InstallError::UnsafeArchive {
    path: String::new(),
    reason: "archive did not contain a `llama-server` entry".into(),
  })?;

  // Atomic rename to the final versioned directory. `final_dir`
  // already passed the does-not-exist check at the top of the
  // function, so we go straight to the rename.
  let from = tmp.keep();
  if let Err(e) = std::fs::rename(&from, &final_dir) {
    // `tmp.keep()` opted out of Drop-based cleanup. If the rename
    // fails (cross-device, permission, disk-full) we must clean the
    // kept temp dir manually so failed installs don't accumulate
    // orphan `<ver>.tmp.<pid>` dirs under dest_root.
    let _ = std::fs::remove_dir_all(&from);
    return Err(InstallError::Io(format!("rename: {e}")));
  }
  let rel = binary
    .strip_prefix(&from)
    // Tempdir was renamed; recompute relative path against tmp's last name segment.
    .or_else(|_| binary.strip_prefix(&final_dir))
    .map_err(|e| InstallError::Io(format!("strip prefix after rename: {e}")))?;
  Ok(ExtractedBinary {
    path: final_dir.join(rel),
  })
}

/// Extract a runtime-library bundle (the Linux `cudart-` tarball) flat
/// into `dest_dir`, the directory holding the installed binary.
///
/// Stricter than [`safe_extract_tar_gz`]: only directories and regular
/// files named `lib*.so*` are accepted, each written under its own file
/// name directly in `dest_dir`, since the binaries resolve them through
/// an `$ORIGIN` rpath. Same size and entry caps. Each file lands through
/// a temp file and a rename, so an interrupted run leaves no partial
/// library behind; an existing file of the same name is replaced.
pub fn safe_extract_libs_tar_gz<R: Read>(
  archive: R,
  dest_dir: &Path,
  limits: ExtractLimits,
) -> Result<(), InstallError> {
  let mut tar = tar::Archive::new(GzDecoder::new(archive));
  let mut caps = Caps::new(limits);
  let mut written = 0usize;
  for entry in tar
    .entries()
    .map_err(|e| InstallError::Io(format!("tar read: {e}")))?
  {
    caps.count()?;
    let mut entry = entry.map_err(|e| InstallError::Io(format!("tar entry: {e}")))?;
    let (entry_path, entry_path_str) = entry_path(&entry)?;
    let refuse = |reason: &str| unsafe_entry(&entry_path_str, reason);
    let safe_rel = safe_relative_path(&entry_path).map_err(|r| refuse(&r))?;
    match entry.header().entry_type() {
      EntryType::Directory => continue,
      EntryType::Regular => {}
      other => return Err(refuse(&format!("unsupported entry type {other:?}"))),
    }
    let name = safe_rel
      .file_name()
      .and_then(|n| n.to_str())
      .ok_or_else(|| refuse("entry has no file name"))?;
    if !(name.starts_with("lib") && name.contains(".so")) {
      return Err(refuse("not a shared library"));
    }
    caps.charge(&entry_path_str, entry.header().size().unwrap_or(0))?;
    let mut tmp = tempfile::Builder::new()
      .prefix(&format!(".{name}.tmp."))
      .tempfile_in(dest_dir)
      .map_err(|e| InstallError::Io(e.to_string()))?;
    copy_capped(&mut entry, tmp.as_file_mut(), caps.limits.per_entry_bytes)?;
    tmp
      .persist(dest_dir.join(name))
      .map_err(|e| InstallError::Io(format!("persist {name}: {e}")))?;
    written += 1;
  }
  if written == 0 {
    return Err(InstallError::UnsafeArchive {
      path: String::new(),
      reason: "runtime bundle held no shared libraries".into(),
    });
  }
  Ok(())
}

/// The `llama-server` an earlier run extracted to
/// `dest_root/version_dir_name`, when that dir exists. Extraction lands
/// through a rename, so an existing dir holds a finished install.
pub fn existing_install(
  dest_root: &Path,
  version_dir_name: &str,
) -> Result<Option<ExtractedBinary>, InstallError> {
  let final_dir = dest_root.join(version_dir_name);
  if !final_dir.exists() {
    return Ok(None);
  }
  // Locate the actual `llama-server` inside `final_dir` rather than
  // computing a path from the archive's layout — the two may not
  // match (partial prior install, different release tarball schema).
  let path = find_llama_server(&final_dir).ok_or_else(|| InstallError::UnsafeArchive {
    path: final_dir.display().to_string(),
    reason: "pre-existing versioned dir does not contain a `llama-server` binary".into(),
  })?;
  Ok(Some(ExtractedBinary { path }))
}

fn unsafe_entry(path: &str, reason: impl Into<String>) -> InstallError {
  InstallError::UnsafeArchive {
    path: path.to_string(),
    reason: reason.into(),
  }
}

/// A tar entry's raw path, and its display form for refusals.
fn entry_path<R: Read>(entry: &tar::Entry<'_, R>) -> Result<(PathBuf, String), InstallError> {
  let path = entry
    .path()
    .map(|p| p.to_path_buf())
    .map_err(|e| unsafe_entry("", format!("bad path: {e}")))?;
  let shown = path.display().to_string();
  Ok((path, shown))
}

/// Running entry count and uncompressed total, checked against the caps.
#[derive(Default)]
struct Caps {
  entries: usize,
  total: u64,
  limits: ExtractLimits,
}

impl Caps {
  fn new(limits: ExtractLimits) -> Self {
    Self {
      entries: 0,
      total: 0,
      limits,
    }
  }

  fn count(&mut self) -> Result<(), InstallError> {
    self.entries += 1;
    if self.entries > self.limits.max_entries {
      return Err(unsafe_entry(
        "",
        format!("entry count exceeded the {MAX_ENTRIES} cap"),
      ));
    }
    Ok(())
  }

  /// Charge a file entry's declared size, before anything is written.
  fn charge(&mut self, path: &str, size: u64) -> Result<(), InstallError> {
    if size > self.limits.per_entry_bytes {
      return Err(unsafe_entry(
        path,
        format!(
          "entry size {size} exceeds per-entry cap of \
           {} bytes",
          self.limits.per_entry_bytes
        ),
      ));
    }
    self.total = self.total.saturating_add(size);
    if self.total > self.limits.total_bytes {
      return Err(unsafe_entry(
        path,
        format!(
          "total uncompressed size exceeded the {}-byte cap",
          self.limits.total_bytes
        ),
      ));
    }
    Ok(())
  }
}

/// Copy at most the per-entry cap, whatever size the header declared.
fn copy_capped(
  entry: &mut impl Read,
  out: &mut impl std::io::Write,
  cap: u64,
) -> Result<(), InstallError> {
  let mut limited = entry.take(cap);
  std::io::copy(&mut limited, out).map_err(|e| InstallError::Io(e.to_string()))?;
  Ok(())
}

/// Dispatch on `archive_name`'s extension to either the tar.gz or
/// (Windows-only) zip extraction codepath, reading the archive from
/// `archive_path`. The picked filename's trailing extension drives the
/// choice — `.zip` routes through the Windows backend, `.tar.gz` / `.tgz`
/// routes through the tar reader. Anything else surfaces an
/// `UnsafeArchive` refusal so the caller can fall back to the
/// manual-path install flow.
pub fn safe_extract(
  archive_name: &str,
  archive_path: &Path,
  dest_root: &Path,
  version_dir_name: &str,
  limits: ExtractLimits,
) -> Result<ExtractedBinary, InstallError> {
  let lower = archive_name.to_ascii_lowercase();
  if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
    let file = std::fs::File::open(archive_path).map_err(|e| InstallError::Io(e.to_string()))?;
    return safe_extract_tar_gz(
      std::io::BufReader::new(file),
      dest_root,
      version_dir_name,
      limits,
    );
  }
  #[cfg(windows)]
  if lower.ends_with(".zip") {
    let file = std::fs::File::open(archive_path).map_err(|e| InstallError::Io(e.to_string()))?;
    return safe_extract_zip(
      std::io::BufReader::new(file),
      dest_root,
      version_dir_name,
      limits,
    );
  }
  #[cfg(not(windows))]
  if lower.ends_with(".zip") {
    // Linux/macOS aren't picking Windows .zip assets via `pick_asset_suffix`,
    // so a `.zip` here means either a corrupt picker or a misrouted manual
    // install path. Refuse loudly rather than silently fall through.
    return Err(InstallError::UnsafeArchive {
      path: archive_name.into(),
      reason: ".zip extraction is only supported on Windows".into(),
    });
  }
  Err(InstallError::UnsafeArchive {
    path: archive_name.into(),
    reason: "unsupported archive extension (expected .tar.gz, .tgz, or .zip)".into(),
  })
}

/// Stream the `.zip` in `archive` into a unique tmp dir
/// under `dest_root`. Same safety contract as
/// [`safe_extract_tar_gz`]: entry path validation rejects `..` /
/// absolute / Windows-drive-prefix entries, per-entry and total-size
/// caps prevent zip-bombs, and only regular files + directories are
/// emitted (zip's analogue of symlinks via central-directory mode
/// bits is ignored — Windows llama.cpp releases don't ship SONAME
/// chains the way Linux .tar.gz does).
#[cfg(windows)]
pub fn safe_extract_zip<R: Read + std::io::Seek>(
  archive: R,
  dest_root: &Path,
  version_dir_name: &str,
  limits: ExtractLimits,
) -> Result<ExtractedBinary, InstallError> {
  std::fs::create_dir_all(dest_root).map_err(|e| InstallError::Io(e.to_string()))?;
  if let Some(existing) = existing_install(dest_root, version_dir_name)? {
    return Ok(existing);
  }
  let final_dir = dest_root.join(version_dir_name);

  let tmp = tempfile::Builder::new()
    .prefix(&format!("{version_dir_name}.tmp."))
    .tempdir_in(dest_root)
    .map_err(|e| InstallError::Io(e.to_string()))?;

  let mut archive =
    zip::ZipArchive::new(archive).map_err(|e| InstallError::Io(format!("zip open: {e}")))?;

  let entry_count = archive.len();
  if entry_count > limits.max_entries {
    return Err(InstallError::UnsafeArchive {
      path: String::new(),
      reason: format!("zip entry count {entry_count} exceeds the {MAX_ENTRIES} cap"),
    });
  }

  let mut total_uncompressed: u64 = 0;
  let mut found_binary: Option<PathBuf> = None;

  for i in 0..entry_count {
    let mut entry = archive
      .by_index(i)
      .map_err(|e| InstallError::Io(format!("zip entry {i}: {e}")))?;
    // `name()` decodes the stored bytes and fails on a header that does not
    // decode. This string only names the entry in a refusal, so it reads the
    // raw bytes; `enclosed_name()` is what decides whether the entry is usable.
    let raw_name = String::from_utf8_lossy(entry.name_raw()).into_owned();
    // `zip` exposes a sanitized `enclosed_name()` that already rejects
    // absolute paths, `..` traversal, and Windows drive prefixes — but
    // we re-validate via `safe_relative_path` to keep the refusal
    // surface identical to the tar codepath.
    let enclosed = entry
      .enclosed_name()
      .ok_or_else(|| InstallError::UnsafeArchive {
        path: raw_name.clone(),
        reason: "zip entry name resolves outside the archive root".into(),
      })?;
    let safe_rel = safe_relative_path(&enclosed).map_err(|reason| InstallError::UnsafeArchive {
      path: raw_name.clone(),
      reason,
    })?;
    let target = tmp.path().join(&safe_rel);

    if entry.is_dir() {
      std::fs::create_dir_all(&target).map_err(|e| InstallError::Io(e.to_string()))?;
      continue;
    }
    if !entry.is_file() {
      // is_symlink() returns true for the zip equivalent of symlinks
      // (Unix mode bits in the external-attributes field). Windows
      // llama.cpp release assets do not ship symlinks; refuse rather
      // than silently emit a broken file.
      return Err(InstallError::UnsafeArchive {
        path: raw_name,
        reason: "zip entry is neither a regular file nor a directory".into(),
      });
    }

    let declared_size = entry.size();
    if declared_size > limits.per_entry_bytes {
      return Err(InstallError::UnsafeArchive {
        path: raw_name,
        reason: format!(
          "zip entry size {declared_size} exceeds per-entry cap of \
           {} bytes",
          limits.per_entry_bytes
        ),
      });
    }
    total_uncompressed = total_uncompressed.saturating_add(declared_size);
    if total_uncompressed > limits.total_bytes {
      return Err(InstallError::UnsafeArchive {
        path: raw_name,
        reason: format!(
          "zip total uncompressed size exceeded the {}-byte cap",
          limits.total_bytes
        ),
      });
    }

    if let Some(parent) = target.parent() {
      std::fs::create_dir_all(parent).map_err(|e| InstallError::Io(e.to_string()))?;
    }
    let mut out = std::fs::File::create(&target).map_err(|e| InstallError::Io(e.to_string()))?;
    // Cap the stream itself in case the declared size lied: read at
    // most `limits.per_entry_bytes` regardless of what the header said.
    let mut limited = (&mut entry).take(limits.per_entry_bytes);
    std::io::copy(&mut limited, &mut out).map_err(|e| InstallError::Io(e.to_string()))?;
    drop(out);

    // Windows binaries: `.exe` is the executable signal; no +x bit
    // bookkeeping. The init wizard looks for `llama-server.exe`.
    if safe_rel.file_name().and_then(|n| n.to_str()) == Some("llama-server.exe") {
      found_binary = Some(target.clone());
    }
  }

  let binary = found_binary.ok_or_else(|| InstallError::UnsafeArchive {
    path: String::new(),
    reason: "zip archive did not contain a `llama-server.exe` entry".into(),
  })?;

  let from = tmp.keep();
  if let Err(e) = std::fs::rename(&from, &final_dir) {
    let _ = std::fs::remove_dir_all(&from);
    return Err(InstallError::Io(format!("rename: {e}")));
  }
  let rel = binary
    .strip_prefix(&from)
    .or_else(|_| binary.strip_prefix(&final_dir))
    .map_err(|e| InstallError::Io(format!("strip prefix after rename: {e}")))?;
  Ok(ExtractedBinary {
    path: final_dir.join(rel),
  })
}

/// Search `root` for a regular file named `llama-server`. Bounded BFS
/// to `MAX_SCAN_DEPTH` so an attacker-planted versioned dir can't
/// turn this into an unbounded filesystem walk. Depth 4 covers the
/// observed llama.cpp tarball layouts (`build/bin/`, `bin/`,
/// top-level) plus headroom for one extra wrapping directory.
fn find_llama_server(root: &Path) -> Option<PathBuf> {
  const MAX_SCAN_DEPTH: u8 = 4;
  // Both Unix (`llama-server`) and Windows (`llama-server.exe`)
  // shapes — the early-return scan applies to whichever the
  // pre-existing dir actually holds.
  let names: &[&std::ffi::OsStr] = &[
    std::ffi::OsStr::new("llama-server"),
    std::ffi::OsStr::new("llama-server.exe"),
  ];
  let mut queue: Vec<(PathBuf, u8)> = vec![(root.to_path_buf(), 0)];
  while let Some((dir, depth)) = queue.pop() {
    let Ok(entries) = std::fs::read_dir(&dir) else {
      continue;
    };
    for entry in entries.flatten() {
      if names.iter().any(|n| entry.file_name() == *n) {
        let path = entry.path();
        if std::fs::metadata(&path)
          .map(|m| m.is_file())
          .unwrap_or(false)
        {
          return Some(path);
        }
      }
      if depth + 1 < MAX_SCAN_DEPTH && entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
        queue.push((entry.path(), depth + 1));
      }
    }
  }
  None
}

/// Validate a symlink target against the archive root. `entry_rel` is
/// the symlink's own (already-validated) path inside the extract tree;
/// `link_target` is the raw `link_name` from the tar header. Refuses
/// absolute targets and any relative target that escapes the extract
/// root once lexically joined to the symlink's parent directory.
///
/// Returns the link target verbatim on success — the caller writes it
/// into the on-disk symlink, preserving POSIX relative-link semantics
/// (the dynamic linker resolves it relative to the symlink's dir at
/// runtime, which is exactly what SONAME chains expect).
fn safe_symlink_target(entry_rel: &Path, link_target: &Path) -> Result<PathBuf, String> {
  // Absolute targets would point outside the extract root by
  // definition (the root is a unique tmp dir under `dest_root`).
  if link_target.is_absolute() {
    return Err("symlink target is absolute".into());
  }
  if link_target.as_os_str().is_empty() {
    return Err("symlink target is empty".into());
  }
  // Lexically join: parent-of-symlink + relative target, normalize
  // by collapsing `..` against the path stack. If at any point we'd
  // pop above the archive root, refuse.
  let mut stack: Vec<&std::ffi::OsStr> = Vec::new();
  let mut parent_len: usize = 0;
  if let Some(parent) = entry_rel.parent() {
    for comp in parent.components() {
      match comp {
        Component::Normal(s) => {
          stack.push(s);
          parent_len += 1;
        }
        Component::CurDir => continue,
        // entry_rel was already validated; these can't occur.
        Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
          return Err("symlink parent path is malformed".into());
        }
      }
    }
  }
  for comp in link_target.components() {
    match comp {
      Component::Normal(s) => stack.push(s),
      Component::CurDir => continue,
      Component::ParentDir => {
        if stack.pop().is_none() {
          return Err("symlink target escapes the archive root via `..`".into());
        }
      }
      Component::RootDir | Component::Prefix(_) => {
        return Err("symlink target contains an absolute component".into());
      }
    }
  }
  // The resolved target must descend strictly past its parent dir.
  // Anything that lands at or above the parent (`.`, `..`, or a
  // chain that pops back to the parent) creates a self-loop or
  // up-pointer: a later regular-file entry under the symlink's
  // path would be OS-resolved through the symlink and could
  // overwrite a previously-extracted file. Refuse outright — the
  // llama.cpp release tarballs only use SONAME-style symlinks
  // (sibling file in the same dir, or strictly deeper).
  if stack.len() <= parent_len {
    return Err("symlink target does not descend into a strict subpath".into());
  }
  Ok(link_target.to_path_buf())
}

/// Resolve an archive entry's path against a virtual root, refusing
/// absolute paths and `..` traversal. Returns the validated relative
/// path on success; `Err(reason)` describes why a path was refused.
fn safe_relative_path(p: &Path) -> Result<PathBuf, String> {
  let mut out = PathBuf::new();
  for comp in p.components() {
    match comp {
      Component::Normal(s) => out.push(s),
      Component::CurDir => continue,
      Component::ParentDir => {
        return Err("`..` traversal in archive entry path".into());
      }
      Component::RootDir => {
        return Err("absolute path in archive entry".into());
      }
      Component::Prefix(_) => {
        return Err("path prefix in archive entry (Windows drive?)".into());
      }
    }
  }
  if out.as_os_str().is_empty() {
    return Err("archive entry path is empty after normalisation".into());
  }
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::*;
  use flate2::write::GzEncoder;
  use flate2::Compression;
  use tar::{Builder, Header};

  fn temp_dir(label: &str) -> PathBuf {
    crate::util::test_temp::unique_temp_dir(&format!("extract-{label}"))
  }

  /// Write `bytes` to a fresh file, the way the installer hands
  /// [`safe_extract`] a downloaded archive.
  fn on_disk(label: &str, bytes: &[u8]) -> PathBuf {
    let path = temp_dir(&format!("{label}-src")).join("archive");
    std::fs::write(&path, bytes).unwrap();
    path
  }

  fn build_archive<F: FnOnce(&mut Builder<GzEncoder<Vec<u8>>>)>(f: F) -> Vec<u8> {
    let buf: Vec<u8> = Vec::new();
    let enc = GzEncoder::new(buf, Compression::fast());
    let mut tar = Builder::new(enc);
    f(&mut tar);
    tar.into_inner().unwrap().finish().unwrap()
  }

  fn write_file_entry(tar: &mut Builder<GzEncoder<Vec<u8>>>, path: &str, body: &[u8]) {
    let mut header = Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o755);
    header.set_entry_type(EntryType::Regular);
    header.set_cksum();
    tar.append_data(&mut header, path, body).unwrap();
  }

  #[test]
  fn extracts_well_formed_archive() {
    let archive = build_archive(|tar| {
      write_file_entry(tar, "build/bin/llama-server", b"#!/bin/sh\necho ok\n");
      write_file_entry(tar, "build/bin/README.md", b"docs");
    });
    let dest = temp_dir("happy");
    let out = safe_extract_tar_gz(archive.as_slice(), &dest, "b9999", ExtractLimits::default())
      .expect("extract");
    assert!(out.path.is_file());
    assert!(out.path.ends_with("build/bin/llama-server"));
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      let mode = std::fs::metadata(&out.path).unwrap().permissions().mode() & 0o777;
      assert_eq!(mode, 0o700, "llama-server must be chmod 0700");
    }
    std::fs::remove_dir_all(&dest).ok();
  }

  #[test]
  fn runtime_libs_land_flat_beside_the_binary() {
    // Upstream's layout: one wrapping dir holding the three libraries.
    let top = "cudart-llama-b11302-bin-ubuntu-cuda-13.4-x64";
    let archive = build_archive(|tar| {
      for lib in ["libcudart.so.13", "libcublas.so.13", "libcublasLt.so.13"] {
        write_file_entry(tar, &format!("{top}/{lib}"), lib.as_bytes());
      }
    });
    let dest = temp_dir("libs-flat");
    std::fs::write(dest.join("llama-server"), b"bin").unwrap();
    safe_extract_libs_tar_gz(archive.as_slice(), &dest, ExtractLimits::default()).expect("extract");
    assert_eq!(
      std::fs::read(dest.join("libcudart.so.13")).unwrap(),
      b"libcudart.so.13"
    );
    assert!(dest.join("libcublasLt.so.13").is_file());
    assert!(!dest.join(top).exists(), "no wrapping dir");
    std::fs::remove_dir_all(&dest).ok();
  }

  #[test]
  fn runtime_bundle_refuses_anything_but_shared_libraries() {
    let dest = temp_dir("libs-refuse");
    let exe = build_archive(|tar| write_file_entry(tar, "top/llama-server", b"swap"));
    assert!(matches!(
      safe_extract_libs_tar_gz(exe.as_slice(), &dest, ExtractLimits::default()),
      Err(InstallError::UnsafeArchive { .. })
    ));
    assert!(!dest.join("llama-server").exists());
    let link = build_archive(|tar| {
      let mut header = Header::new_gnu();
      header.set_size(0);
      header.set_entry_type(EntryType::Symlink);
      header.set_link_name("/usr/lib/libc.so.6").unwrap();
      header.set_cksum();
      tar
        .append_data(&mut header, "top/libcudart.so.13", &[][..])
        .unwrap();
    });
    assert!(safe_extract_libs_tar_gz(link.as_slice(), &dest, ExtractLimits::default()).is_err());
    let empty = build_archive(|_| {});
    assert!(safe_extract_libs_tar_gz(empty.as_slice(), &dest, ExtractLimits::default()).is_err());
    std::fs::remove_dir_all(&dest).ok();
  }

  #[test]
  fn safe_relative_path_refuses_dotdot_and_absolute() {
    // The `tar` crate's high-level Builder refuses to write `..` and
    // absolute paths at archive-build time, which makes it impossible
    // to construct a hostile archive through the standard API. Test
    // the path validator directly — that's the production code path
    // an attacker-crafted archive (built with a raw tar writer) would
    // hit.
    assert!(
      safe_relative_path(Path::new("../escape")).is_err(),
      "`..` must be refused"
    );
    assert!(
      safe_relative_path(Path::new("/etc/passwd")).is_err(),
      "absolute paths must be refused"
    );
    assert!(
      safe_relative_path(Path::new("build/bin/llama-server")).is_ok(),
      "normal relative paths must pass"
    );
    // `./prefix` is fine — Component::CurDir is filtered out.
    assert!(safe_relative_path(Path::new("./build/bin/llama-server")).is_ok());
  }

  #[test]
  fn refuses_hardlink_entry() {
    let archive = build_archive(|tar| {
      write_file_entry(tar, "build/bin/llama-server", b"binary");
      let mut header = Header::new_gnu();
      header.set_size(0);
      header.set_entry_type(EntryType::Link);
      header.set_link_name("build/bin/llama-server").unwrap();
      header.set_cksum();
      tar
        .append_data(&mut header, "build/bin/llama-server-alias", &[][..])
        .unwrap();
    });
    let dest = temp_dir("hardlink");
    let err = safe_extract_tar_gz(archive.as_slice(), &dest, "b9999", ExtractLimits::default())
      .unwrap_err();
    assert!(
      matches!(err, InstallError::UnsafeArchive { ref reason, .. } if reason.contains("hardlink")),
      "expected hardlink refusal, got {err:?}"
    );
    std::fs::remove_dir_all(&dest).ok();
  }

  #[test]
  fn refuses_absolute_symlink_target() {
    let archive = build_archive(|tar| {
      write_file_entry(tar, "build/bin/llama-server", b"binary");
      let mut header = Header::new_gnu();
      header.set_size(0);
      header.set_entry_type(EntryType::Symlink);
      header.set_link_name("/etc/passwd").unwrap();
      header.set_cksum();
      tar
        .append_data(&mut header, "passwd-link", &[][..])
        .unwrap();
    });
    let dest = temp_dir("symlink-abs");
    let err = safe_extract_tar_gz(archive.as_slice(), &dest, "b9999", ExtractLimits::default())
      .unwrap_err();
    assert!(
      matches!(err, InstallError::UnsafeArchive { ref reason, .. } if reason.contains("absolute")),
      "expected absolute-target refusal, got {err:?}"
    );
    std::fs::remove_dir_all(&dest).ok();
  }

  #[test]
  fn refuses_fifo_entry() {
    // Anything that isn't Regular / Directory / Symlink — including
    // FIFOs, devices, sockets — must be refused. The `tar` crate
    // doesn't construct these via the high-level API; we synthesise
    // the header directly.
    let archive = build_archive(|tar| {
      write_file_entry(tar, "build/bin/llama-server", b"binary");
      let mut header = Header::new_gnu();
      header.set_size(0);
      header.set_entry_type(EntryType::Fifo);
      header.set_cksum();
      tar
        .append_data(&mut header, "build/bin/oddpipe", &[][..])
        .unwrap();
    });
    let dest = temp_dir("fifo-entry");
    let err = safe_extract_tar_gz(archive.as_slice(), &dest, "b9999", ExtractLimits::default())
      .unwrap_err();
    assert!(
      matches!(err, InstallError::UnsafeArchive { ref reason, .. } if reason.contains("unsupported entry type")),
      "expected unsupported-entry-type refusal, got {err:?}"
    );
    std::fs::remove_dir_all(&dest).ok();
  }

  #[test]
  fn refuses_symlink_with_no_target() {
    // A symlink header whose `link_name` is empty/unset hits either
    // (a) `entry.link_name()` returning `Ok(None)` → "symlink with no
    // target", or (b) `Ok(Some(empty))` → "symlink target is empty"
    // (the ADV-3 explicit-empty check). Both paths are correct
    // refusals; the test accepts either reason.
    let archive = build_archive(|tar| {
      write_file_entry(tar, "build/bin/llama-server", b"binary");
      let mut header = Header::new_gnu();
      header.set_size(0);
      header.set_entry_type(EntryType::Symlink);
      // Deliberately do NOT call `header.set_link_name(...)`.
      header.set_cksum();
      tar
        .append_data(&mut header, "no-target-link", &[][..])
        .unwrap();
    });
    let dest = temp_dir("symlink-no-target");
    let err = safe_extract_tar_gz(archive.as_slice(), &dest, "b9999", ExtractLimits::default())
      .unwrap_err();
    assert!(
      matches!(err, InstallError::UnsafeArchive { ref reason, .. } if reason.contains("no target") || reason.contains("empty")),
      "expected no-target or empty-target refusal, got {err:?}"
    );
    std::fs::remove_dir_all(&dest).ok();
  }

  #[test]
  fn refuses_self_loop_symlink() {
    // Adversarial finding: a symlink with target "." resolves through
    // the OS to its own parent dir. A later regular-file entry under
    // the symlink's path would then be `File::create`-resolved up one
    // level and could overwrite an earlier extracted file. Refuse
    // self-loop targets at validation time.
    let archive = build_archive(|tar| {
      write_file_entry(tar, "build/bin/llama-server", b"binary");
      let mut header = Header::new_gnu();
      header.set_size(0);
      header.set_entry_type(EntryType::Symlink);
      header.set_link_name(".").unwrap();
      header.set_cksum();
      tar.append_data(&mut header, "loop", &[][..]).unwrap();
    });
    let dest = temp_dir("self-loop-symlink");
    let err = safe_extract_tar_gz(archive.as_slice(), &dest, "b9999", ExtractLimits::default())
      .unwrap_err();
    assert!(
      matches!(err, InstallError::UnsafeArchive { ref reason, .. } if reason.contains("strict subpath") || reason.contains("empty")),
      "expected self-loop refusal, got {err:?}"
    );
    std::fs::remove_dir_all(&dest).ok();
  }

  #[test]
  fn refuses_symlink_escaping_via_dotdot() {
    let archive = build_archive(|tar| {
      write_file_entry(tar, "build/bin/llama-server", b"binary");
      let mut header = Header::new_gnu();
      header.set_size(0);
      header.set_entry_type(EntryType::Symlink);
      header.set_link_name("../../../etc/passwd").unwrap();
      header.set_cksum();
      tar.append_data(&mut header, "evil-link", &[][..]).unwrap();
    });
    let dest = temp_dir("symlink-escape");
    let err = safe_extract_tar_gz(archive.as_slice(), &dest, "b9999", ExtractLimits::default())
      .unwrap_err();
    assert!(
      matches!(err, InstallError::UnsafeArchive { ref reason, .. } if reason.contains("escapes")),
      "expected dotdot-escape refusal, got {err:?}"
    );
    std::fs::remove_dir_all(&dest).ok();
  }

  #[cfg(unix)]
  #[test]
  fn accepts_in_archive_soname_symlink_chain() {
    // Mirrors the llama.cpp release shape: a regular `.so.X.Y.Z`
    // shared library with two symlinks forming the SONAME chain.
    let archive = build_archive(|tar| {
      write_file_entry(tar, "llama-b9999/llama-server", b"binary");
      write_file_entry(tar, "llama-b9999/libllama.so.0.0.9999", b"so contents");
      let mut h1 = Header::new_gnu();
      h1.set_size(0);
      h1.set_entry_type(EntryType::Symlink);
      h1.set_link_name("libllama.so.0.0.9999").unwrap();
      h1.set_cksum();
      tar
        .append_data(&mut h1, "llama-b9999/libllama.so.0", &[][..])
        .unwrap();
      let mut h2 = Header::new_gnu();
      h2.set_size(0);
      h2.set_entry_type(EntryType::Symlink);
      h2.set_link_name("libllama.so.0").unwrap();
      h2.set_cksum();
      tar
        .append_data(&mut h2, "llama-b9999/libllama.so", &[][..])
        .unwrap();
    });
    let dest = temp_dir("soname-chain");
    let out = safe_extract_tar_gz(archive.as_slice(), &dest, "b9999", ExtractLimits::default())
      .expect("extract");
    let dir = out.path.parent().unwrap();
    let unversioned = dir.join("libllama.so");
    let soname = dir.join("libllama.so.0");
    let real = dir.join("libllama.so.0.0.9999");
    assert!(
      std::fs::symlink_metadata(&unversioned)
        .unwrap()
        .file_type()
        .is_symlink(),
      "libllama.so should be a symlink"
    );
    assert!(std::fs::symlink_metadata(&soname)
      .unwrap()
      .file_type()
      .is_symlink());
    // The chain must resolve to a real file (dynamic linker would
    // follow it the same way at runtime).
    assert!(std::fs::metadata(&unversioned).unwrap().is_file());
    assert_eq!(
      std::fs::read(&real).unwrap(),
      b"so contents",
      "real file body must be preserved"
    );
    std::fs::remove_dir_all(&dest).ok();
  }

  #[test]
  fn refuses_archive_without_llama_server_entry() {
    let archive = build_archive(|tar| {
      write_file_entry(tar, "build/bin/some-other-binary", b"surprise");
    });
    let dest = temp_dir("no-binary");
    let err = safe_extract_tar_gz(archive.as_slice(), &dest, "b9999", ExtractLimits::default())
      .unwrap_err();
    assert!(
      matches!(err, InstallError::UnsafeArchive { ref reason, .. } if reason.contains("llama-server")),
      "expected llama-server-missing refusal, got {err:?}"
    );
    std::fs::remove_dir_all(&dest).ok();
  }

  /// Build a tar header that *declares* `size` uncompressed bytes
  /// without actually writing that many bytes to the archive. We use
  /// this for the per-entry and total-size cap tests so the test
  /// archive itself stays small (we don't want to allocate 1 GiB in a
  /// unit test). The cap checks rely solely on `entry.header().size()`
  /// before reading body bytes, so this exercises the production
  /// refusal path without paying the I/O cost of a real bomb.
  fn write_zero_body_with_declared_size(
    tar: &mut Builder<GzEncoder<Vec<u8>>>,
    path: &str,
    declared_size: u64,
  ) {
    let mut header = Header::new_gnu();
    header.set_size(declared_size);
    header.set_mode(0o755);
    header.set_entry_type(EntryType::Regular);
    header.set_cksum();
    // Append an empty body — the tar reader will still surface the
    // header's declared size, which is what the cap checks read.
    tar.append_data(&mut header, path, &[][..]).unwrap();
  }

  #[test]
  fn refuses_archive_exceeding_entry_count_cap() {
    let archive = build_archive(|tar| {
      // MAX_ENTRIES (10_000) + 1 = 10_001 should trip the cap on the
      // 10_001st iteration. Empty regular files keep the archive
      // small enough to test in milliseconds.
      for i in 0..(MAX_ENTRIES + 1) {
        write_file_entry(tar, &format!("e{i}"), b"");
      }
    });
    let dest = temp_dir("entry-count");
    let err = safe_extract_tar_gz(archive.as_slice(), &dest, "b9999", ExtractLimits::default())
      .unwrap_err();
    assert!(
      matches!(err, InstallError::UnsafeArchive { ref reason, .. } if reason.contains("entry count")),
      "expected entry-count cap refusal, got {err:?}"
    );
    std::fs::remove_dir_all(&dest).ok();
  }

  #[test]
  fn refuses_archive_with_single_entry_over_per_entry_cap() {
    let archive = build_archive(|tar| {
      // Declare more than the floor per-entry cap (2 GiB) on one
      // entry. The body bytes are empty; the cap check reads the
      // header's declared size.
      write_zero_body_with_declared_size(
        tar,
        "build/bin/llama-server",
        ExtractLimits::default().per_entry_bytes + 1,
      );
    });
    let dest = temp_dir("per-entry-cap");
    let err = safe_extract_tar_gz(archive.as_slice(), &dest, "b9999", ExtractLimits::default())
      .unwrap_err();
    assert!(
      matches!(err, InstallError::UnsafeArchive { ref reason, .. } if reason.contains("per-entry cap")),
      "expected per-entry cap refusal, got {err:?}"
    );
    std::fs::remove_dir_all(&dest).ok();
  }

  /// Measured on the real `llama-b11804-bin-ubuntu-rocm-10.0-x64.tar.gz`
  /// (249,908,117 bytes), whose biggest entry `libggml-hip.so` declares
  /// 1,172,883,432 bytes. The old flat 1 GiB per-entry cap refused the
  /// whole ROCm release (#119); caps derived from the asset's own size
  /// accept it.
  #[test]
  fn derived_limits_accept_the_real_rocm_blob_size() {
    let limits = ExtractLimits::for_asset(Some(249_908_117));
    let mut caps = Caps::new(limits);
    assert!(
      caps
        .charge("llama-b11804/libggml-hip.so", 1_172_883_432)
        .is_ok(),
      "the real ROCm blob must fit its derived caps: {limits:?}"
    );
    assert!(caps.charge("llama-b11804/llama-server", 5_282_968).is_ok());
  }

  #[test]
  fn derived_limits_scale_with_declared_asset_size_and_floor_at_two_gib() {
    assert_eq!(
      ExtractLimits::for_asset(None).total_bytes,
      MIN_TOTAL_UNCOMPRESSED_BYTES
    );
    assert_eq!(
      ExtractLimits::for_asset(Some(1024)).total_bytes,
      MIN_TOTAL_UNCOMPRESSED_BYTES
    );
    assert_eq!(
      ExtractLimits::for_asset(Some(1024 * 1024 * 1024)).total_bytes,
      16 * 1024 * 1024 * 1024
    );
  }

  /// The total cap is what bounds a bomb that spreads across many
  /// entries. Checked on `Caps` directly: authoring a multi-GiB archive
  /// in a unit test is CI-prohibitive, and a tar header that lies about
  /// its size is rejected structurally before the cap check fires.
  #[test]
  fn total_cap_refuses_once_exceeded() {
    let limits = ExtractLimits {
      total_bytes: 100,
      per_entry_bytes: 60,
      ..ExtractLimits::default()
    };
    let mut caps = Caps::new(limits);
    assert!(caps.charge("a", 60).is_ok());
    let err = caps.charge("b", 60).unwrap_err();
    assert!(
      matches!(&err, InstallError::UnsafeArchive { reason, .. } if reason.contains("-byte cap")),
      "expected total-cap refusal, got {err:?}"
    );
  }

  #[test]
  fn second_call_against_existing_dir_finds_binary_via_scan() {
    // First call extracts normally into `b9999/build/bin/llama-server`.
    // The second call must NOT trust the temp extraction's relative
    // path — it must scan `final_dir` directly. We force this by
    // changing the binary's relative path between calls.
    let archive_a = build_archive(|tar| {
      write_file_entry(tar, "build/bin/llama-server", b"v1");
    });
    let dest = temp_dir("existing");
    let out_a = safe_extract_tar_gz(
      archive_a.as_slice(),
      &dest,
      "b9999",
      ExtractLimits::default(),
    )
    .expect("first extract");
    assert!(out_a.path.is_file());
    // Second archive places the binary at a different relative
    // path. If the early-return code naively joined the new
    // tmp-relative path onto final_dir, it would point to a path
    // that doesn't exist.
    let archive_b = build_archive(|tar| {
      write_file_entry(tar, "binaries/llama-server", b"v1");
    });
    let out_b = safe_extract_tar_gz(
      archive_b.as_slice(),
      &dest,
      "b9999",
      ExtractLimits::default(),
    )
    .expect("second extract");
    assert!(
      out_b.path.is_file(),
      "early-return must locate the binary actually on disk, got {}",
      out_b.path.display()
    );
    // The returned path lives inside the pre-existing final_dir,
    // not the discarded temp extraction.
    assert!(out_b.path.starts_with(dest.join("b9999")));
    std::fs::remove_dir_all(&dest).ok();
  }

  #[test]
  fn safe_extract_dispatches_on_extension() {
    // Anything that isn't a known archive extension surfaces an
    // actionable refusal — callers should not silently pass through.
    let dest = temp_dir("dispatch-bad-ext");
    let src = on_disk("dispatch-bad-ext", b"not an archive");
    let err = safe_extract(
      "artifact.exe",
      &src,
      &dest,
      "b9999",
      ExtractLimits::default(),
    )
    .unwrap_err();
    assert!(
      matches!(err, InstallError::UnsafeArchive { ref reason, .. } if reason.contains("unsupported archive extension")),
      "expected unsupported-extension refusal, got {err:?}"
    );
    std::fs::remove_dir_all(&dest).ok();
  }

  #[test]
  fn safe_extract_routes_tar_gz_to_tar_path() {
    let archive = build_archive(|tar| {
      write_file_entry(tar, "build/bin/llama-server", b"#!/bin/sh\necho ok\n");
    });
    let dest = temp_dir("dispatch-targz");
    let src = on_disk("dispatch-targz", &archive);
    let out = safe_extract(
      "llama-b9999-bin-ubuntu-x64.tar.gz",
      &src,
      &dest,
      "b9999",
      ExtractLimits::default(),
    )
    .expect("tar.gz route");
    assert!(out.path.ends_with("build/bin/llama-server"));
    std::fs::remove_dir_all(&dest).ok();
  }

  #[cfg(not(windows))]
  #[test]
  fn safe_extract_refuses_zip_on_non_windows() {
    let dest = temp_dir("dispatch-zip-not-windows");
    let src = on_disk("dispatch-zip-not-windows", b"PK\x03\x04");
    let err = safe_extract(
      "llama-b9999-bin-win-cpu-x64.zip",
      &src,
      &dest,
      "b9999",
      ExtractLimits::default(),
    )
    .unwrap_err();
    assert!(
      matches!(err, InstallError::UnsafeArchive { ref reason, .. } if reason.contains("only supported on Windows")),
      "expected non-Windows zip refusal, got {err:?}"
    );
    std::fs::remove_dir_all(&dest).ok();
  }

  // Windows-only zip extraction tests live inside `cfg(windows)`
  // because the `zip` crate isn't compiled on other platforms (per
  // the Cargo.toml gate). The Windows CI lane exercises
  // them; locally on Linux they're a no-op.
  #[cfg(windows)]
  mod windows_zip {
    use super::*;
    use std::io::Cursor;
    use zip::write::{SimpleFileOptions, ZipWriter};

    fn build_zip<F: FnOnce(&mut ZipWriter<Cursor<Vec<u8>>>)>(f: F) -> Vec<u8> {
      let buf = Cursor::new(Vec::<u8>::new());
      let mut writer = ZipWriter::new(buf);
      f(&mut writer);
      writer.finish().unwrap().into_inner()
    }

    fn write_zip_file(writer: &mut ZipWriter<Cursor<Vec<u8>>>, path: &str, body: &[u8]) {
      use std::io::Write as _;
      writer
        .start_file(path, SimpleFileOptions::default())
        .unwrap();
      writer.write_all(body).unwrap();
    }

    #[test]
    fn extracts_well_formed_zip() {
      let archive = build_zip(|w| {
        write_zip_file(w, "llama-server.exe", b"MZ\x90\x00");
        write_zip_file(w, "README.txt", b"docs");
      });
      let dest = temp_dir("zip-happy");
      let out = safe_extract_zip(
        Cursor::new(archive),
        &dest,
        "b9999",
        ExtractLimits::default(),
      )
      .expect("zip extract");
      assert!(out.path.is_file(), "binary not extracted");
      assert!(out.path.ends_with("llama-server.exe"));
      std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn refuses_zip_without_llama_server_entry() {
      let archive = build_zip(|w| {
        write_zip_file(w, "other.exe", b"MZ");
      });
      let dest = temp_dir("zip-no-binary");
      let err = safe_extract_zip(
        Cursor::new(archive),
        &dest,
        "b9999",
        ExtractLimits::default(),
      )
      .unwrap_err();
      assert!(
        matches!(err, InstallError::UnsafeArchive { ref reason, .. } if reason.contains("llama-server.exe")),
        "expected missing-binary refusal, got {err:?}"
      );
      std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn refuses_zip_with_traversal_entry() {
      // The `zip` crate's `enclosed_name()` already rejects `..`
      // segments. Verify the resulting UnsafeArchive error path is
      // what production callers see.
      let archive = build_zip(|w| {
        write_zip_file(w, "../escape.exe", b"evil");
      });
      let dest = temp_dir("zip-traversal");
      let err = safe_extract_zip(
        Cursor::new(archive),
        &dest,
        "b9999",
        ExtractLimits::default(),
      )
      .unwrap_err();
      assert!(
        matches!(err, InstallError::UnsafeArchive { .. }),
        "expected traversal refusal, got {err:?}"
      );
      std::fs::remove_dir_all(&dest).ok();
    }

    #[test]
    fn dispatch_routes_zip_to_zip_path() {
      let archive = build_zip(|w| {
        write_zip_file(w, "llama-server.exe", b"MZ");
      });
      let dest = temp_dir("dispatch-zip");
      let src = on_disk("dispatch-zip", &archive);
      let out = safe_extract(
        "llama-b9999-bin-win-cpu-x64.zip",
        &src,
        &dest,
        "b9999",
        ExtractLimits::default(),
      )
      .expect("zip route");
      assert!(out.path.ends_with("llama-server.exe"));
      std::fs::remove_dir_all(&dest).ok();
    }
  }

  #[test]
  fn early_return_refuses_when_existing_dir_lacks_binary() {
    // A user (or partial prior install) left a versioned dir on
    // disk with no `llama-server` inside. The early-return must
    // surface an actionable error, not a phantom path.
    let dest = temp_dir("empty-existing");
    let final_dir = dest.join("b9999");
    std::fs::create_dir_all(final_dir.join("docs")).unwrap();
    std::fs::write(final_dir.join("docs/README.md"), b"no binary here").unwrap();

    let archive = build_archive(|tar| {
      write_file_entry(tar, "build/bin/llama-server", b"new binary");
    });
    let err = safe_extract_tar_gz(archive.as_slice(), &dest, "b9999", ExtractLimits::default())
      .unwrap_err();
    assert!(
      matches!(err, InstallError::UnsafeArchive { ref reason, .. } if reason.contains("llama-server")),
      "expected actionable missing-binary error, got {err:?}"
    );
    std::fs::remove_dir_all(&dest).ok();
  }
}
