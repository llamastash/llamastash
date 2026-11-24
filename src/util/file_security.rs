//! Cross-platform hardening of secret files (`runtime.json`, `state.json`)
//! against other accounts. Unix hardening is the `chmod 0o600` in
//! [`crate::util::atomic_write`]. On Windows there is no mode-bit equivalent,
//! so we apply a Protected DACL that grants Generic-All to the file owner and
//! no inheritance from the parent. This module also holds, on Unix, the shared
//! directory swap-surface check
//! ([`dir_swap_surface`]) used by the init binary-adoption, config-write,
//! and doctor preflights.

use std::path::Path;

/// Apply an owner-only DACL to `path` on Windows (no-op on Unix —
/// caller's `chmod` already hardened the file). Best-effort: failures
/// are swallowed with a warning. Safe to call repeatedly.
#[cfg(windows)]
pub fn set_owner_only_dacl(path: &Path) {
  use std::os::windows::ffi::OsStrExt;
  use windows_sys::Win32::Foundation::LocalFree;
  use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
  };
  use windows_sys::Win32::Security::{SetFileSecurityW, DACL_SECURITY_INFORMATION};

  // Owner = Generic All, Protected DACL (no inheritance from parent).
  // `D:` is the DACL section, `P` is Protected, and `(A;;GA;;;OW)` is the
  // Allow ACE granting Generic All to the OWner. Swallowing a failure is
  // safe: the state dir sits under `%LOCALAPPDATA%`, which already inherits
  // a per-user ACL, so this only guards a misconfigured parent ACL.
  let sddl: Vec<u16> = "D:P(A;;GA;;;OW)\0".encode_utf16().collect();
  let mut sd_ptr: windows_sys::Win32::Security::PSECURITY_DESCRIPTOR = std::ptr::null_mut();
  // SAFETY: SDDL is a NUL-terminated UTF-16 string; sd_ptr receives a
  // freshly-allocated security descriptor we LocalFree below.
  let built = unsafe {
    ConvertStringSecurityDescriptorToSecurityDescriptorW(
      sddl.as_ptr(),
      SDDL_REVISION_1,
      &mut sd_ptr,
      std::ptr::null_mut(),
    )
  };
  if built == 0 {
    log::warn!(
      "could not build security descriptor for {}: {}",
      path.display(),
      std::io::Error::last_os_error()
    );
    return;
  }
  let path_w: Vec<u16> = path
    .as_os_str()
    .encode_wide()
    .chain(std::iter::once(0))
    .collect();
  // SAFETY: sd_ptr is a valid SD from the call above; path_w is
  // NUL-terminated UTF-16; SetFileSecurityW reads both and returns
  // BOOL.
  let applied = unsafe { SetFileSecurityW(path_w.as_ptr(), DACL_SECURITY_INFORMATION, sd_ptr) };
  if applied == 0 {
    log::warn!(
      "could not apply owner-only DACL to {}: {}",
      path.display(),
      std::io::Error::last_os_error()
    );
  }
  // SAFETY: sd_ptr was allocated by ConvertStringSecurityDescriptorToSecurityDescriptorW
  // per its documented contract: caller must LocalFree.
  unsafe {
    LocalFree(sd_ptr as _);
  }
}

#[cfg(not(windows))]
pub fn set_owner_only_dacl(_path: &Path) {
  // Unix files are hardened via `chmod 0o600` in atomic_write::write_secure.
}

/// Why `path` is a swap surface, or nothing when it is safe to hold
/// trusted files. A swap surface lets a user other than the directory's
/// owner rename or replace a file inside it. An attacker could use that
/// to substitute the adopted `llama-server` or a `0600` config.
///
/// The rule is **owner-aware**: what matters is whether a user *other
/// than the directory's owner* can write into it, not the raw mode bits.
/// - A world-writable directory admits any account, owned or not.
/// - A group-writable directory hands write to its group's members. That
///   is safe only if the group is a **user-private group** — one holding
///   nothing but that user (`is_user_private_group`) — which is the
///   `user:user` home-dir default the owner's 002 umask produces. Refusing
///   a private group would block legitimate installs like a `mise` manage
///   tree under `~/.local/share`.
/// - Any directory owned by a non-root account other than the caller is
///   controlled by that account regardless of mode bits (the owner can
///   always `chmod`/rename inside it), so it is a swap surface too.
///
/// Missing/metadata-failing paths report `None`: a path that can't be
/// statted isn't a directory we can adopt into anyway.
#[cfg(unix)]
pub fn dir_swap_surface(path: &Path, our_uid: u32) -> Option<SwapSurface> {
  use std::os::unix::fs::{MetadataExt, PermissionsExt};
  let meta = std::fs::metadata(path).ok()?;
  let owner = meta.uid();
  let mode = meta.permissions().mode() & 0o777;
  // Only this case reads the answer; skipping the lookup otherwise saves an
  // NSS round trip (LDAP/SSSD) per call.
  let private_group =
    owner == our_uid && mode & 0o020 != 0 && is_user_private_group(meta.gid(), our_uid);
  SwapSurface::for_perm(owner, mode, our_uid, private_group)
}

