use std::{
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::fs::{DirBuilderExt, MetadataExt},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};

const COMPONENT_PIDS_MAX: &[u8] = b"32";
/// How many tasks above the user's current count a component host may reach.
/// `RLIMIT_NPROC` counts every task of the user, not the host's own, so this
/// margin is shared with every supervisor and host starting at the same time
/// and with the rest of the desktop. At 32 it was used up when a session
/// started several plugin items together, and Mesa could not start the one
/// shader-compiler thread it needs to load the GPU driver. The per-host bound
/// is the leaf's `pids.max`; this rlimit only keeps a runaway host from
/// exhausting the user across concurrently launched hosts and commands.
pub const COMPONENT_TASK_MARGIN: u64 = 1024;
const COMPONENT_MEMORY_MAX: &[u8] = b"536870912";
const COMPONENT_CPU_MAX: &[u8] = b"100000 100000";
const COMPONENT_CGROUP_PREFIX: &str = "touchbar-component-";
/// The leaves `command.run.v1` creates beside the component hosts' leaves.
const COMMAND_CGROUP_PREFIX: &str = "touchbar-command-";
const EMPTY_WAIT: Duration = Duration::from_secs(2);
const EMPTY_POLL: Duration = Duration::from_millis(10);
static NEXT_CGROUP: AtomicU64 = AtomicU64::new(1);

/// Mandatory cgroup-v2 containment for one component-host process. The
/// component's seccomp policy separately prevents process creation; this leaf
/// bounds the Mesa worker threads required by live GPU rendering and provides
/// recursive cleanup if the trusted host itself fails.
pub struct ComponentCgroup {
    path: PathBuf,
    processes: OwnedFd,
    kill: OwnedFd,
}

