use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{File, OpenOptions},
    io, mem,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use input_linux_sys::{input_event, timeval};

const EV_KEY: u16 = 1;
const KEY_FN: u16 = 0x1d0;
const KEY_CAPABILITY_BYTES: usize = 96;
const INPUT_DIRECTORY: &str = "/dev/input";
/// touchbard's own uinput keyboard. It never carries Fn.
const OWN_DEVICE_NAME: &str = "TouchBar System Keys";
/// epoll token of the /dev/input watch; device tokens are their descriptors.
const WATCH_TOKEN: u64 = u64::MAX;

/// Reads only the physical Fn state. Other keyboard events never leave this
/// module or cross the privileged hardware boundary.
///
/// Fn counts as held while any physical keyboard advertising KEY_FN holds it,
/// as recorded in the input core's key bitmap (EVIOCGKEY). Events only wake
/// the module up. A remapper such as xremap grabs the internal keyboard
/// exclusively, so that node delivers no events at all, yet the kernel still
/// tracks its keys; the remapper's virtual device re-emits them and provides
/// the wake-up. Virtual devices therefore wake but never contribute state, so
/// no process with uinput access can forge Fn, and with it the recovery hold.
/// Nodes come and go (a remapper restart recreates its device); losing one is
/// not fatal, and new ones are picked up through an inotify watch on
/// /dev/input.
pub struct FnInput {
    epoll: OwnedFd,
    watch: OwnedFd,
    devices: BTreeMap<RawFd, FnDevice>,
    pressed: bool,
}

struct FnDevice {
    file: File,
    path: PathBuf,
    rdev: u64,
    /// Registered below /sys/devices/virtual (uinput, remappers).
    virtual_device: bool,
    pressed: bool,
}

impl FnInput {
    pub fn open() -> Result<Self> {
        let epoll = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if epoll < 0 {
            return Err(io::Error::last_os_error()).context("create Fn input epoll");
        }
        // SAFETY: epoll_create1 returned a new descriptor owned by nothing else.
        let epoll = unsafe { OwnedFd::from_raw_fd(epoll) };

        let watch = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if watch < 0 {
            return Err(io::Error::last_os_error()).context("create Fn input device watch");
        }
        // SAFETY: inotify_init1 returned a new descriptor owned by nothing else.
        let watch = unsafe { OwnedFd::from_raw_fd(watch) };
        let directory = CString::new(INPUT_DIRECTORY).expect("static path has no NUL");
        // IN_ATTRIB also covers a node udev re-labels after the kernel created it.
        let result = unsafe {
            libc::inotify_add_watch(
                watch.as_raw_fd(),
                directory.as_ptr(),
                libc::IN_CREATE | libc::IN_ATTRIB,
            )
        };
        if result < 0 {
            return Err(io::Error::last_os_error()).context("watch /dev/input for Fn devices");
        }
        epoll_add(&epoll, watch.as_raw_fd(), WATCH_TOKEN)?;

        let mut input = Self {
            epoll,
            watch,
            devices: BTreeMap::new(),
            pressed: false,
        };
        input.rescan()?;
        input.pressed = input.any_pressed();
        if input.devices.is_empty() {
            println!("fn-input=waiting devices=0");
        } else {
            println!(
                "fn-input=ready devices={} pressed={}",
                input.devices.len(),
                input.pressed
            );
        }
        Ok(input)
    }

    /// One descriptor for the whole set: it becomes readable when any watched
    /// keyboard has events or /dev/input changes.
    pub fn as_raw_fd(&self) -> i32 {
        self.epoll.as_raw_fd()
    }

    pub fn pressed(&self) -> bool {
        self.pressed
    }

