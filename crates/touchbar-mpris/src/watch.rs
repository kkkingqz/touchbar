//! Follows MPRIS players on a D-Bus connection and forwards commands to them.

use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use zbus::{
    MatchRule,
    blocking::{Connection, MessageIterator, connection::Builder, fdo::DBusProxy},
    message::{Message, Type},
    names::BusName,
    zvariant::{ObjectPath, OwnedValue, Value},
};

use crate::model::{Metadata, NowPlaying, PlaybackStatus, Players, Property, is_player_bus_name};

const MPRIS_PATH: &str = "/org/mpris/MediaPlayer2";
const MPRIS_NAMESPACE: &str = "org.mpris.MediaPlayer2";
const ROOT_INTERFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";
const PROPERTIES_INTERFACE: &str = "org.freedesktop.DBus.Properties";
const BUS_DRIVER: &str = "org.freedesktop.DBus";
/// MPRIS reserves this track ID for "no track"; `SetPosition` cannot use it.
const NO_TRACK: &str = "/org/mpris/MediaPlayer2/TrackList/NoTrack";
/// Metadata text is shown on a 60-pixel strip; longer values are cut.
const MAXIMUM_TEXT_BYTES: usize = 512;
const MAXIMUM_ARTISTS: usize = 8;

/// A player that does not answer within this time is skipped rather than
/// holding up the updates of every other player.
pub const METHOD_TIMEOUT: Duration = Duration::from_secs(1);
/// Signals wait here while the reader awaits a reply. The connection stops
/// reading when the queue is full, so it is sized well above any burst.
const MAXIMUM_QUEUED: usize = 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Command {
    PlayPause,
    Play,
    Pause,
    Next,
    Previous,
    /// Moves to an absolute position in the track with this ID, which must
    /// still be the current one. Players without track IDs take an empty ID.
    SetPosition {
        track_id: String,
        position: Duration,
    },
}

#[derive(Debug)]
pub enum CommandError {
    /// No player owns that bus name any more.
    UnknownPlayer,
    /// The player says it cannot do this now.
    Unsupported,
    /// The track changed since the command was made.
    StaleTrack,
    Failed(zbus::Error),
}

impl From<zbus::Error> for CommandError {
    fn from(error: zbus::Error) -> Self {
        Self::Failed(error)
    }
}

/// Keeps a [`Players`] current from one D-Bus connection.
///
/// Players are listed once at start; after that only signals are used:
/// `NameOwnerChanged` for players that appear or leave, `PropertiesChanged`
/// for their state and `Seeked` for jumps. Signals count only when they come
/// from the connection that owns a player's name. MPRIS does not signal the
/// position as it advances, so it is read when a player starts, stops or
/// changes track and extrapolated in between.
pub struct MprisWatcher {
    connection: Connection,
    players: Arc<Mutex<Players>>,
    stopped: Arc<AtomicBool>,
    /// Another process's now-playing choice, followed while it is current.
    preferred: Mutex<Option<String>>,
}

impl MprisWatcher {
    /// Watches the session bus. `notify` runs on the watcher's thread after
    /// every change, and once more if the watcher stops.
    pub fn session(notify: impl Fn() + Send + 'static) -> zbus::Result<Self> {
        let connection = Builder::session()?
            .method_timeout(METHOD_TIMEOUT)
            .max_queued(MAXIMUM_QUEUED)
            .build()?;
        Self::start(connection, notify)
    }

    pub fn start(connection: Connection, notify: impl Fn() + Send + 'static) -> zbus::Result<Self> {
        // Listen before listing players, so that no change in between is lost.
        let messages = MessageIterator::from(&connection);
        let bus = DBusProxy::new(&connection)?;
        for rule in match_rules()? {
            bus.add_match_rule(rule)?;
        }
        let players = Arc::new(Mutex::new(Players::default()));
        let mut names = bus
            .list_names()?
            .into_iter()
            .map(|name| name.to_string())
            .filter(|name| is_player_bus_name(name))
            .collect::<Vec<_>>();
        // Players already playing rank by discovery order; sorting makes that
        // order the same in every process.
        names.sort();
        for name in names {
            let Ok(owner) = BusName::try_from(name.as_str())
                .map_err(zbus::Error::from)
                .and_then(|bus_name| bus.get_name_owner(bus_name).map_err(zbus::Error::from))
            else {
                continue;
            };
            discover(&connection, &players, &name, owner.as_str());
        }
        let reader_connection = connection.clone();
        let reader_players = Arc::clone(&players);
        let stopped = Arc::new(AtomicBool::new(false));
        let reader_stopped = Arc::clone(&stopped);
        thread::Builder::new()
            .name("mpris-watcher".into())
            .spawn(move || {
                for message in messages {
                    let Ok(message) = message else {
                        continue;
                    };
                    if handle(&reader_connection, &reader_players, &message) {
                        notify();
                    }
                }
                // The bus closed: the players are no longer followed.
                reader_stopped.store(true, Ordering::Release);
                notify();
            })
            .map_err(|error| zbus::Error::Failure(format!("start MPRIS watcher: {error}")))?;
        Ok(Self {
            connection,
            players,
            stopped,
            preferred: Mutex::new(None),
        })
    }

