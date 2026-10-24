//! Remote side of the resumable session: `--x-mosh-*` flag routing and the keeper
//! rendezvous contract shared by attacher and keeper.
//!
//! Pure-pipe rule: once connected, the attacher only pumps bytes. Its one own message is
//! `END{"keeper-unavailable"}` on bring-up failure; the session protocol starts with `HELLO`
//! end to end. The peer-uid check on the keeper socket is the attacher's (mosh-dqw.10).
//!
//! Rendezvous: the attacher resolves the paths in its own environment, and the keeper it
//! spawns inherits that environment, so both name the same socket.

#[cfg(unix)]
mod attacher;
#[cfg(unix)]
mod keeper;

use std::io;

/// Routes the `--x-mosh-*` flags of `remote-client-bridge`. `None` means "not a mosh flag".
/// A new flag is a match arm. Every other `--x-mosh-*` flag (and every flag on non-Unix) is
/// an error with upstream's message, never a plain pump.
pub(in crate::remote) fn dispatch(args: &[String]) -> Option<io::Result<()>> {
    match args {
        #[cfg(unix)]
        [flag] if flag == super::SESSION_FLAG => Some(attacher::run(args)),
        #[cfg(unix)]
        [flag] if flag == super::KEEPER_FLAG => Some(keeper::run(args)),
        [flag, ..] if flag.starts_with("--x-mosh-") => Some(Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported remote client bridge option",
        ))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn non_mosh_args_are_not_routed() {
        assert!(dispatch(&[]).is_none());
        assert!(dispatch(&args(&["--idle-timeout-v1"])).is_none());
    }

    #[test]
    fn unknown_or_malformed_mosh_flags_are_errors() {
        for list in [
            &["--x-mosh-bogus-v9"][..],
            &[super::super::SESSION_FLAG, "extra"][..],
            &[super::super::KEEPER_FLAG, "extra"][..],
        ] {
            let result = dispatch(&args(list)).expect("mosh flag must be routed");
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput, "{list:?}");
        }
    }

    #[test]
    fn status_hook_advertises_capability() {
        let status = include_str!("../../cli/status.rs");
        let hook = format!("\"{}\", // mosh-fork hook", super::super::STATUS_CAPABILITY);
        assert!(status.contains(&hook), "status.rs lacks {hook}");
    }
}

/// Keeper rendezvous contract (unix only: uses `flock`, owner and mode checks).
#[cfg(unix)]
pub(in crate::remote::attach::mosh) mod rendezvous {
    use sha2::{Digest, Sha256};
    use std::fs::{self, File, OpenOptions};
    use std::io;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    use std::os::unix::io::AsRawFd;
    use std::path::{Path, PathBuf};

