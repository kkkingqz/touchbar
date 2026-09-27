//! The session daemon's now-playing choice, shared with plugin supervisors.
//!
//! Which player started last is history: a watcher started after two players
//! began cannot know their order. The session daemon, which follows the bus
//! for the whole session, therefore records its choice in a runtime file, and
//! the supervisors behind the media items follow it, so the bar controls the
//! same player that the `media.*` profile facts name.

use std::{
    fs,
    io::{self, Write},
    path::Path,
};

use crate::model::is_player_bus_name;

/// D-Bus names are at most 255 bytes; anything longer is not a choice.
const MAXIMUM_BYTES: u64 = 255;

/// Records `bus_name` as the now-playing player, or removes the record. The
/// file is replaced by a rename, so readers never see a partial name.
pub fn write(path: &Path, bus_name: Option<&str>) -> io::Result<()> {
    let Some(bus_name) = bus_name else {
        return match fs::remove_file(path) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            result => result,
        };
    };
    let mut staging = path.as_os_str().to_owned();
    staging.push(".new");
    let mut file = fs::File::create(&staging)?;
    file.write_all(bus_name.as_bytes())?;
    drop(file);
    fs::rename(&staging, path)
}

/// The recorded player, or `None` without a valid record.
pub fn read(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    if file.metadata().ok()?.len() > MAXIMUM_BYTES {
        return None;
    }
    let name = io::read_to_string(file).ok()?;
    is_player_bus_name(&name).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_choice_round_trips_and_its_removal_reads_as_none() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("media-session");
        assert_eq!(read(&path), None);
        write(&path, Some("org.mpris.MediaPlayer2.vlc")).unwrap();
        assert_eq!(read(&path).as_deref(), Some("org.mpris.MediaPlayer2.vlc"));
        write(&path, None).unwrap();
        assert_eq!(read(&path), None);
        write(&path, None).unwrap();
    }

    #[test]
    fn anything_but_a_player_name_is_ignored() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("media-session");
        for content in [
            "org.mpris.MediaPlayer2.playerctld".to_owned(),
            "org.gnome.Shell".to_owned(),
            format!("org.mpris.MediaPlayer2.{}", "x".repeat(300)),
        ] {
            fs::write(&path, content).unwrap();
            assert_eq!(read(&path), None);
        }
    }
}