    /// Whether the connection closed, after which nothing changes any more.
    pub fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }

    pub fn now_playing(&self) -> Option<NowPlaying> {
        let preferred = self
            .preferred
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        lock(&self.players).now_playing_preferring(Instant::now(), preferred.as_deref())
    }

    /// Follows another process's choice of session, such as the session
    /// daemon's, whenever that player is playing or paused. See
    /// [`Players::selected_preferring`].
    pub fn prefer(&self, bus_name: Option<String>) {
        *self
            .preferred
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = bus_name;
    }

    /// Reads a player's position again, to correct drift in the estimate.
    pub fn refresh_position(&self, bus_name: &str) {
        let Some(owner) = lock(&self.players).owner(bus_name).map(str::to_owned) else {
            return;
        };
        if let Some(position) = read_position(&self.connection, &owner) {
            let _ =
                lock(&self.players).update(bus_name, Property::Position(position), Instant::now());
        }
    }

    pub fn command(&self, bus_name: &str, command: &Command) -> Result<(), CommandError> {
        let (owner, capabilities, track_id, position) = {
            let players = lock(&self.players);
            let owner = players
                .owner(bus_name)
                .ok_or(CommandError::UnknownPlayer)?
                .to_owned();
            (
                owner,
                players.capabilities(bus_name).unwrap_or_default(),
                players.track_id(bus_name).map(str::to_owned),
                players.position(bus_name, Instant::now()),
            )
        };
        let allowed = match command {
            Command::PlayPause => capabilities.can_play || capabilities.can_pause,
            Command::Play => capabilities.can_play,
            Command::Pause => capabilities.can_pause,
            Command::Next => capabilities.can_go_next,
            Command::Previous => capabilities.can_go_previous,
            Command::SetPosition { .. } => capabilities.can_seek,
        };
        if !capabilities.can_control || !allowed {
            return Err(CommandError::Unsupported);
        }
        let call = |member: &str| -> Result<(), CommandError> {
            self.connection
                .call_method(
                    Some(owner.as_str()),
                    MPRIS_PATH,
                    Some(PLAYER_INTERFACE),
                    member,
                    &(),
                )
                .map(drop)
                .map_err(CommandError::from)
        };
        match command {
            Command::PlayPause => call("PlayPause"),
            Command::Play => call("Play"),
            Command::Pause => call("Pause"),
            Command::Next => call("Next"),
            Command::Previous => call("Previous"),
            Command::SetPosition {
                track_id: requested,
                position: target,
            } => {
                if requested.as_str() != track_id.as_deref().unwrap_or_default() {
                    return Err(CommandError::StaleTrack);
                }
                let target = micros(*target)?;
                let track = track_id
                    .as_deref()
                    .filter(|track| *track != NO_TRACK)
                    .and_then(|track| ObjectPath::try_from(track).ok());
                let reply = match track {
                    Some(track) => self.connection.call_method(
                        Some(owner.as_str()),
                        MPRIS_PATH,
                        Some(PLAYER_INTERFACE),
                        "SetPosition",
                        &(track, target),
                    ),
                    // Without a track ID only a relative seek is possible.
                    None => {
                        let current = micros(position.ok_or(CommandError::Unsupported)?)?;
                        self.connection.call_method(
                            Some(owner.as_str()),
                            MPRIS_PATH,
                            Some(PLAYER_INTERFACE),
                            "Seek",
                            &(target - current,),
                        )
                    }
                };
                reply.map(drop).map_err(CommandError::from)
            }
        }
    }
}

impl Drop for MprisWatcher {
    fn drop(&mut self) {
        // Closing ends the reader thread's message stream.
        let _ = self.connection.clone().close();
    }
}

fn micros(duration: Duration) -> Result<i64, CommandError> {
    i64::try_from(duration.as_micros()).map_err(|_| CommandError::Unsupported)
}

fn lock(players: &Mutex<Players>) -> MutexGuard<'_, Players> {
    players.lock().unwrap_or_else(PoisonError::into_inner)
}