    /// Returns only actual state transitions, coalescing an arbitrary number
    /// of unrelated keyboard events read in the same batch.
    pub fn read_changed(&mut self) -> Result<Option<bool>> {
        let mut ready = [libc::epoll_event { events: 0, u64: 0 }; 16];
        let count = unsafe {
            libc::epoll_wait(
                self.epoll.as_raw_fd(),
                ready.as_mut_ptr(),
                ready.len() as i32,
                0,
            )
        };
        if count < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(None);
            }
            return Err(error).context("wait for Fn input");
        }

        let mut rescan = false;
        for event in &ready[..count as usize] {
            let token = event.u64;
            if token == WATCH_TOKEN {
                drain_watch(&self.watch);
                rescan = true;
                continue;
            }
            let descriptor = token as RawFd;
            let Some(device) = self.devices.get_mut(&descriptor) else {
                continue;
            };
            if let Err(error) = device.discard_events() {
                self.forget(descriptor, &error);
                rescan = true;
            }
        }
        // A grabbed keyboard never becomes readable, so every wake-up re-reads
        // the key state of all watched devices rather than trusting events.
        let lost = self
            .devices
            .iter_mut()
            .filter_map(|(descriptor, device)| {
                device.refresh().err().map(|error| (*descriptor, error))
            })
            .collect::<Vec<_>>();
        for (descriptor, error) in lost {
            self.forget(descriptor, &error);
            rescan = true;
        }
        if rescan {
            self.rescan()?;
        }

        let pressed = self.any_pressed();
        if pressed == self.pressed {
            return Ok(None);
        }
        self.pressed = pressed;
        Ok(Some(pressed))
    }

    fn any_pressed(&self) -> bool {
        self.devices
            .values()
            .any(|device| device.pressed && !device.virtual_device)
    }

    /// Removal surfaces as ENODEV. A key held on a vanished device is released
    /// with it; a replacement may reuse its node.
    fn forget(&mut self, descriptor: RawFd, error: &anyhow::Error) {
        if let Some(device) = self.devices.get(&descriptor) {
            println!(
                "fn-input=removed device={} reason={error:#}",
                device.path.display()
            );
        }
        self.remove(descriptor);
    }

    fn remove(&mut self, descriptor: RawFd) {
        unsafe {
            libc::epoll_ctl(
                self.epoll.as_raw_fd(),
                libc::EPOLL_CTL_DEL,
                descriptor,
                std::ptr::null_mut(),
            );
        }
        self.devices.remove(&descriptor);
    }

    fn rescan(&mut self) -> Result<()> {
        for path in event_nodes()? {
            let Ok(metadata) = std::fs::metadata(&path) else {
                continue;
            };
            if self
                .devices
                .values()
                .any(|device| device.rdev == metadata.rdev())
            {
                continue;
            }
            let Some(device) = FnDevice::open(&path) else {
                continue;
            };
            let descriptor = device.file.as_raw_fd();
            epoll_add(&self.epoll, descriptor, descriptor as u64)?;
            println!(
                "fn-input=added device={} virtual={} pressed={}",
                device.path.display(),
                device.virtual_device,
                device.pressed
            );
            self.devices.insert(descriptor, device);
        }
        Ok(())
    }
}

impl FnDevice {
    /// Opens `path` if it is a keyboard advertising KEY_FN, other than
    /// touchbard's own virtual keyboard. Nodes that vanish or refuse the
    /// queries while being probed are skipped rather than treated as fatal.
    fn open(path: &Path) -> Option<Self> {
        let event = path.file_name()?.to_str()?;
        let sysfs_name = Path::new("/sys/class/input")
            .join(event)
            .join("device/name");
        let name = std::fs::read_to_string(sysfs_name).unwrap_or_default();
        if name.trim() == OWN_DEVICE_NAME {
            return None;
        }
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .ok()?;
        if !supports_key(&file, KEY_FN).ok()? {
            return None;
        }
        // Identify the node by the descriptor actually opened, not the path.
        let rdev = file.metadata().ok()?.rdev();
        let virtual_device = std::fs::canonicalize(Path::new("/sys/class/input").join(event))
            .ok()?
            .starts_with("/sys/devices/virtual");
        let pressed = query_key_state(&file, KEY_FN).ok()?;
        Some(Self {
            file,
            path: path.to_path_buf(),
            rdev,
            virtual_device,
            pressed,
        })
    }

    fn refresh(&mut self) -> Result<()> {
        self.pressed = query_key_state(&self.file, KEY_FN)
            .with_context(|| format!("query Fn state of {}", self.path.display()))?;
        Ok(())
    }

    /// Empties the queue so the descriptor stops polling readable. The events
    /// themselves are not interpreted; `refresh` reads the resulting state.
    fn discard_events(&mut self) -> Result<()> {
        let mut events = [zero_event(); 64];
        loop {
            let bytes = unsafe {
                libc::read(
                    self.file.as_raw_fd(),
                    events.as_mut_ptr().cast(),
                    mem::size_of_val(&events),
                )
            };
            if bytes < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(());
                }
                return Err(error)
                    .with_context(|| format!("read Fn input {}", self.path.display()));
            }
            if bytes == 0 {
                bail!("Fn input {} closed", self.path.display());
            }
            if !(bytes as usize).is_multiple_of(mem::size_of::<input_event>()) {
                bail!("Fn input returned a partial event");
            }
        }
    }
}

fn event_nodes() -> Result<Vec<PathBuf>> {
    let mut events = std::fs::read_dir("/sys/class/input")?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let suffix = name.strip_prefix("event")?;
            (!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| name.to_owned())
        })
        .collect::<Vec<_>>();
    events.sort();
    Ok(events
        .into_iter()
        .map(|event| Path::new(INPUT_DIRECTORY).join(event))
        .collect())
}

