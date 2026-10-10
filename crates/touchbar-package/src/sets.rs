//! Checked metadata from explicitly referenced files. The source manifest
//! stays unchanged: derived items, profiles and bars exist only in memory.
use crate::{
    AutomaticProfile, ItemContribution, PluginManifest, PresentationBar, PresentationBarElement,
    RuntimeSpec, SUPPORTED_COMPONENT_WORLD,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use touchbar_set_config::SetDocument;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetReference {
    pub id: String,
    pub file: String,
}

/// Walk the package's directory descriptors rather than following path
/// parents. Both terminal files and every directory must resist symlinks.
pub fn read_set_files(
    root: &std::path::Path,
    manifest: &PluginManifest,
) -> Result<BTreeMap<String, Vec<u8>>> {
    use std::{
        ffi::CString,
        fs::{File, OpenOptions},
        io::Read,
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, OpenOptionsExt},
            },
        },
        path::Component,
    };
    manifest.validate()?;
    let directory = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(root)?;
    let mut files = BTreeMap::new();
    for reference in &manifest.sets {
        let mut parent = directory.try_clone()?;
        let mut components = std::path::Path::new(&reference.file)
            .components()
            .peekable();
        while let Some(component) = components.next() {
            let Component::Normal(name) = component else {
                bail!("unnormalized set path");
            };
            let name = CString::new(name.as_bytes())?;
            let last = components.peek().is_none();
            let flags = libc::O_RDONLY
                | libc::O_NONBLOCK
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | if last { 0 } else { libc::O_DIRECTORY };
            // SAFETY: owned directory descriptor, live NUL-terminated name.
            let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("open set {}", reference.file));
            }
            // SAFETY: openat transferred a fresh descriptor on success.
            let file = unsafe { File::from_raw_fd(fd) };
            if !last {
                parent = file;
                continue;
            }
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > 256 * 1024 {
                bail!("set must be a single-link regular file of at most 256 KiB");
            }
            let mut bytes = Vec::new();
            file.take(256 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 256 * 1024 {
                bail!("set grew beyond its limit");
            }
            files.insert(reference.file.clone(), bytes);
        }
    }
    Ok(files)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResolvedSetCatalog {
    pub documents: BTreeMap<String, SetDocument>,
    pub items: Vec<ItemContribution>,
    pub profiles: Vec<AutomaticProfile>,
    pub bars: Vec<PresentationBar>,
    pub priorities: BTreeMap<String, i32>,
}

pub fn resolve_set_catalog(
    manifest: &PluginManifest,
    files: &BTreeMap<String, Vec<u8>>,
) -> Result<ResolvedSetCatalog> {
    manifest.validate()?;
    let expected: BTreeSet<_> = manifest
        .sets
        .iter()
        .map(|reference| reference.file.clone())
        .collect();
    if expected != files.keys().cloned().collect() {
        bail!("set file membership does not match manifest");
    }
    if files.values().map(Vec::len).sum::<usize>() > 1024 * 1024 {
        bail!("set catalog exceeds 1 MiB");
    }
    let mut catalog = ResolvedSetCatalog {
        profiles: manifest.profiles.clone(),
        bars: manifest.bars.clone(),
        ..Default::default()
    };
    for reference in &manifest.sets {
        let source =
            std::str::from_utf8(&files[&reference.file]).context("set file is not UTF-8")?;
        let document =
            SetDocument::from_toml(source).with_context(|| format!("set {}", reference.file))?;
        if document.id != reference.id {
            bail!("set ID does not match manifest reference {}", reference.id);
        }
        catalog.items.push(ItemContribution {
            id: document.id.clone(),
            label: document.label.clone(),
            default_width: document.default_width,
            show_in_default_profile: document.show_in_default_profile,
            expanded_bar: document.expanded_panel.clone(),
            press_and_hold_bar: document.press_and_hold_panel.clone(),
        });
        if let Some(selection) = &document.selection {
            catalog.profiles.push(AutomaticProfile {
                id: document.id.clone(),
                label: selection
                    .label
                    .clone()
                    .unwrap_or_else(|| document.label.clone()),
                items: vec![document.id.clone()],
                principal_item: Some(document.id.clone()),
                applications: selection.applications.clone(),
                activities: selection.activities.clone(),
                enabled_by_default: selection.enabled_by_default,
            });
            catalog
                .priorities
                .insert(document.id.clone(), selection.priority);
        }
        for panel in &document.panels {
            let items = if panel.items.is_empty() {
                vec![document.id.clone()]
            } else {
                panel.items.clone()
            };
            catalog.bars.push(PresentationBar {
                id: panel.id.clone(),
                minimum_width: panel.minimum_width,
                preferred_width: panel.preferred_width,
                maximum_width: panel.maximum_width,
                dismiss_on_selection: panel.dismiss_on_selection,
                principal_item: panel
                    .principal_item
                    .clone()
                    .or_else(|| Some(document.id.clone())),
                elements: if !panel.elements.is_empty() {
                    panel.elements.clone()
                } else {
                    items
                        .into_iter()
                        .map(|item| PresentationBarElement::Item {
                            item,
                            minimum_width: panel.minimum_width,
                            preferred_width: panel.preferred_width,
                            maximum_width: panel.maximum_width,
                        })
                        .collect()
                },
            });
        }
        catalog.documents.insert(document.id.clone(), document);
    }
    // Reuse the complete legacy metadata validator on the derived projection.
    // No synthetic fields are ever serialized or substituted for signed bytes.
    let mut projection = manifest.clone();
    if let RuntimeSpec::ButtonSetLogic { entrypoint, .. } = &manifest.runtime {
        projection.runtime = RuntimeSpec::Component {
            entrypoint: entrypoint.clone(),
            world: SUPPORTED_COMPONENT_WORLD.into(),
        };
    }
    projection.sets.clear();
    projection.resolved_sets = None;
    projection.items = catalog.items.clone();
    projection.profiles = catalog.profiles.clone();
    projection.bars = catalog.bars.clone();
    projection.validate()?;
    Ok(catalog)
}