fn match_rules() -> zbus::Result<[MatchRule<'static>; 3]> {
    Ok([
        MatchRule::builder()
            .msg_type(Type::Signal)
            .sender(BUS_DRIVER)?
            .interface(BUS_DRIVER)?
            .member("NameOwnerChanged")?
            .arg0ns(MPRIS_NAMESPACE)?
            .build(),
        MatchRule::builder()
            .msg_type(Type::Signal)
            .interface(PROPERTIES_INTERFACE)?
            .member("PropertiesChanged")?
            .path(MPRIS_PATH)?
            .build(),
        MatchRule::builder()
            .msg_type(Type::Signal)
            .interface(PLAYER_INTERFACE)?
            .member("Seeked")?
            .path(MPRIS_PATH)?
            .build(),
    ])
}

/// Applies one message. Returns true if the players changed.
fn handle(connection: &Connection, players: &Mutex<Players>, message: &Message) -> bool {
    if message.message_type() != Type::Signal {
        return false;
    }
    let header = message.header();
    let (Some(interface), Some(member), Some(sender)) =
        (header.interface(), header.member(), header.sender())
    else {
        return false;
    };
    let sender = sender.as_str();
    let from_mpris_path = header
        .path()
        .is_some_and(|path| path.as_str() == MPRIS_PATH);
    match (interface.as_str(), member.as_str()) {
        (BUS_DRIVER, "NameOwnerChanged") if sender == BUS_DRIVER => {
            let Ok((name, _, owner)) = message.body().deserialize::<(String, String, String)>()
            else {
                return false;
            };
            if !is_player_bus_name(&name) {
                return false;
            }
            if owner.is_empty() {
                return lock(players).remove(&name);
            }
            // The startup listing may already have found this owner.
            if lock(players).owner(&name) == Some(owner.as_str()) {
                return false;
            }
            discover(connection, players, &name, &owner)
        }
        (PROPERTIES_INTERFACE, "PropertiesChanged") if from_mpris_path => {
            let Ok((interface, changed, invalidated)) =
                message
                    .body()
                    .deserialize::<(String, HashMap<String, OwnedValue>, Vec<String>)>()
            else {
                return false;
            };
            if interface != PLAYER_INTERFACE && interface != ROOT_INTERFACE {
                return false;
            }
            let names = lock(players).owned_by(sender);
            if names.is_empty() {
                return false;
            }
            let mut properties = changed
                .iter()
                .filter_map(|(name, value)| property(&interface, name, value))
                .collect::<Vec<_>>();
            // Invalidated properties come without a value; read them.
            for name in invalidated.iter().take(16) {
                if let Some(value) = read_property(connection, sender, &interface, name) {
                    properties.extend(property(&interface, name, &value));
                }
            }
            if properties.is_empty() {
                return false;
            }
            let now = Instant::now();
            let reread = {
                let mut players = lock(players);
                names
                    .into_iter()
                    .filter(|name| players.update_all(name, properties.clone(), now))
                    .collect::<Vec<_>>()
            };
            for name in reread {
                if let Some(position) = read_position(connection, sender) {
                    let _ =
                        lock(players).update(&name, Property::Position(position), Instant::now());
                }
            }
            true
        }
        (PLAYER_INTERFACE, "Seeked") if from_mpris_path => {
            let Ok(position) = message.body().deserialize::<i64>() else {
                return false;
            };
            let Ok(position) = u64::try_from(position).map(Duration::from_micros) else {
                return false;
            };
            let mut players = lock(players);
            let names = players.owned_by(sender);
            let now = Instant::now();
            for name in &names {
                let _ = players.update(name, Property::Position(position), now);
            }
            !names.is_empty()
        }
        _ => false,
    }
}

/// Reads a player's state and adds it. A connection that does not answer as
/// an MPRIS player is dropped instead.
fn discover(
    connection: &Connection,
    players: &Mutex<Players>,
    bus_name: &str,
    owner: &str,
) -> bool {
    let Some(player) = read_all(connection, owner, PLAYER_INTERFACE) else {
        return lock(players).remove(bus_name);
    };
    let root = read_all(connection, owner, ROOT_INTERFACE).unwrap_or_default();
    let properties = root
        .iter()
        .filter_map(|(name, value)| property(ROOT_INTERFACE, name, value))
        .chain(
            player
                .iter()
                .filter_map(|(name, value)| property(PLAYER_INTERFACE, name, value)),
        )
        .collect::<Vec<_>>();
    lock(players).insert(bus_name, owner, properties, Instant::now());
    true
}