/// True when `gid` is a user-private group for `uid`: the group's name is
/// the user's own, it is the user's primary group, and it lists no member
/// besides that user. The same test Debian's `USERGROUPS_ENAB` uses.
///
/// This is the premise the owner-aware rule rests on — "a group-writable
/// dir you own is safe because its group contains no one else" only holds
/// for a group like this. It fails on macOS, where every local user's
/// primary group is `staff` (gid 20), and on Linux hosts with a shared
/// primary group such as `users`: a `you:staff` 0775 dir lets every other
/// local user replace the adopted `llama-server` or the `0600` config.
///
/// Fails closed — a missing passwd/group entry, an unreadable NSS source,
/// or a buffer too small for the entry all report `false`, so a group we
/// cannot prove private is treated as shared.
#[cfg(unix)]
pub(crate) fn is_user_private_group(gid: libc::gid_t, uid: libc::uid_t) -> bool {
  use std::ffi::CStr;
  // The `_r` calls write into caller buffers and signal ERANGE by leaving
  // `*result` null rather than by overflowing; these sizes clear every
  // realistic passwd/group entry.
  let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
  let mut grp: libc::group = unsafe { std::mem::zeroed() };
  let mut pbuf = vec![0 as libc::c_char; 4096];
  let mut gbuf = vec![0 as libc::c_char; 16384];
  let mut pres = std::ptr::null_mut();
  let mut gres = std::ptr::null_mut();
  // SAFETY: each call fills its struct and at most `buf.len()` c_chars,
  // writing `*result` last; every buffer outlives the pointers below.
  unsafe {
    if libc::getpwuid_r(uid, &mut pwd, pbuf.as_mut_ptr(), pbuf.len(), &mut pres) != 0
      || pres.is_null()
      || pwd.pw_name.is_null()
    {
      return false;
    }
    if libc::getgrgid_r(gid, &mut grp, gbuf.as_mut_ptr(), gbuf.len(), &mut gres) != 0
      || gres.is_null()
      || grp.gr_name.is_null()
    {
      return false;
    }
    let user = CStr::from_ptr(pwd.pw_name);
    if pwd.pw_gid != gid || CStr::from_ptr(grp.gr_name) != user {
      return false;
    }
    let mut m = grp.gr_mem;
    while !(*m).is_null() {
      if CStr::from_ptr(*m) != user {
        return false;
      }
      m = m.add(1);
    }
  }
  true
}

/// Classification of a directory that lets a non-owner write into it.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapSurface {
  /// Owned by another non-root account: that account controls it
  /// regardless of mode bits.
  ForeignOwner { owner: u32, mode: u32 },
  /// World-write bit set: any account on the machine can write.
  WorldWritable { mode: u32 },
  /// Group-write bit set on a directory whose group is not the caller's
  /// user-private group: its members (root's group, macOS `staff`, a
  /// shared `users`) can write.
  GroupWritable { owner: u32, mode: u32 },
}

#[cfg(unix)]
impl SwapSurface {
  fn for_perm(owner: u32, mode: u32, our_uid: u32, private_group: bool) -> Option<SwapSurface> {
    if owner != our_uid && owner != 0 {
      return Some(SwapSurface::ForeignOwner { owner, mode });
    }
    if mode & 0o002 != 0 {
      return Some(SwapSurface::WorldWritable { mode });
    }
    // Group-write is only the owner's own business when the directory is
    // theirs *and* the group is theirs alone; a root-owned or shared-group
    // dir hands write access to accounts that aren't the owner.
    if mode & 0o020 != 0 && !(owner == our_uid && private_group) {
      return Some(SwapSurface::GroupWritable { owner, mode });
    }
    None
  }

  /// One-line reason for the calling preflight's error/warning message.
  pub fn describe(self, our_uid: u32) -> String {
    match self {
      SwapSurface::ForeignOwner { owner, .. } => format!(
        "is owned by UID {owner} (neither you, UID {our_uid}, nor root); that account controls it"
      ),
      SwapSurface::WorldWritable { mode } => format!(
        "is world-writable (mode {mode:#o}), so any user could replace a file inside it; \
         run `chmod go-w` on it"
      ),
      SwapSurface::GroupWritable { mode, .. } => format!(
        "is group-writable (mode {mode:#o}) and its group is not your user-private group, \
         so other members of that group could replace a file inside it; run `chmod g-w` on it"
      ),
    }
  }
}

#[cfg(all(test, unix))]
mod tests_unix {
  use super::*;
  use std::fs;
  use std::os::unix::fs::{MetadataExt, PermissionsExt};