    /// Paths of one keeper's rendezvous point.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(in crate::remote::attach::mosh) struct KeeperRendezvous {
        /// Private (0700, euid-owned) directory holding the other three.
        pub dir: PathBuf,
        /// Keeper listening socket (mode 0600 when bound).
        pub socket: PathBuf,
        /// Held exclusively by the keeper for its lifetime.
        pub live_lock: PathBuf,
        /// Serialises keeper spawning.
        pub spawn_lock: PathBuf,
    }

    fn fits_sun_path(path: &Path) -> bool {
        use std::os::unix::ffi::OsStrExt;
        path.as_os_str().as_bytes().len() <= 103
    }

    fn geteuid() -> u32 {
        // SAFETY: geteuid takes no pointers and has no preconditions.
        unsafe { libc::geteuid() }
    }

    pub(super) fn current_build_id() -> String {
        format!(
            "{}+{}",
            crate::build_info::version(),
            crate::build_info::build_id().unwrap_or("")
        )
    }

    /// First 24 hex digits of SHA-256 over the length-prefixed fields.
    pub(super) fn keeper_hash(
        euid: u32,
        session: &str,
        version: u32,
        build_id: &str,
        client_socket: &Path,
    ) -> String {
        use std::os::unix::ffi::OsStrExt;
        let mut hasher = Sha256::new();
        let mut field = |bytes: &[u8]| {
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(bytes);
        };
        field(&euid.to_le_bytes());
        field(session.as_bytes());
        field(&version.to_le_bytes());
        field(build_id.as_bytes());
        field(client_socket.as_os_str().as_bytes());
        let digest = hasher.finalize();
        digest.iter().take(12).map(|b| format!("{b:02x}")).collect()
    }

    pub(super) fn rendezvous_paths(temp_dir: &Path, euid: u32, hash: &str) -> KeeperRendezvous {
        let name = format!("herdr-mosh-{euid}");
        let socket_name = format!("k-{hash}.sock");
        let mut dir = temp_dir.join(&name);
        if !fits_sun_path(&dir.join(&socket_name)) {
            dir = Path::new("/tmp").join(&name);
        }
        KeeperRendezvous {
            socket: dir.join(socket_name),
            live_lock: dir.join(format!("k-{hash}.live")),
            spawn_lock: dir.join(format!("k-{hash}.spawn")),
            dir,
        }
    }

    /// Resolve the rendezvous for `session` in the current environment.
    pub(in crate::remote::attach::mosh) fn resolve(session: &str) -> KeeperRendezvous {
        let euid = geteuid();
        let hash = keeper_hash(
            euid,
            session,
            crate::remote::attach::mosh::session::SESSION_PROTOCOL_VERSION,
            &current_build_id(),
            &crate::server::socket_paths::client_socket_path(),
        );
        rendezvous_paths(&std::env::temp_dir(), euid, &hash)
    }

    /// Reject anything but a real directory, owned by us, with mode exactly 0700.
    pub(in crate::remote::attach::mosh) fn validate_private_dir(dir: &Path) -> io::Result<()> {
        validate_private_dir_for(dir, geteuid())
    }

    pub(super) fn validate_private_dir_for(dir: &Path, euid: u32) -> io::Result<()> {
        let metadata = fs::symlink_metadata(dir)?;
        let reject = |why: &str| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("untrusted mosh rendezvous directory {}: {why}", dir.display()),
            ))
        };
        if !metadata.is_dir() {
            return reject("not a directory");
        }
        if metadata.uid() != euid {
            return reject("owned by another user");
        }
        if metadata.mode() & 0o7777 != 0o700 {
            return reject("mode is not 0700");
        }
        Ok(())
    }

    /// Create the private directory if missing, then validate it.
    pub(in crate::remote::attach::mosh) fn ensure_private_dir(dir: &Path) -> io::Result<()> {
        match crate::platform::create_remote_private_dir(dir) {
            Ok(()) => {}
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err),
        }
        validate_private_dir(dir)
    }

    /// Advisory exclusive lock; released when dropped (the fd closes).
    #[derive(Debug)]
    pub(in crate::remote::attach::mosh) struct RendezvousLock {
        _file: File,
    }

    fn open_lock(path: &Path) -> io::Result<File> {
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(path)
    }

    fn flock(file: &File, op: libc::c_int) -> io::Result<bool> {
        // SAFETY: this live File owns the descriptor; flock neither consumes it nor
        // dereferences pointers. Drop releases the advisory lock automatically.
        if unsafe { libc::flock(file.as_raw_fd(), op) } == 0 {
            return Ok(true);
        }
        let err = io::Error::last_os_error();
        if op & libc::LOCK_NB != 0 && err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(false)
        } else {
            Err(err)
        }
    }

    /// Non-blocking: `None` when another open file description holds the lock.
    pub(in crate::remote::attach::mosh) fn try_lock(path: &Path) -> io::Result<Option<RendezvousLock>> {
        let file = open_lock(path)?;
        Ok(flock(&file, libc::LOCK_EX | libc::LOCK_NB)?.then_some(RendezvousLock { _file: file }))
    }

    /// Blocking variant of `try_lock`.
    pub(in crate::remote::attach::mosh) fn lock(path: &Path) -> io::Result<RendezvousLock> {
        let file = open_lock(path)?;
        flock(&file, libc::LOCK_EX)?;
        Ok(RendezvousLock { _file: file })
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::os::unix::fs::{symlink, PermissionsExt};
        use std::sync::atomic::{AtomicU32, Ordering};

        struct TestDir(PathBuf);

        impl TestDir {
            fn new(name: &str) -> TestDir {
                static COUNTER: AtomicU32 = AtomicU32::new(0);
                let n = COUNTER.fetch_add(1, Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "mosh-rdv-test-{}-{n}-{name}",
                    std::process::id()
                ));
                let _ = fs::remove_dir_all(&path);
                TestDir(path)
            }
        }

        impl Drop for TestDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn private_dir_created_and_idempotent() {
            let t = TestDir::new("ok");
            ensure_private_dir(&t.0).unwrap();
            validate_private_dir(&t.0).unwrap();
            ensure_private_dir(&t.0).unwrap();
        }

        #[test]
        fn wrong_mode_rejected() {
            let t = TestDir::new("mode");
            ensure_private_dir(&t.0).unwrap();
            fs::set_permissions(&t.0, fs::Permissions::from_mode(0o755)).unwrap();
            let err = validate_private_dir(&t.0).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
            assert!(ensure_private_dir(&t.0).is_err());
        }

        #[test]
        fn symlink_rejected() {
            let t = TestDir::new("link-target");
            ensure_private_dir(&t.0).unwrap();
            let link = TestDir::new("link");
            symlink(&t.0, &link.0).unwrap();
            // The remove in TestDir::drop uses remove_dir_all, which unlinks a symlink.
            let err = validate_private_dir(&link.0).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        fn foreign_owner_rejected() {
            let t = TestDir::new("owner");
            ensure_private_dir(&t.0).unwrap();
            validate_private_dir_for(&t.0, geteuid()).unwrap();
            let err = validate_private_dir_for(&t.0, geteuid().wrapping_add(1)).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        fn regular_file_rejected() {
            let t = TestDir::new("file");
            fs::write(&t.0, b"x").unwrap();
            fs::set_permissions(&t.0, fs::Permissions::from_mode(0o700)).unwrap();
            let err = validate_private_dir(&t.0).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        }

        #[test]
        fn hash_stable_and_sensitive_to_each_input() {
            let sock = Path::new("/run/herdr/client.sock");
            let base = keeper_hash(1000, "s", 1, "b", sock);
            assert_eq!(base, keeper_hash(1000, "s", 1, "b", sock));
            assert_eq!(base.len(), 24);
            assert!(base.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
            assert_ne!(base, keeper_hash(1001, "s", 1, "b", sock));
            assert_ne!(base, keeper_hash(1000, "t", 1, "b", sock));
            assert_ne!(base, keeper_hash(1000, "s", 2, "b", sock));
            assert_ne!(base, keeper_hash(1000, "s", 1, "c", sock));
            assert_ne!(base, keeper_hash(1000, "s", 1, "b", Path::new("/other.sock")));
            // Field boundaries are unambiguous.
            assert_ne!(
                keeper_hash(1000, "ab", 1, "c", sock),
                keeper_hash(1000, "a", 1, "bc", sock)
            );
        }

        #[test]
        fn paths_use_temp_dir_when_short() {
            let hash = "0123456789abcdef01234567";
            let r = rendezvous_paths(Path::new("/tmp/short"), 1000, hash);
            assert_eq!(r.dir, Path::new("/tmp/short/herdr-mosh-1000"));
            assert_eq!(r.socket, r.dir.join("k-0123456789abcdef01234567.sock"));
            assert_eq!(r.live_lock, r.dir.join("k-0123456789abcdef01234567.live"));
            assert_eq!(r.spawn_lock, r.dir.join("k-0123456789abcdef01234567.spawn"));
            assert!(fits_sun_path(&r.socket));
        }

        #[test]
        fn paths_fall_back_to_tmp_when_long() {
            let hash = "0123456789abcdef01234567";
            let long = PathBuf::from(format!("/{}", "a".repeat(200)));
            let r = rendezvous_paths(&long, 1000, hash);
            assert_eq!(r.dir, Path::new("/tmp/herdr-mosh-1000"));
            assert!(fits_sun_path(&r.socket));
            assert!(r.socket.starts_with(&r.dir));
        }

        #[test]
        fn resolve_names_are_consistent() {
            let r = resolve("sess");
            assert_eq!(r, resolve("sess"));
            assert_ne!(r.socket, resolve("other").socket);
            assert!(fits_sun_path(&r.socket));
        }

        #[test]
        fn try_lock_is_exclusive_until_dropped() {
            let t = TestDir::new("lock");
            ensure_private_dir(&t.0).unwrap();
            let path = t.0.join("k.live");
            let first = try_lock(&path).unwrap();
            assert!(first.is_some());
            assert!(try_lock(&path).unwrap().is_none());
            drop(first);
            assert!(try_lock(&path).unwrap().is_some());
            drop(lock(&path).unwrap());
        }
    }
}