impl ComponentCgroup {
    pub fn create(instance_id: u64) -> Result<Self> {
        let parent = current_cgroup_directory()?;
        cleanup_orphaned_component_cgroups(&parent)?;
        let unique = NEXT_CGROUP.fetch_add(1, Ordering::Relaxed);
        let path = parent.join(format!(
            "{COMPONENT_CGROUP_PREFIX}{}-{instance_id}-{unique}",
            std::process::id()
        ));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .context("create component cgroup")?;
        let result = (|| {
            write_setting(&path, "cgroup.max.depth", b"0")?;
            write_setting(&path, "cgroup.max.descendants", b"0")?;
            // Per-user rlimits cannot enforce a per-host task ceiling. pids
            // is mandatory; configure other delegated controllers when present.
            write_setting(&path, "pids.max", COMPONENT_PIDS_MAX)?;
            write_optional_setting(&path, "memory.max", COMPONENT_MEMORY_MAX)?;
            write_optional_setting(&path, "memory.swap.max", b"0")?;
            write_optional_setting(&path, "memory.oom.group", b"1")?;
            write_optional_setting(&path, "cpu.max", COMPONENT_CPU_MAX)?;
            let processes = open_control(&path, "cgroup.procs", libc::O_WRONLY)?;
            let kill = open_control(&path, "cgroup.kill", libc::O_WRONLY)?;
            Ok(Self {
                path: path.clone(),
                processes,
                kill,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir(&path);
        }
        result
    }

    pub fn processes_fd(&self) -> RawFd {
        self.processes.as_raw_fd()
    }

    fn kill_all(&self) {
        let _ = write_all(self.kill.as_raw_fd(), b"1");
    }

    fn wait_empty(&self) {
        let deadline = Instant::now() + EMPTY_WAIT;
        while Instant::now() < deadline {
            if fs::read_to_string(self.path.join("cgroup.events"))
                .ok()
                .is_some_and(|value| value.lines().any(|line| line == "populated 0"))
            {
                return;
            }
            thread::sleep(EMPTY_POLL);
        }
    }
}

/// A supervisor killed with SIGKILL cannot run `Drop`. The parent-death
/// signals of its host and commands still kill them, leaving empty cgroup
/// leaves; anything a command started survives in its leaf. Clean up only
/// leaves with our exact generated names, a dead owner PID, and private
/// same-user ownership: whatever still runs in one is killed, and the leaf is
/// removed once it is authoritatively `populated 0`. A live supervisor's
/// leaves are therefore never touched.
fn cleanup_orphaned_component_cgroups(parent: &Path) -> Result<()> {
    // SAFETY: geteuid takes no arguments.
    let effective_user = unsafe { libc::geteuid() };
    for entry in fs::read_dir(parent).context("enumerate component cgroups")? {
        let entry = entry.context("read component cgroup entry")?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Some(owner_pid) = generated_cgroup_owner(&name) else {
            continue;
        };
        if Path::new("/proc").join(owner_pid.to_string()).exists() {
            continue;
        }
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error).context("inspect orphan component cgroup"),
        };
        if !metadata.file_type().is_dir()
            || metadata.uid() != effective_user
            || metadata.mode() & 0o022 != 0
        {
            bail!("orphan component cgroup is not a private user-owned directory");
        }
        if !orphan_emptied(&path)? {
            continue;
        }
        match fs::remove_dir(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("remove orphan component cgroup"),
        }
    }
    Ok(())
}

/// Whether an orphan leaf is empty, killing what still runs in it first.
fn orphan_emptied(path: &Path) -> Result<bool> {
    let populated = || -> Result<Option<bool>> {
        match fs::read_to_string(path.join("cgroup.events")) {
            Ok(events) => Ok(Some(!events.lines().any(|line| line == "populated 0"))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read orphan component cgroup state"),
        }
    };
    match populated()? {
        None => return Ok(false),
        Some(false) => return Ok(true),
        Some(true) => {}
    }
    match open_control(path, "cgroup.kill", libc::O_WRONLY) {
        // EBUSY/ENOENT only mean it emptied or went away meanwhile.
        Ok(kill) => {
            let _ = write_all(kill.as_raw_fd(), b"1");
        }
        Err(_) => return Ok(false),
    }
    let deadline = Instant::now() + EMPTY_WAIT;
    while Instant::now() < deadline {
        match populated()? {
            None => return Ok(false),
            Some(false) => return Ok(true),
            Some(true) => thread::sleep(EMPTY_POLL),
        }
    }
    Ok(false)
}

fn generated_cgroup_owner(name: &str) -> Option<u32> {
    let suffix = name
        .strip_prefix(COMPONENT_CGROUP_PREFIX)
        .or_else(|| name.strip_prefix(COMMAND_CGROUP_PREFIX))?;
    let mut fields = suffix.split('-');
    let owner = fields.next()?.parse().ok()?;
    let instance = fields.next()?;
    let unique = fields.next()?;
    if owner == 0
        || fields.next().is_some()
        || instance.is_empty()
        || unique.is_empty()
        || !instance.bytes().all(|value| value.is_ascii_digit())
        || !unique.bytes().all(|value| value.is_ascii_digit())
    {
        return None;
    }
    Some(owner)
}

impl Drop for ComponentCgroup {
    fn drop(&mut self) {
        self.kill_all();
        self.wait_empty();
        let _ = fs::remove_dir(&self.path);
    }
}

pub fn component_task_limit(maximum_additional_tasks: u64) -> Result<u64> {
    if maximum_additional_tasks == 0 {
        bail!("component task allowance must be nonzero");
    }
    // SAFETY: geteuid takes no arguments.
    let effective_user = unsafe { libc::geteuid() };
    let mut tasks = 0_u64;
    for entry in fs::read_dir("/proc").context("enumerate user tasks")? {
        let Ok(entry) = entry else { continue };
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        if entry
            .metadata()
            .ok()
            .is_some_and(|value| value.uid() == effective_user)
        {
            let count = fs::read_dir(entry.path().join("task"))
                .ok()
                .map(|entries| entries.filter_map(Result::ok).count() as u64)
                .unwrap_or(1);
            tasks = tasks.saturating_add(count.max(1));
        }
    }
    let requested = tasks
        .checked_add(maximum_additional_tasks)
        .context("component task limit overflow")?;
    let mut inherited = std::mem::MaybeUninit::<libc::rlimit>::uninit();
    // SAFETY: inherited points to sufficient writable storage.
    if unsafe { libc::getrlimit(libc::RLIMIT_NPROC, inherited.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error()).context("read inherited task limit");
    }
    // SAFETY: getrlimit succeeded and initialized inherited.
    let inherited = unsafe { inherited.assume_init() };
    let limit = requested.min(inherited.rlim_max);
    if limit <= tasks {
        bail!("inherited task limit leaves no room for the component host");
    }
    Ok(limit)
}

pub(crate) fn current_cgroup_directory() -> Result<PathBuf> {
    let membership = fs::read_to_string("/proc/self/cgroup").context("read cgroup membership")?;
    let mut unified = None;
    for line in membership.lines() {
        let mut fields = line.splitn(3, ':');
        if fields.next() == Some("0") && fields.next() == Some("") {
            if unified.is_some() {
                bail!("multiple unified cgroup memberships");
            }
            unified = fields.next();
        }
    }
    let relative = unified
        .and_then(|path| path.strip_prefix('/'))
        .context("missing unified cgroup-v2 membership")?;
    let root = Path::new("/sys/fs/cgroup").canonicalize()?;
    let mut directory = root.join(relative).canonicalize()?;
    if !directory.starts_with(&root) {
        bail!("cgroup membership escapes the cgroup-v2 mount");
    }
    // systemd DelegateSubgroup and ci-session.sh put the supervisor in this
    // leaf. Commands and component hosts belong beside it, under an empty
    // delegated domain; enabling pids in a populated parent instead creates
    // a threaded domain whose ordinary children cannot accept processes.
    if directory
        .file_name()
        .is_some_and(|name| name == "touchbar-supervisor")
    {
        directory = directory
            .parent()
            .context("missing delegated cgroup parent")?
            .to_owned();
    }
    if !directory.starts_with(&root) {
        bail!("delegated cgroup escapes the cgroup-v2 mount");
    }
    let metadata = fs::metadata(&directory)?;
    // SAFETY: geteuid takes no arguments.
    let effective_user = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != effective_user || metadata.mode() & 0o022 != 0 {
        bail!("current cgroup is not a private user-owned directory");
    }
    if fs::read_to_string(directory.join("cgroup.type"))?.trim() != "domain"
        || !fs::read_to_string(directory.join("cgroup.procs"))?
            .trim()
            .is_empty()
    {
        bail!(
            "supervisor needs an empty delegated domain; use ci-session.sh or DelegateSubgroup=touchbar-supervisor"
        );
    }
    let available = fs::read_to_string(directory.join("cgroup.controllers"))?;
    let available = available.split_whitespace().collect::<Vec<_>>();
    if !available.contains(&"pids") {
        bail!("pids controller is not delegated to the supervisor");
    }
    let enable = ["cpu", "memory", "pids"]
        .into_iter()
        .filter(|controller| available.contains(controller))
        .map(|controller| format!("+{controller}"))
        .collect::<Vec<_>>()
        .join(" ");
    write_setting(&directory, "cgroup.subtree_control", enable.as_bytes())?;
    Ok(directory)
}

fn write_setting(directory: &Path, name: &str, value: &[u8]) -> Result<()> {
    let descriptor = open_control(directory, name, libc::O_WRONLY)?;
    write_all(descriptor.as_raw_fd(), value)
        .with_context(|| format!("configure component cgroup {name}"))
}

fn write_optional_setting(directory: &Path, name: &str, value: &[u8]) -> Result<()> {
    match fs::symlink_metadata(directory.join(name)) {
        Ok(_) => write_setting(directory, name, value),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect component cgroup {name}")),
    }
}

fn open_control(directory: &Path, name: &str, flags: i32) -> Result<OwnedFd> {
    if name.contains('/') || name.is_empty() {
        bail!("invalid cgroup control name");
    }
    let path = directory.join(name);
    let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())?;
    // SAFETY: path is a live NUL-terminated string and no mode is used.
    let descriptor =
        unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC | libc::O_NOFOLLOW) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error()).with_context(|| format!("open cgroup {name}"));
    }
    // SAFETY: successful open returned one uniquely owned descriptor.
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata is writable and descriptor is live.
    if unsafe { libc::fstat(descriptor.as_raw_fd(), metadata.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error()).context("inspect cgroup control");
    }
    // SAFETY: fstat succeeded and initialized metadata.
    let metadata = unsafe { metadata.assume_init() };
    // SAFETY: geteuid takes no arguments.
    let effective_user = unsafe { libc::geteuid() };
    if metadata.st_mode & libc::S_IFMT != libc::S_IFREG
        || metadata.st_uid != effective_user
        || metadata.st_mode & 0o022 != 0
    {
        bail!("cgroup control is not a private user-owned regular file");
    }
    Ok(descriptor)
}

fn write_all(descriptor: RawFd, mut bytes: &[u8]) -> io::Result<()> {
    while !bytes.is_empty() {
        // SAFETY: descriptor is live and bytes is readable for its length.
        let written = unsafe { libc::write(descriptor, bytes.as_ptr().cast(), bytes.len()) };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "cgroup control accepted zero bytes",
            ));
        }
        bytes = &bytes[written as usize..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::generated_cgroup_owner;

    #[test]
    fn generated_cgroup_names_have_an_unambiguous_owner() {
        assert_eq!(
            generated_cgroup_owner("touchbar-component-123-4-5"),
            Some(123)
        );
        assert_eq!(
            generated_cgroup_owner("touchbar-command-123-4-5"),
            Some(123)
        );
        for invalid in [
            "other-123-4-5",
            "touchbar-component-0-4-5",
            "touchbar-component-123-4",
            "touchbar-component-123-4-5-6",
            "touchbar-component-123-x-5",
            "touchbar-component-123-4-x",
            "touchbar-command-0-4-5",
            "touchbar-command-123-4",
            "touchbar-commands-123-4-5",
        ] {
            assert_eq!(generated_cgroup_owner(invalid), None, "{invalid}");
        }
    }
}