fn read_all(
    connection: &Connection,
    owner: &str,
    interface: &str,
) -> Option<HashMap<String, OwnedValue>> {
    connection
        .call_method(
            Some(owner),
            MPRIS_PATH,
            Some(PROPERTIES_INTERFACE),
            "GetAll",
            &(interface,),
        )
        .ok()?
        .body()
        .deserialize()
        .ok()
}

fn read_property(
    connection: &Connection,
    owner: &str,
    interface: &str,
    name: &str,
) -> Option<OwnedValue> {
    connection
        .call_method(
            Some(owner),
            MPRIS_PATH,
            Some(PROPERTIES_INTERFACE),
            "Get",
            &(interface, name),
        )
        .ok()?
        .body()
        .deserialize()
        .ok()
}

fn read_position(connection: &Connection, owner: &str) -> Option<Duration> {
    let value = read_property(connection, owner, PLAYER_INTERFACE, "Position")?;
    duration(&value)
}

/// Decodes one MPRIS property. Players differ in the integer types they use,
/// so every integer type is accepted where MPRIS specifies one.
fn property(interface: &str, name: &str, value: &Value<'_>) -> Option<Property> {
    let value = inner(value);
    Some(match (interface, name) {
        (ROOT_INTERFACE, "Identity") => Property::Identity(text(value)?),
        (ROOT_INTERFACE, "DesktopEntry") => Property::DesktopEntry(text(value)?),
        (PLAYER_INTERFACE, "PlaybackStatus") => {
            Property::Status(PlaybackStatus::from_mpris(&text(value)?)?)
        }
        (PLAYER_INTERFACE, "Metadata") => Property::Metadata(metadata(value)?),
        (PLAYER_INTERFACE, "Rate") => Property::Rate(number(value)?),
        (PLAYER_INTERFACE, "Position") => Property::Position(duration(value)?),
        (PLAYER_INTERFACE, "CanControl") => Property::CanControl(boolean(value)?),
        (PLAYER_INTERFACE, "CanPlay") => Property::CanPlay(boolean(value)?),
        (PLAYER_INTERFACE, "CanPause") => Property::CanPause(boolean(value)?),
        (PLAYER_INTERFACE, "CanGoNext") => Property::CanGoNext(boolean(value)?),
        (PLAYER_INTERFACE, "CanGoPrevious") => Property::CanGoPrevious(boolean(value)?),
        (PLAYER_INTERFACE, "CanSeek") => Property::CanSeek(boolean(value)?),
        _ => return None,
    })
}

fn metadata(value: &Value<'_>) -> Option<Metadata> {
    let Value::Dict(entries) = value else {
        return None;
    };
    let mut metadata = Metadata::default();
    for (key, value) in entries.iter() {
        let value = inner(value);
        match text(inner(key)).as_deref() {
            Some("mpris:trackid") => metadata.track_id = text(value),
            Some("mpris:length") => {
                metadata.length = duration(value).filter(|length| !length.is_zero())
            }
            Some("xesam:title") => metadata.title = text(value).filter(|title| !title.is_empty()),
            Some("xesam:album") => metadata.album = text(value).filter(|album| !album.is_empty()),
            Some("xesam:artist") => {
                metadata.artists = match value {
                    Value::Array(artists) => artists
                        .iter()
                        .filter_map(|artist| text(inner(artist)))
                        .filter(|artist| !artist.is_empty())
                        .take(MAXIMUM_ARTISTS)
                        .collect(),
                    // Some players send one string instead of a list.
                    _ => text(value)
                        .into_iter()
                        .filter(|artist| !artist.is_empty())
                        .collect(),
                }
            }
            _ => {}
        }
    }
    Some(metadata)
}

/// Variants can nest; the value is the innermost one.
fn inner<'a>(value: &'a Value<'a>) -> &'a Value<'a> {
    match value {
        Value::Value(boxed) => inner(boxed),
        value => value,
    }
}

fn text(value: &Value<'_>) -> Option<String> {
    let text = match value {
        Value::Str(text) => text.as_str(),
        Value::ObjectPath(path) => path.as_str(),
        _ => return None,
    };
    Some(
        truncate(text)
            .chars()
            .filter(|character| !character.is_control())
            .collect(),
    )
}

