//! A private dbus-daemon for tests of the session bus clients.

use std::{path::PathBuf, process::Command};

use zbus::blocking::{Connection, connection::Builder};

pub struct PrivateBus {
    pub address: String,
    pid: libc::pid_t,
    directory: tempfile::TempDir,
}

impl PrivateBus {
    pub fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let address = format!("unix:path={}", directory.path().join("bus").display());
        let output = Command::new("dbus-daemon")
            .args([
                "--session",
                "--fork",
                "--nopidfile",
                &format!("--address={address}"),
                "--print-address=1",
                "--print-pid=1",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let output = String::from_utf8(output.stdout).unwrap();
        let mut lines = output.lines();
        let address = lines.next().unwrap().to_owned();
        let pid = lines.next().unwrap().parse().unwrap();
        Self {
            address,
            pid,
            directory,
        }
    }

    pub fn connect(&self) -> Connection {
        Builder::address(self.address.as_str())
            .unwrap()
            .build()
            .unwrap()
    }

    pub fn file(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }
}

impl Drop for PrivateBus {
    fn drop(&mut self) {
        // SAFETY: this PID came directly from the private dbus-daemon.
        let _ = unsafe { libc::kill(self.pid, libc::SIGTERM) };
    }
}
