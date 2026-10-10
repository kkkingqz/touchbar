//! Event-driven watching for the user profile document.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};

const WATCH_MASK: u32 = libc::IN_CLOSE_WRITE
    | libc::IN_MOVED_TO
    | libc::IN_MOVED_FROM
    | libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_ATTRIB
    | libc::IN_DELETE_SELF
    | libc::IN_MOVE_SELF;

pub struct ProfileWatcher {
    target: PathBuf,
    sources: BTreeSet<PathBuf>,
    attempts: BTreeSet<PathBuf>,
    directories: BTreeMap<PathBuf, i32>,
    fd: OwnedFd,
}

impl ProfileWatcher {
    pub fn new(target: &Path) -> Result<Self> {
        let target = if target.is_absolute() {
            target.to_path_buf()
        } else {
            std::env::current_dir()?.join(target)
        };
        target
            .file_name()
            .context("profile watch target needs a file name")?;
        let fd = create_watch()?;
        let mut watcher = Self {
            sources: BTreeSet::from([target.clone()]),
            target,
            attempts: BTreeSet::new(),
            directories: BTreeMap::new(),
            fd,
        };
        watcher.rearm_to_nearest_parent()?;
        Ok(watcher)
    }

    pub fn replace_sources(&mut self, mut sources: BTreeSet<PathBuf>) -> Result<()> {
        if sources.len() > 64 {
            bail!("too many profile watch sources");
        }
        sources.insert(self.target.clone());
        self.sources = sources;
        self.attempts.clear();
        self.rearm_to_nearest_parent()
    }
    pub fn observe_attempts(&mut self, attempts: BTreeSet<PathBuf>) -> Result<()> {
        if attempts.len() > 64 {
            bail!("too many requested profile watch sources");
        }
        self.attempts = attempts;
        self.rearm_to_nearest_parent()
    }

    pub fn notification_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Drains all queued filesystem events. Any event in the narrowest
    /// existing ancestor is sufficient: the profile loader performs strict
    /// validation, and rearming descends as newly created directories appear.
    pub fn drain(&mut self) -> Result<bool> {
        let mut changed = false;
        let mut bytes = [0_u8; 4096];
        loop {
            let count =
                unsafe { libc::read(self.fd.as_raw_fd(), bytes.as_mut_ptr().cast(), bytes.len()) };
            if count > 0 {
                changed = true;
                let mut offset = 0;
                while offset + std::mem::size_of::<libc::inotify_event>() <= count as usize {
                    // SAFETY: a complete kernel event header fits; byte storage may be unaligned.
                    let event = unsafe {
                        std::ptr::read_unaligned(
                            bytes.as_ptr().add(offset).cast::<libc::inotify_event>(),
                        )
                    };
                    if event.mask & (libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF)
                        != 0
                    {
                        self.directories.retain(|_, wd| *wd != event.wd);
                        if event.mask & libc::IN_IGNORED == 0 {
                            // SAFETY: remove the moved/deleted directory's own live watch.
                            unsafe {
                                libc::inotify_rm_watch(self.fd.as_raw_fd(), event.wd);
                            }
                        }
                    }
                    offset += std::mem::size_of::<libc::inotify_event>() + event.len as usize;
                }
                continue;
            }
            if count == 0 {
                bail!("profile filesystem watch closed unexpectedly");
            }
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::WouldBlock {
                break;
            }
            if error.kind() != std::io::ErrorKind::Interrupted {
                return Err(error).context("read profile filesystem events");
            }
        }
        if changed {
            self.rearm_to_nearest_parent()?;
        }
        Ok(changed)
    }

    fn rearm_to_nearest_parent(&mut self) -> Result<()> {
        let directories = self
            .sources
            .union(&self.attempts)
            .map(|source| {
                nearest_existing_directory(source.parent().context("profile source lacks parent")?)
            })
            .collect::<Result<BTreeSet<_>>>()?;
        let obsolete: Vec<_> = self
            .directories
            .keys()
            .filter(|path| !directories.contains(*path))
            .cloned()
            .collect();
        for path in obsolete {
            if let Some(watch) = self.directories.remove(&path) {
                // SAFETY: this is a watch identifier owned by this inotify fd.
                unsafe {
                    libc::inotify_rm_watch(self.fd.as_raw_fd(), watch);
                }
            }
        }
        for path in directories {
            if !self.directories.contains_key(&path) {
                let watch = watch_directory(self.fd.as_raw_fd(), &path)?;
                self.directories.insert(path, watch);
            }
        }
        Ok(())
    }
}