  #[test]
  fn for_perm_accepts_self_owned_group_writable_dir() {
    // The reported false positive: a group-writable directory you own
    // (the home-dir / `mise` installs case) has no other user who can
    // write, so it is not a swap surface.
    assert_eq!(
      SwapSurface::for_perm(1000, 0o775, 1000, true),
      None,
      "self-owned group-writable dir in a user-private group must be safe"
    );
    assert_eq!(
      SwapSurface::for_perm(1000, 0o755, 1000, false),
      None,
      "plain self-owned dir must be safe"
    );
  }

  #[test]
  fn for_perm_refuses_self_owned_group_writable_dir_on_a_shared_group() {
    // macOS: every local user's primary group is `staff`, so a `you:staff`
    // 0775 dir lets every other local user replace the file inside it.
    // Same for a shared primary group like `users` on Linux.
    assert_eq!(
      SwapSurface::for_perm(1000, 0o775, 1000, false),
      Some(SwapSurface::GroupWritable {
        owner: 1000,
        mode: 0o775
      }),
      "self-owned is not enough when the group is shared"
    );
  }

  #[test]
  fn for_perm_refuses_world_writable_regardless_of_owner() {
    assert_eq!(
      SwapSurface::for_perm(1000, 0o777, 1000, true),
      Some(SwapSurface::WorldWritable { mode: 0o777 })
    );
    // Even root-owned, world-write admits any account.
    assert_eq!(
      SwapSurface::for_perm(0, 0o777, 1000, true),
      Some(SwapSurface::WorldWritable { mode: 0o777 })
    );
  }

  #[test]
  fn for_perm_refuses_group_writable_foreign_owned_dir() {
    // A root-owned, group-writable dir hands write to a group of other
    // accounts; a dir owned by another non-root user is theirs outright.
    assert_eq!(
      SwapSurface::for_perm(0, 0o775, 1000, false),
      Some(SwapSurface::GroupWritable {
        owner: 0,
        mode: 0o775
      })
    );
    assert_eq!(
      SwapSurface::for_perm(999, 0o755, 1000, false),
      Some(SwapSurface::ForeignOwner {
        owner: 999,
        mode: 0o755
      }),
      "a dir owned by another account is controlled by them even at 0755"
    );
  }

  #[test]
  fn user_private_group_fails_closed() {
    let uid = unsafe { libc::geteuid() };
    assert!(
      !is_user_private_group(0, uid),
      "root's group is never private to you"
    );
    assert!(
      !is_user_private_group(0x7fff_fffe, uid),
      "an unresolvable gid must fail closed"
    );
  }

  #[test]
  fn real_dir_matches_the_predicate() {
    let dir = crate::util::test_temp::unique_temp_dir("swap-surface");
    let our_uid = unsafe { libc::geteuid() };
    // A new dir takes its parent's group on macOS/BSD, not the egid.
    let dir_gid = fs::metadata(&dir).unwrap().gid();

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(dir_swap_surface(&dir, our_uid), None);

    // Which way 0775 goes depends on the dir's group: a private
    // `user:user` accepts it, a shared `staff`/`users` refuses it.
    let private = is_user_private_group(dir_gid, our_uid);
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o775)).unwrap();
    assert_eq!(
      dir_swap_surface(&dir, our_uid),
      if private {
        None
      } else {
        Some(SwapSurface::GroupWritable {
          owner: our_uid,
          mode: 0o775,
        })
      },
      "self-owned 0775 follows the primary-group test"
    );

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(
      dir_swap_surface(&dir, our_uid),
      Some(SwapSurface::WorldWritable { mode: 0o777 })
    );

    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(
      dir_swap_surface(&dir.join("does-not-exist"), our_uid),
      None,
      "an unstat-able path skips the check"
    );
    fs::remove_dir_all(&dir).ok();
  }
}

#[cfg(all(test, windows))]
mod tests_windows {
  use super::*;
  use std::time::{SystemTime, UNIX_EPOCH};

  fn temp_path(label: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .expect("clock")
      .as_nanos();
    let p = std::env::temp_dir().join(format!(
      "llamastash-dacl-{label}-{}-{nanos}.tmp",
      std::process::id()
    ));
    std::fs::write(&p, b"secret").expect("seed file");
    p
  }

  #[test]
  fn set_owner_only_dacl_no_panic_on_existing_file() {
    // We don't try to verify the resulting DACL programmatically here —
    // that requires more Win32 plumbing than the test deserves. The
    // contract is "best-effort, no panic, no harm to the file";
    // verifying the call returns without panicking and that the file
    // is still readable is enough.
    let p = temp_path("apply");
    set_owner_only_dacl(&p);
    let read = std::fs::read(&p).expect("file still readable by owner");
    assert_eq!(read, b"secret");
    std::fs::remove_file(&p).ok();
  }

  #[test]
  fn set_owner_only_dacl_no_panic_on_missing_file() {
    // Best-effort contract: missing file just logs a warning.
    let p = temp_path("missing");
    std::fs::remove_file(&p).expect("remove");
    set_owner_only_dacl(&p);
  }
}