fn truncate(text: &str) -> &str {
    if text.len() <= MAXIMUM_TEXT_BYTES {
        return text;
    }
    let mut end = MAXIMUM_TEXT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn boolean(value: &Value<'_>) -> Option<bool> {
    match value {
        Value::Bool(value) => Some(*value),
        _ => None,
    }
}

fn integer(value: &Value<'_>) -> Option<i64> {
    match *value {
        Value::I64(value) => Some(value),
        Value::U64(value) => i64::try_from(value).ok(),
        Value::I32(value) => Some(value.into()),
        Value::U32(value) => Some(value.into()),
        Value::I16(value) => Some(value.into()),
        Value::U16(value) => Some(value.into()),
        Value::U8(value) => Some(value.into()),
        Value::F64(value) if value.is_finite() => Some(value as i64),
        _ => None,
    }
}

fn number(value: &Value<'_>) -> Option<f64> {
    match *value {
        Value::F64(value) => Some(value),
        _ => integer(value).map(|value| value as f64),
    }
}

/// MPRIS times are microseconds; negative values mean "unknown".
fn duration(value: &Value<'_>) -> Option<Duration> {
    u64::try_from(integer(inner(value))?)
        .ok()
        .map(Duration::from_micros)
}

#[cfg(test)]
mod tests {
    use std::{
        process::Command as Process,
        sync::mpsc::{self, Receiver},
    };

    use zbus::zvariant::{Dict, Signature};

    use super::*;
    use crate::{Capabilities, player_name};

    struct PrivateBus {
        address: String,
        pid: libc::pid_t,
        _directory: tempfile::TempDir,
    }

    impl PrivateBus {
        fn start() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let address = format!("unix:path={}", directory.path().join("bus").display());
            let output = Process::new("dbus-daemon")
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
                _directory: directory,
            }
        }

        fn connect(&self) -> Connection {
            Builder::address(self.address.as_str())
                .unwrap()
                .method_timeout(METHOD_TIMEOUT)
                .max_queued(MAXIMUM_QUEUED)
                .build()
                .unwrap()
        }
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            // SAFETY: this PID came directly from the private dbus-daemon.
            let _ = unsafe { libc::kill(self.pid, libc::SIGTERM) };
        }
    }

    #[derive(Clone)]
    struct Shared {
        status: Arc<Mutex<&'static str>>,
        title: Arc<Mutex<&'static str>>,
        position: Arc<Mutex<i64>>,
        track_id: Option<&'static str>,
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl Shared {
        fn new(status: &'static str, title: &'static str, track_id: Option<&'static str>) -> Self {
            Self {
                status: Arc::new(Mutex::new(status)),
                title: Arc::new(Mutex::new(title)),
                position: Arc::new(Mutex::new(5_000_000)),
                track_id,
                calls: Arc::default(),
            }
        }

        fn record(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    struct FakeRoot;

    #[zbus::interface(name = "org.mpris.MediaPlayer2")]
    impl FakeRoot {
        #[zbus(property)]
        fn identity(&self) -> String {
            "Fake Player".into()
        }

        #[zbus(property)]
        fn desktop_entry(&self) -> String {
            "org.example.fake".into()
        }
    }

    struct FakePlayer(Shared);

    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl FakePlayer {
        #[zbus(property)]
        fn playback_status(&self) -> String {
            (*self.0.status.lock().unwrap()).into()
        }

        #[zbus(property)]
        fn metadata(&self) -> HashMap<String, OwnedValue> {
            metadata_map(&self.0, *self.0.title.lock().unwrap())
        }

        #[zbus(property)]
        fn position(&self) -> i64 {
            *self.0.position.lock().unwrap()
        }

        #[zbus(property)]
        fn rate(&self) -> f64 {
            1.0
        }

        #[zbus(property)]
        fn can_control(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_play(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_pause(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_go_next(&self) -> bool {
            true
        }

        #[zbus(property)]
        fn can_go_previous(&self) -> bool {
            false
        }

        #[zbus(property)]
        fn can_seek(&self) -> bool {
            true
        }

        fn play_pause(&self) {
            self.0.record("PlayPause".into());
        }

        fn next(&self) {
            self.0.record("Next".into());
        }

        fn previous(&self) {
            self.0.record("Previous".into());
        }

        fn set_position(&self, track_id: ObjectPath<'_>, position: i64) {
            self.0.record(format!("SetPosition {track_id} {position}"));
        }

        fn seek(&self, offset: i64) {
            self.0.record(format!("Seek {offset}"));
        }
    }

    fn metadata_map(shared: &Shared, title: &str) -> HashMap<String, OwnedValue> {
        let mut map = HashMap::from([
            ("xesam:title".to_owned(), owned(Value::from(title))),
            (
                "xesam:artist".to_owned(),
                owned(Value::from(vec!["Someone".to_owned(), "Else".to_owned()])),
            ),
            (
                "mpris:length".to_owned(),
                owned(Value::from(200_000_000_i64)),
            ),
        ]);
        if let Some(track_id) = shared.track_id {
            map.insert(
                "mpris:trackid".to_owned(),
                owned(Value::from(ObjectPath::try_from(track_id).unwrap())),
            );
        }
        map
    }

    fn owned(value: Value<'_>) -> OwnedValue {
        value.try_to_owned().unwrap()
    }

    struct FakeService {
        connection: Connection,
        shared: Shared,
    }

    impl FakeService {
        fn start(bus: &PrivateBus, name: &str, shared: Shared) -> Self {
            let connection = Builder::address(bus.address.as_str())
                .unwrap()
                .name(name)
                .unwrap()
                .serve_at(MPRIS_PATH, FakeRoot)
                .unwrap()
                .serve_at(MPRIS_PATH, FakePlayer(shared.clone()))
                .unwrap()
                .build()
                .unwrap();
            Self { connection, shared }
        }

        fn unique_name(&self) -> String {
            self.connection.unique_name().unwrap().to_string()
        }

        fn set_status(&self, status: &'static str) {
            *self.shared.status.lock().unwrap() = status;
            self.changed(HashMap::from([("PlaybackStatus", Value::from(status))]));
        }

        fn set_title(&self, title: &'static str) {
            *self.shared.title.lock().unwrap() = title;
            let metadata = metadata_map(&self.shared, title);
            self.changed(HashMap::from([("Metadata", Value::from(metadata))]));
        }

        fn changed(&self, properties: HashMap<&str, Value<'_>>) {
            emit_changed(&self.connection, None, properties);
        }

        /// Closes the connection, as when the application quits.
        fn quit(self) {
            self.connection.close().unwrap();
        }

        fn seeked(&self, position: i64) {
            *self.shared.position.lock().unwrap() = position;
            self.connection
                .emit_signal(
                    None::<&str>,
                    MPRIS_PATH,
                    PLAYER_INTERFACE,
                    "Seeked",
                    &(position,),
                )
                .unwrap();
        }
    }

    fn emit_changed(
        connection: &Connection,
        destination: Option<&str>,
        properties: HashMap<&str, Value<'_>>,
    ) {
        connection
            .emit_signal(
                destination,
                MPRIS_PATH,
                PROPERTIES_INTERFACE,
                "PropertiesChanged",
                &(PLAYER_INTERFACE, properties, Vec::<String>::new()),
            )
            .unwrap();
    }

    fn watch(bus: &PrivateBus) -> (MprisWatcher, Receiver<()>) {
        let (sender, receiver) = mpsc::channel();
        let watcher = MprisWatcher::start(bus.connect(), move || {
            let _ = sender.send(());
        })
        .unwrap();
        (watcher, receiver)
    }

    /// Waits until `condition` holds for the watcher's current state.
    fn wait_for(
        watcher: &MprisWatcher,
        changes: &Receiver<()>,
        condition: impl Fn(Option<&NowPlaying>) -> bool,
    ) -> Option<NowPlaying> {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let playing = watcher.now_playing();
            if condition(playing.as_ref()) {
                return playing;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out; last state {playing:?}");
            let _ = changes.recv_timeout(left.min(Duration::from_millis(100)));
        }
    }

    fn selected_is<'a>(name: &'a str) -> impl Fn(Option<&NowPlaying>) -> bool + 'a {
        move |playing| playing.is_some_and(|playing| playing.bus_name == name)
    }

    #[test]
    #[ignore = "reads the players of the running desktop session"]
    fn live_session_reports_now_playing() {
        let (sender, changes) = mpsc::channel();
        let watcher = MprisWatcher::session(move || {
            let _ = sender.send(());
        })
        .unwrap();
        println!("now playing: {:#?}", watcher.now_playing());
        let seconds = std::env::var("MPRIS_WATCH_SECONDS")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if changes.recv_timeout(left).is_ok() {
                println!("changed: {:#?}", watcher.now_playing());
            }
        }
    }

    #[test]
    fn players_on_the_bus_at_start_are_read_completely() {
        let bus = PrivateBus::start();
        let _vlc = FakeService::start(
            &bus,
            "org.mpris.MediaPlayer2.vlc",
            Shared::new("Paused", "Song", Some("/org/videolan/vlc/playlist/3")),
        );
        let _mirror = FakeService::start(
            &bus,
            "org.mpris.MediaPlayer2.playerctld",
            Shared::new("Playing", "Mirror", None),
        );
        let (watcher, _changes) = watch(&bus);
        let playing = watcher
            .now_playing()
            .expect("the paused player is selected");
        assert_eq!(playing.bus_name, "org.mpris.MediaPlayer2.vlc");
        assert_eq!(playing.player_name(), "vlc");
        assert_eq!(playing.identity.as_deref(), Some("Fake Player"));
        assert_eq!(playing.desktop_entry.as_deref(), Some("org.example.fake"));
        assert_eq!(playing.status, PlaybackStatus::Paused);
        assert_eq!(playing.metadata.title.as_deref(), Some("Song"));
        assert_eq!(playing.metadata.artists, ["Someone", "Else"]);
        assert_eq!(
            playing.metadata.track_id.as_deref(),
            Some("/org/videolan/vlc/playlist/3")
        );
        assert_eq!(playing.metadata.length, Some(Duration::from_secs(200)));
        assert_eq!(playing.position, Some(Duration::from_secs(5)));
        assert_eq!(
            playing.capabilities,
            Capabilities {
                can_control: true,
                can_play: true,
                can_pause: true,
                can_go_next: true,
                can_go_previous: false,
                can_seek: true,
            }
        );
    }

    #[test]
    fn signals_follow_players_as_they_start_change_and_leave() {
        let bus = PrivateBus::start();
        let music = FakeService::start(
            &bus,
            "org.mpris.MediaPlayer2.rhythmbox",
            Shared::new("Playing", "Music", Some("/org/gnome/Rhythmbox3/1")),
        );
        let (watcher, changes) = watch(&bus);
        wait_for(
            &watcher,
            &changes,
            selected_is("org.mpris.MediaPlayer2.rhythmbox"),
        );

        // A browser appears and starts a video: it becomes now playing.
        let video = FakeService::start(
            &bus,
            "org.mpris.MediaPlayer2.firefox.instance_1_7",
            Shared::new("Stopped", "Video", None),
        );
        video.set_status("Playing");
        let playing = wait_for(
            &watcher,
            &changes,
            selected_is("org.mpris.MediaPlayer2.firefox.instance_1_7"),
        )
        .unwrap();
        assert_eq!(player_name(&playing.bus_name), "firefox");

        // Paused, it keeps the session; a new title arrives with metadata.
        video.set_status("Paused");
        video.set_title("Next video");
        wait_for(&watcher, &changes, |playing| {
            playing.is_some_and(|playing| {
                playing.status == PlaybackStatus::Paused
                    && playing.metadata.title.as_deref() == Some("Next video")
            })
        });

        // A seek moves the position without waiting for playback.
        video.seeked(90_000_000);
        wait_for(&watcher, &changes, |playing| {
            playing.is_some_and(|playing| playing.position == Some(Duration::from_secs(90)))
        });

        // Closing the browser returns the session to the music.
        video.quit();
        wait_for(
            &watcher,
            &changes,
            selected_is("org.mpris.MediaPlayer2.rhythmbox"),
        );
        music.set_status("Stopped");
        wait_for(&watcher, &changes, |playing| playing.is_none());
    }

    #[test]
    fn only_the_owner_of_a_player_can_change_it() {
        let bus = PrivateBus::start();
        let player = FakeService::start(
            &bus,
            "org.mpris.MediaPlayer2.vlc",
            Shared::new("Playing", "Song", None),
        );
        let target = bus.connect();
        let unique = target.unique_name().unwrap().to_string();
        let (sender, changes) = mpsc::channel();
        let watcher = MprisWatcher::start(target, move || {
            let _ = sender.send(());
        })
        .unwrap();
        assert_ne!(player.unique_name(), unique);

        let impostor = bus.connect();
        emit_changed(
            &impostor,
            None,
            HashMap::from([("PlaybackStatus", Value::from("Paused"))]),
        );
        emit_changed(
            &impostor,
            Some(&unique),
            HashMap::from([("PlaybackStatus", Value::from("Paused"))]),
        );
        assert!(changes.recv_timeout(Duration::from_millis(300)).is_err());
        assert_eq!(
            watcher.now_playing().map(|playing| playing.status),
            Some(PlaybackStatus::Playing)
        );
    }

    #[test]
    fn commands_reach_the_player_and_check_the_track() {
        let bus = PrivateBus::start();
        let with_track = Shared::new("Playing", "Song", Some("/org/videolan/vlc/playlist/3"));
        let vlc = FakeService::start(&bus, "org.mpris.MediaPlayer2.vlc", with_track.clone());
        let (watcher, changes) = watch(&bus);
        wait_for(
            &watcher,
            &changes,
            selected_is("org.mpris.MediaPlayer2.vlc"),
        );

        let name = "org.mpris.MediaPlayer2.vlc";
        watcher.command(name, &Command::PlayPause).unwrap();
        watcher.command(name, &Command::Next).unwrap();
        assert!(matches!(
            watcher.command(name, &Command::Previous),
            Err(CommandError::Unsupported)
        ));
        watcher
            .command(
                name,
                &Command::SetPosition {
                    track_id: "/org/videolan/vlc/playlist/3".into(),
                    position: Duration::from_secs(30),
                },
            )
            .unwrap();
        assert!(matches!(
            watcher.command(
                name,
                &Command::SetPosition {
                    track_id: "/org/videolan/vlc/playlist/2".into(),
                    position: Duration::from_secs(30),
                },
            ),
            Err(CommandError::StaleTrack)
        ));
        assert!(matches!(
            watcher.command("org.mpris.MediaPlayer2.gone", &Command::PlayPause),
            Err(CommandError::UnknownPlayer)
        ));
        assert_eq!(
            *vlc.shared.calls.lock().unwrap(),
            [
                "PlayPause",
                "Next",
                "SetPosition /org/videolan/vlc/playlist/3 30000000",
            ]
        );
    }

    #[test]
    fn players_without_track_ids_seek_relative_to_the_estimate() {
        let bus = PrivateBus::start();
        let browser = FakeService::start(
            &bus,
            "org.mpris.MediaPlayer2.chromium.instance42",
            Shared::new("Paused", "Video", None),
        );
        let (watcher, changes) = watch(&bus);
        wait_for(
            &watcher,
            &changes,
            selected_is("org.mpris.MediaPlayer2.chromium.instance42"),
        );
        watcher
            .command(
                "org.mpris.MediaPlayer2.chromium.instance42",
                &Command::SetPosition {
                    track_id: String::new(),
                    position: Duration::from_secs(65),
                },
            )
            .unwrap();
        assert_eq!(*browser.shared.calls.lock().unwrap(), ["Seek 60000000"]);
    }

    #[test]
    fn property_decoding_tolerates_the_variations_players_send() {
        let mut dict = Dict::new(&Signature::Str, &Signature::Variant);
        // One artist as a plain string, a length as an unsigned integer in a
        // variant nested twice, a track ID as a string, an empty title.
        for (key, value) in [
            ("xesam:artist", variant(Value::from("Solo"))),
            (
                "mpris:length",
                variant(variant(Value::from(180_000_000_u64))),
            ),
            (
                "mpris:trackid",
                variant(Value::from("/not/an/object/path/type")),
            ),
            ("xesam:title", variant(Value::from(""))),
        ] {
            dict.append(Value::from(key), value).unwrap();
        }
        let Some(Property::Metadata(metadata)) =
            property(PLAYER_INTERFACE, "Metadata", &Value::from(dict))
        else {
            panic!("metadata did not decode");
        };
        assert_eq!(metadata.artists, ["Solo"]);
        assert_eq!(metadata.length, Some(Duration::from_secs(180)));
        assert_eq!(
            metadata.track_id.as_deref(),
            Some("/not/an/object/path/type")
        );
        assert_eq!(metadata.title, None);

        assert_eq!(
            property(PLAYER_INTERFACE, "Position", &Value::from(-1_i64)),
            None
        );
        assert_eq!(
            property(PLAYER_INTERFACE, "Rate", &Value::from(1_u32)),
            Some(Property::Rate(1.0))
        );
        assert_eq!(
            property(
                PLAYER_INTERFACE,
                "PlaybackStatus",
                &Value::from("Buffering")
            ),
            None
        );
        let long = "é".repeat(MAXIMUM_TEXT_BYTES);
        let Some(Property::Identity(identity)) =
            property(ROOT_INTERFACE, "Identity", &Value::from(long.as_str()))
        else {
            panic!("identity did not decode");
        };
        assert!(identity.len() <= MAXIMUM_TEXT_BYTES && identity.chars().all(|c| c == 'é'));
        assert_eq!(
            metadata_artists(Value::from(vec!["a\nb".to_owned()])),
            ["ab"],
            "control characters are removed"
        );
    }

    fn variant(value: Value<'_>) -> Value<'_> {
        Value::Value(Box::new(value))
    }

    fn metadata_artists(artists: Value<'_>) -> Vec<String> {
        let mut dict = Dict::new(&Signature::Str, &Signature::Variant);
        dict.append(Value::from("xesam:artist"), variant(artists))
            .unwrap();
        match property(PLAYER_INTERFACE, "Metadata", &Value::from(dict)) {
            Some(Property::Metadata(metadata)) => metadata.artists,
            other => panic!("unexpected {other:?}"),
        }
    }
}