fn nearest_existing_directory(path: &Path) -> Result<PathBuf> {
    let mut current = PathBuf::from("/");
    for component in path.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            continue;
        }
        let candidate = current.join(component.as_os_str());
        match fs::symlink_metadata(&candidate) {
            Ok(metadata) if metadata.is_dir() => current = candidate,
            Ok(_) => return Ok(current), // Watch before a missing or unsafe parent.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(current),
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect profile parent {}", candidate.display()));
            }
        }
    }
    Ok(current)
}
fn create_watch() -> Result<OwnedFd> {
    // SAFETY: scalar flags; inotify_init1 transfers a fresh fd on success.
    let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error()).context("create profile filesystem watch");
    }
    // SAFETY: successful inotify_init1 transfers ownership.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
fn watch_directory(fd: RawFd, path: &Path) -> Result<i32> {
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
        .context("profile parent contains NUL")?;
    // SAFETY: fd is live and path is NUL-terminated; kernel retains no pointer.
    let watch = unsafe {
        libc::inotify_add_watch(
            fd,
            path.as_ptr(),
            WATCH_MASK | libc::IN_ONLYDIR | libc::IN_DONT_FOLLOW,
        )
    };
    if watch < 0 {
        return Err(std::io::Error::last_os_error()).context("watch profile parent");
    }
    Ok(watch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, time::Duration};

    fn wait_readable(fd: RawFd) {
        let mut poll = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut poll, 1, Duration::from_secs(1).as_millis() as i32) };
        assert_eq!(ready, 1);
    }

    #[test]
    fn descends_into_new_parents_and_observes_atomic_saves() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("touchbar");
        let target = directory.join("profiles.toml");
        let mut watcher = ProfileWatcher::new(&target).unwrap();

        fs::create_dir(&directory).unwrap();
        wait_readable(watcher.notification_fd());
        assert!(watcher.drain().unwrap());
        assert!(watcher.directories.contains_key(&directory));

        let temporary = directory.join("profiles.toml.new");
        let mut file = fs::File::create(&temporary).unwrap();
        file.write_all(b"version = 1\n").unwrap();
        file.sync_all().unwrap();
        fs::rename(temporary, target).unwrap();
        wait_readable(watcher.notification_fd());
        assert!(watcher.drain().unwrap());
    }
    #[test]
    fn missing_include_creation_reloads_after_failed_attempt() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("profiles.toml");
        fs::write(&target,"version=1\nfallback=\"default\"\n[[profile]]\nid=\"default\"\n[[profile.element]]\nkind=\"flexible-space\"\nminimum=0\nweight=1\n").unwrap();
        let mut watcher = ProfileWatcher::new(&target).unwrap();
        let missing = root.path().join("parts/new.toml");
        watcher
            .observe_attempts(std::collections::BTreeSet::from([
                target.clone(),
                missing.clone(),
            ]))
            .unwrap();
        fs::create_dir(root.path().join("parts")).unwrap();
        wait_readable(watcher.notification_fd());
        assert!(watcher.drain().unwrap());
        fs::write(&missing, "version=1\n").unwrap();
        wait_readable(watcher.notification_fd());
        assert!(watcher.drain().unwrap());
        fs::write(&target,"version=1\nfallback=\"default\"\ninclude=[\"parts/new.toml\"]\n[[profile]]\nid=\"default\"\n[[profile.element]]\nkind=\"flexible-space\"\nminimum=0\nweight=1\n").unwrap();
        assert_eq!(
            touchbar_profile_config::load_with_sources(&target)
                .unwrap()
                .sources
                .len(),
            2
        );
    }
    #[test]
    fn replaced_parent_directory_is_watched_again() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("parts");
        fs::create_dir(&directory).unwrap();
        let mut watcher = ProfileWatcher::new(&directory.join("profiles.toml")).unwrap();
        fs::rename(&directory, root.path().join("old-parts")).unwrap();
        wait_readable(watcher.notification_fd());
        assert!(watcher.drain().unwrap());
        fs::create_dir(&directory).unwrap();
        wait_readable(watcher.notification_fd());
        assert!(watcher.drain().unwrap());
        fs::write(directory.join("profiles.toml"), "version=1").unwrap();
        wait_readable(watcher.notification_fd());
        assert!(watcher.drain().unwrap());
    }
}