fn epoll_add(epoll: &OwnedFd, descriptor: RawFd, token: u64) -> Result<()> {
    let mut event = libc::epoll_event {
        events: libc::EPOLLIN as u32,
        u64: token,
    };
    let result = unsafe {
        libc::epoll_ctl(
            epoll.as_raw_fd(),
            libc::EPOLL_CTL_ADD,
            descriptor,
            &mut event,
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error()).context("register Fn input descriptor");
    }
    Ok(())
}

/// Only the fact that /dev/input changed matters; the names are rescanned.
fn drain_watch(watch: &OwnedFd) {
    let mut buffer = [0_u8; 4096];
    while unsafe { libc::read(watch.as_raw_fd(), buffer.as_mut_ptr().cast(), buffer.len()) } > 0 {}
}

fn supports_key(file: &File, key: u16) -> Result<bool> {
    let mut bits = [0_u8; KEY_CAPABILITY_BYTES];
    let request = ioctl_read_request(0x20 + u64::from(EV_KEY), bits.len());
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request, bits.as_mut_ptr()) };
    if result < 0 {
        return Err(io::Error::last_os_error()).context("query evdev key capabilities");
    }
    let byte = usize::from(key / 8);
    Ok(byte < bits.len() && bits[byte] & (1 << (key % 8)) != 0)
}

fn query_key_state(file: &File, key: u16) -> Result<bool> {
    let mut bits = [0_u8; KEY_CAPABILITY_BYTES];
    let request = ioctl_read_request(0x18, bits.len());
    let result = unsafe { libc::ioctl(file.as_raw_fd(), request, bits.as_mut_ptr()) };
    if result < 0 {
        return Err(io::Error::last_os_error()).context("query evdev key state");
    }
    let byte = usize::from(key / 8);
    Ok(byte < bits.len() && bits[byte] & (1 << (key % 8)) != 0)
}

const fn ioctl_read_request(number: u64, size: usize) -> u64 {
    const IOC_READ: u64 = 2;
    const IOC_DIR_SHIFT: u64 = 30;
    const IOC_SIZE_SHIFT: u64 = 16;
    const IOC_TYPE_SHIFT: u64 = 8;
    (IOC_READ << IOC_DIR_SHIFT)
        | ((size as u64) << IOC_SIZE_SHIFT)
        | ((b'E' as u64) << IOC_TYPE_SHIFT)
        | number
}

const fn zero_event() -> input_event {
    input_event {
        time: timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        type_: 0,
        code: 0,
        value: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_capability_buffer_covers_fn() {
        assert!(usize::from(KEY_FN / 8) < KEY_CAPABILITY_BYTES);
    }

    fn device(virtual_device: bool, pressed: bool) -> FnDevice {
        FnDevice {
            file: File::open("/dev/null").unwrap(),
            path: PathBuf::from("/dev/null"),
            rdev: 0,
            virtual_device,
            pressed,
        }
    }

    fn empty_input() -> FnInput {
        let epoll = unsafe { OwnedFd::from_raw_fd(libc::epoll_create1(libc::EPOLL_CLOEXEC)) };
        let watch = unsafe { OwnedFd::from_raw_fd(libc::inotify_init1(libc::IN_CLOEXEC)) };
        FnInput {
            epoll,
            watch,
            devices: BTreeMap::new(),
            pressed: false,
        }
    }

    #[test]
    fn fn_is_held_while_any_physical_keyboard_holds_it() {
        let mut input = empty_input();
        assert!(!input.any_pressed());

        input.devices.insert(100, device(false, false));
        input.devices.insert(101, device(false, true));
        assert!(input.any_pressed());

        input.devices.get_mut(&101).unwrap().pressed = false;
        assert!(!input.any_pressed());
    }

    #[test]
    fn virtual_keyboards_wake_but_cannot_hold_fn() {
        let mut input = empty_input();
        // A remapper's device, or any other uinput device, reporting Fn.
        input.devices.insert(100, device(true, true));
        assert!(!input.any_pressed());

        input.devices.insert(101, device(false, true));
        assert!(input.any_pressed());
    }

    #[test]
    #[ignore = "requires readable live evdev keyboards"]
    fn live_keyboards_advertising_fn_are_watched() {
        let mut input = FnInput::open().unwrap();
        assert!(!input.devices.is_empty());
        assert_eq!(input.read_changed().unwrap(), None);
    }

    #[test]
    fn a_vanished_keyboard_releases_its_fn() {
        let mut input = empty_input();
        input.devices.insert(100, device(false, true));
        input.remove(100);
        assert!(!input.any_pressed());
    }
}
