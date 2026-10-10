//! One-level includes, checked before constructing the active profile model.
use crate::{
    ContributionConfig, MAX_CONFIG_BYTES, ProfileConfig, ProfileDocument, ProfileRuleConfig,
    RegionConfig,
};
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::{
    collections::BTreeSet,
    ffi::CString,
    fs::{File, OpenOptions},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::{Component, Path, PathBuf},
};

#[derive(Debug)]
pub struct LoadedProfiles {
    pub document: ProfileDocument,
    pub sources: BTreeSet<PathBuf>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fragment {
    version: u32,
    #[serde(default, rename = "contribution")]
    contributions: Vec<ContributionConfig>,
    #[serde(default, rename = "profile")]
    profiles: Vec<ProfileConfig>,
    #[serde(default, rename = "rule")]
    rules: Vec<ProfileRuleConfig>,
    #[serde(default, rename = "region")]
    regions: Vec<RegionConfig>,
}

pub fn load_with_sources(path: &Path) -> Result<LoadedProfiles> {
    let path = absolute(path)?;
    let bytes = read_secure(&path)?;
    let mut document: ProfileDocument =
        toml::from_str(std::str::from_utf8(&bytes)?).context("parse profile root")?;
    let paths = include_paths(&path, &document.include)?;
    let mut total = bytes.len();
    let mut sources = BTreeSet::from([path]);
    for path in paths {
        let bytes = read_secure(&path)?;
        total += bytes.len();
        if total > 1024 * 1024 {
            bail!("profile include graph exceeds 1 MiB");
        }
        let fragment: Fragment =
            toml::from_str(std::str::from_utf8(&bytes)?).with_context(|| {
                format!(
                    "parse profile fragment {} (nested includes and fallback are forbidden)",
                    path.display()
                )
            })?;
        if fragment.version != 1 {
            bail!("unsupported profile fragment version");
        }
        document.contributions.extend(fragment.contributions);
        document.profiles.extend(fragment.profiles);
        document.rules.extend(fragment.rules);
        document.regions.extend(fragment.regions);
        sources.insert(path);
    }
    document.include.clear();
    document.validate()?;
    Ok(LoadedProfiles { document, sources })
}

/// Includes from the current root, even when a fragment cannot yet be loaded.
/// The watcher retains last-good sources and this bounded set of attempted ones.
pub fn requested_sources(path: &Path) -> Result<BTreeSet<PathBuf>> {
    #[derive(Deserialize)]
    struct Request {
        #[serde(default)]
        include: Vec<String>,
    }
    let path = absolute(path)?;
    let bytes = read_secure(&path)?;
    let request: Request = toml::from_str(std::str::from_utf8(&bytes)?)?;
    let mut paths: BTreeSet<_> = include_paths(&path, &request.include)?
        .into_iter()
        .collect();
    paths.insert(path);
    Ok(paths)
}
fn include_paths(root: &Path, names: &[String]) -> Result<Vec<PathBuf>> {
    if names.len() > 63 {
        bail!("profile graph must contain at most 64 files including root");
    }
    let parent = root.parent().context("profile root lacks parent")?;
    let mut unique = BTreeSet::new();
    let mut paths = Vec::new();
    for name in names {
        if name.is_empty()
            || name.as_bytes().contains(&0)
            || name.contains('\\')
            || name
                .split('/')
                .any(|part| part.is_empty() || matches!(part, "." | ".."))
            || !Path::new(name)
                .components()
                .all(|part| matches!(part, Component::Normal(_)))
        {
            bail!("include must be a normalized relative path");
        }
        let path = parent.join(name);
        if path == root || !unique.insert(path.clone()) {
            bail!("duplicate profile include");
        }
        paths.push(path);
    }
    Ok(paths)
}
fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            _ => normalized.push(component.as_os_str()),
        }
    }
    Ok(normalized)
}
fn read_secure(path: &Path) -> Result<Vec<u8>> {
    let mut parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open("/")?;
    let mut names = path
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .peekable();
    while let Some(name) = names.next() {
        let name = CString::new(name.as_bytes())?;
        let last = names.peek().is_none();
        let flags = libc::O_RDONLY
            | libc::O_NONBLOCK
            | libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | if last { 0 } else { libc::O_DIRECTORY };
        // SAFETY: parent is owned, name is live and NUL-terminated.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("open profile configuration {}", path.display()));
        }
        // SAFETY: successful openat transfers a fresh descriptor.
        let file = unsafe { File::from_raw_fd(fd) };
        if !last {
            parent = file;
            continue;
        }
        let metadata = file.metadata()?;
        // SAFETY: geteuid takes no arguments and retains no pointers.
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o022 != 0
            || metadata.len() > MAX_CONFIG_BYTES
        {
            bail!(
                "profile configuration {} must be a bounded single-link regular file owned by the current user without group/other write permission",
                path.display()
            );
        }
        let mut bytes = Vec::new();
        file.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            bail!("profile configuration grew beyond 256 KiB");
        }
        return Ok(bytes);
    }
    bail!("profile configuration requires a file name")
}
