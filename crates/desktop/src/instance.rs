//! Keep one server window per local user, including while its listener is stopped.
use eframe::egui;
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub(crate) struct ServerInstance {
    // Keep the inode and handle for the whole app lifetime. Removing the lock
    // file on exit would let competing launches lock different inodes.
    _lock: File,
    activation: PathBuf,
    next_poll: Instant,
}

fn private_file(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

impl ServerInstance {
    /// A second launch signals the owner and exits before creating a window or
    /// touching the LAN listener, pairing file, or native Codex threads.
    pub(crate) fn acquire(directory: &Path) -> io::Result<Option<Self>> {
        fs::create_dir_all(directory)?;
        let lock = private_file(&directory.join("server.lock"))?;
        let activation = directory.join("server.activate");
        match lock.try_lock() {
            Ok(()) => {
                let _ = fs::remove_file(&activation);
                Ok(Some(Self {
                    _lock: lock,
                    activation,
                    next_poll: Instant::now(),
                }))
            }
            Err(TryLockError::WouldBlock) => {
                drop(private_file(&activation)?);
                Ok(None)
            }
            Err(TryLockError::Error(error)) => Err(error),
        }
    }

    fn take_activation(&self) -> bool {
        fs::remove_file(&self.activation).is_ok()
    }

    pub(crate) fn poll(&mut self, ctx: &egui::Context) {
        if Instant::now() < self.next_poll {
            return;
        }
        self.next_poll = Instant::now() + Duration::from_millis(250);
        if self.take_activation() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            // Some Wayland compositors disallow programmatic focus. Their dock
            // can still highlight the existing window for the user.
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
                egui::UserAttentionType::Informational,
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn competing_launches_signal_owner_and_later_launches_recover_the_lock() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("codex-lan-instance-{}-{nonce}", std::process::id()));
        let owner = ServerInstance::acquire(&directory).unwrap().unwrap();
        assert!(!owner.take_activation());
        for _ in 0..3 {
            assert!(ServerInstance::acquire(&directory).unwrap().is_none());
            assert!(owner.take_activation());
            assert!(!owner.take_activation());
        }
        drop(owner);
        // A stale activation marker and the persistent lock file must not
        // prevent a new owner after a normal exit or process termination.
        drop(private_file(&directory.join("server.activate")).unwrap());
        let next = ServerInstance::acquire(&directory).unwrap().unwrap();
        assert!(!next.take_activation());
        drop(next);
        fs::remove_dir_all(directory).unwrap();
    }
}
