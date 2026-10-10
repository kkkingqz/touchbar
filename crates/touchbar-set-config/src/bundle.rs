//! A deterministic bounded bundle for verified, sealed set descriptions.
use crate::MAX_CONFIG_BYTES;
use anyhow::{Result, bail};
use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
    path::{Component, Path},
};
pub const MAX_SET_BUNDLE_BYTES: usize = 1024 * 1024 + 64 * 262 + 8;
pub struct SetBundle {
    pub files: BTreeMap<String, Vec<u8>>,
}
impl SetBundle {
    pub fn encode(files: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>> {
        if files.len() > 64 || files.values().map(Vec::len).sum::<usize>() > 1024 * 1024 {
            bail!("set catalog exceeds its budget");
        }
        let mut bytes = b"TBS1".to_vec();
        bytes.extend_from_slice(&(files.len() as u32).to_le_bytes());
        for (path, source) in files {
            check_path(path)?;
            if source.len() > MAX_CONFIG_BYTES {
                bail!("set file exceeds 256 KiB");
            }
            bytes.extend_from_slice(&(path.len() as u16).to_le_bytes());
            bytes.extend_from_slice(&(source.len() as u32).to_le_bytes());
            bytes.extend_from_slice(path.as_bytes());
            bytes.extend_from_slice(source);
        }
        Ok(bytes)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_SET_BUNDLE_BYTES {
            bail!("set bundle exceeds its budget");
        }
        let mut cursor = Cursor::new(bytes);
        let mut magic = [0; 4];
        cursor.read_exact(&mut magic)?;
        if &magic != b"TBS1" {
            bail!("unknown set bundle version");
        }
        let mut count = [0; 4];
        cursor.read_exact(&mut count)?;
        let count = u32::from_le_bytes(count);
        if count > 64 {
            bail!("too many bundled sets");
        }
        let mut files = BTreeMap::new();
        let mut total = 0;
        for _ in 0..count {
            let mut path_len = [0; 2];
            cursor.read_exact(&mut path_len)?;
            let mut source_len = [0; 4];
            cursor.read_exact(&mut source_len)?;
            let path_len = u16::from_le_bytes(path_len) as usize;
            let source_len = u32::from_le_bytes(source_len) as usize;
            total += source_len;
            if path_len > 256 || source_len > MAX_CONFIG_BYTES || total > 1024 * 1024 {
                bail!("bundle member exceeds its budget");
            }
            let mut path = vec![0; path_len];
            cursor.read_exact(&mut path)?;
            let path = String::from_utf8(path)?;
            check_path(&path)?;
            let mut source = vec![0; source_len];
            cursor.read_exact(&mut source)?;
            if files.insert(path, source).is_some() {
                bail!("duplicate set bundle path");
            }
        }
        if cursor.position() as usize != bytes.len() {
            bail!("trailing set bundle data");
        }
        Ok(Self { files })
    }
}
fn check_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.len() > 256
        || path.as_bytes().contains(&0)
        || path.contains('\\')
        || path
            .split('/')
            .any(|part| part.is_empty() || matches!(part, "." | ".."))
        || path.ends_with('/')
        || !Path::new(path)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
    {
        bail!("invalid set bundle path");
    }
    Ok(())
}
