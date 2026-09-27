//! Player state and the "now playing" choice, without any I/O.
//!
//! macOS gives the Touch Bar one now-playing session: the application that
//! most recently started playback keeps it while paused, until another one
//! starts playing or goes away. The same rule applies here, so pausing a video
//! does not hand the controls to music that was already playing behind it.

use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

/// Every MPRIS player owns a bus name under this prefix.
pub const BUS_NAME_PREFIX: &str = "org.mpris.MediaPlayer2.";

/// playerctld mirrors the player it considers active under its own name.
/// Tracking it too would show that session twice.
const MIRRORS: [&str; 1] = ["org.mpris.MediaPlayer2.playerctld"];

/// Rates other than zero outside this range are ignored; MPRIS players use
/// 1.0 and a few speed-ups or slow-downs, never anything near the limits. A
/// vanishing rate would put the next second of the track beyond any duration.
const MINIMUM_RATE: f64 = 1.0 / 64.0;
const MAXIMUM_RATE: f64 = 64.0;

/// Whether a bus name belongs to a player this crate follows.
pub fn is_player_bus_name(name: &str) -> bool {
    name.strip_prefix(BUS_NAME_PREFIX)
        .is_some_and(|rest| !rest.is_empty())
        && !MIRRORS.contains(&name)
}

/// The player's name without the MPRIS prefix and any instance suffix, for
/// example `firefox` for `org.mpris.MediaPlayer2.firefox.instance_1_42`.
pub fn player_name(bus_name: &str) -> &str {
    let rest = bus_name.strip_prefix(BUS_NAME_PREFIX).unwrap_or(bus_name);
    rest.split('.').next().unwrap_or(rest)
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl PlaybackStatus {
    /// Parses MPRIS `PlaybackStatus`.
    pub fn from_mpris(value: &str) -> Option<Self> {
        match value {
            "Playing" => Some(Self::Playing),
            "Paused" => Some(Self::Paused),
            "Stopped" => Some(Self::Stopped),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Metadata {
    /// `mpris:trackid`, an object path that `SetPosition` must repeat.
    pub track_id: Option<String>,
    pub title: Option<String>,
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub length: Option<Duration>,
}

impl Metadata {
    fn same_track(&self, other: &Self) -> bool {
        self.track_id == other.track_id && self.title == other.title
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Capabilities {
    pub can_control: bool,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_go_next: bool,
    pub can_go_previous: bool,
    pub can_seek: bool,
}

/// One decoded MPRIS property.
#[derive(Clone, Debug, PartialEq)]
pub enum Property {
    Identity(String),
    DesktopEntry(String),
    Status(PlaybackStatus),
    Metadata(Metadata),
    Rate(f64),
    Position(Duration),
    CanControl(bool),
    CanPlay(bool),
    CanPause(bool),
    CanGoNext(bool),
    CanGoPrevious(bool),
    CanSeek(bool),
}

/// The now-playing session, with its position worked out for one instant.
#[derive(Clone, Debug, PartialEq)]
pub struct NowPlaying {
    pub bus_name: String,
    pub identity: Option<String>,
    pub desktop_entry: Option<String>,
    pub status: PlaybackStatus,
    pub metadata: Metadata,
    pub capabilities: Capabilities,
    pub rate: f64,
    /// `None` when the player does not report its position.
    pub position: Option<Duration>,
}

impl NowPlaying {
    pub fn player_name(&self) -> &str {
        player_name(&self.bus_name)
    }
}

#[derive(Clone, Debug)]
struct Player {
    /// The unique connection name that owns the bus name. Signals carry only
    /// this, so it is how they are attributed.
    owner: String,
    identity: Option<String>,
    desktop_entry: Option<String>,
    status: PlaybackStatus,
    metadata: Metadata,
    capabilities: Capabilities,
    rate: f64,
    /// The position last reported by the player, and when.
    position: Option<(Duration, Instant)>,
    /// When the player last started playing, as a sequence number, or zero if
    /// it has not been seen starting.
    started: u64,
}

impl Player {
    fn new(owner: &str) -> Self {
        Self {
            owner: owner.into(),
            identity: None,
            desktop_entry: None,
            status: PlaybackStatus::Stopped,
            metadata: Metadata::default(),
            capabilities: Capabilities::default(),
            rate: 1.0,
            position: None,
            started: 0,
        }
    }

    /// Playing or paused: a player that can hold the now-playing session.
    fn is_current(&self) -> bool {
        matches!(
            self.status,
            PlaybackStatus::Playing | PlaybackStatus::Paused
        )
    }

    fn position_at(&self, now: Instant) -> Option<Duration> {
        let (position, at) = self.position?;
        let position = if self.status == PlaybackStatus::Playing {
            position.saturating_add(now.saturating_duration_since(at).mul_f64(self.rate))
        } else {
            position
        };
        Some(match self.metadata.length {
            Some(length) => position.min(length),
            None => position,
        })
    }

    /// Re-anchors the position at `now`, so that a change of status or rate
    /// only affects the time after it.
    fn freeze_position(&mut self, now: Instant) {
        self.position = self.position_at(now).map(|position| (position, now));
    }

    /// Applies one property and reports what changed.
    fn set(&mut self, property: Property, now: Instant) -> Outcome {
        let mut outcome = Outcome::default();
        match property {
            Property::Identity(identity) => self.identity = Some(identity),
            Property::DesktopEntry(entry) => self.desktop_entry = Some(entry),
            Property::Status(status) if status != self.status => {
                self.freeze_position(now);
                self.status = status;
                outcome.started = status == PlaybackStatus::Playing;
                // Players often correct their position when they start or stop.
                outcome.reread_position = true;
            }
            Property::Status(_) => {}
            Property::Metadata(metadata) if metadata != self.metadata => {
                if !metadata.same_track(&self.metadata) {
                    self.position = None;
                }
                self.metadata = metadata;
                outcome.reread_position = self.position.is_none();
            }
            Property::Metadata(_) => {}
            Property::Rate(rate) => {
                let valid = rate == 0.0 || (MINIMUM_RATE..=MAXIMUM_RATE).contains(&rate);
                if valid && rate != self.rate {
                    self.freeze_position(now);
                    self.rate = rate;
                }
            }
            Property::Position(position) => self.position = Some((position, now)),
            Property::CanControl(value) => self.capabilities.can_control = value,
            Property::CanPlay(value) => self.capabilities.can_play = value,
            Property::CanPause(value) => self.capabilities.can_pause = value,
            Property::CanGoNext(value) => self.capabilities.can_go_next = value,
            Property::CanGoPrevious(value) => self.capabilities.can_go_previous = value,
            Property::CanSeek(value) => self.capabilities.can_seek = value,
        }
        outcome
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Outcome {
    started: bool,
    reread_position: bool,
}

/// Every player on the bus, keyed by bus name.
#[derive(Debug, Default)]
pub struct Players {
    players: BTreeMap<String, Player>,
    starts: u64,
}

impl Players {
    /// Adds a player, replacing any earlier one under the same bus name.
    pub fn insert(
        &mut self,
        bus_name: &str,
        owner: &str,
        properties: impl IntoIterator<Item = Property>,
        now: Instant,
    ) {
        let mut player = Player::new(owner);
        for property in position_last(properties) {
            let _ = player.set(property, now);
        }
        // A player that appears already playing has just started.
        if player.status == PlaybackStatus::Playing {
            player.started = self.next_start();
        }
        self.players.insert(bus_name.into(), player);
    }

    pub fn remove(&mut self, bus_name: &str) -> bool {
        self.players.remove(bus_name).is_some()
    }

    pub fn contains(&self, bus_name: &str) -> bool {
        self.players.contains_key(bus_name)
    }

    pub fn owner(&self, bus_name: &str) -> Option<&str> {
        self.players
            .get(bus_name)
            .map(|player| player.owner.as_str())
    }

    /// The bus names that one connection owns.
    pub fn owned_by(&self, owner: &str) -> Vec<String> {
        self.players
            .iter()
            .filter(|(_, player)| player.owner == owner)
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// Applies one property of a known player. Returns true when its position
    /// should be read again.
    pub fn update(&mut self, bus_name: &str, property: Property, now: Instant) -> bool {
        let Some(player) = self.players.get_mut(bus_name) else {
            return false;
        };
        let outcome = player.set(property, now);
        if outcome.started {
            let started = self.next_start();
            if let Some(player) = self.players.get_mut(bus_name) {
                player.started = started;
            }
        }
        outcome.reread_position
    }

    /// Applies properties that arrived together, the position last: a new
    /// track in the same batch would otherwise discard it. Returns true when
    /// the position should be read again.
    pub fn update_all(
        &mut self,
        bus_name: &str,
        properties: impl IntoIterator<Item = Property>,
        now: Instant,
    ) -> bool {
        let mut reread = false;
        let mut fresh = false;
        for property in position_last(properties) {
            fresh |= matches!(property, Property::Position(_));
            reread |= self.update(bus_name, property, now);
        }
        reread && !fresh
    }

    pub fn capabilities(&self, bus_name: &str) -> Option<Capabilities> {
        self.players.get(bus_name).map(|player| player.capabilities)
    }

    pub fn track_id(&self, bus_name: &str) -> Option<&str> {
        self.players
            .get(bus_name)
            .and_then(|player| player.metadata.track_id.as_deref())
    }

    pub fn position(&self, bus_name: &str, now: Instant) -> Option<Duration> {
        self.players
            .get(bus_name)
            .and_then(|player| player.position_at(now))
    }

    /// The bus name of the now-playing player: of the players that are
    /// playing or paused, the one that started playing last. A player seen
    /// only paused ranks below every player seen starting; ties go to the
    /// playing one, then to the first name.
    pub fn selected(&self) -> Option<&str> {
        self.selected_preferring(None)
    }

    /// Like [`Players::selected`], but `preferred` wins while it is playing
    /// or paused. The start order is history that a watcher started later
    /// cannot know, so processes that must agree follow one authority's
    /// choice instead of ranking on their own.
    pub fn selected_preferring(&self, preferred: Option<&str>) -> Option<&str> {
        if let Some((name, _)) = preferred.and_then(|name| self.players.get_key_value(name))
            && self.players[name].is_current()
        {
            return Some(name.as_str());
        }
        self.players
            .iter()
            .filter(|(_, player)| player.is_current())
            .max_by(|(left_name, left), (right_name, right)| {
                left.started
                    .cmp(&right.started)
                    .then_with(|| {
                        (left.status == PlaybackStatus::Playing)
                            .cmp(&(right.status == PlaybackStatus::Playing))
                    })
                    .then_with(|| right_name.cmp(left_name))
            })
            .map(|(name, _)| name.as_str())
    }

    pub fn now_playing(&self, now: Instant) -> Option<NowPlaying> {
        self.now_playing_preferring(now, None)
    }

    /// The now-playing session as chosen by [`Players::selected_preferring`].
    pub fn now_playing_preferring(
        &self,
        now: Instant,
        preferred: Option<&str>,
    ) -> Option<NowPlaying> {
        let bus_name = self.selected_preferring(preferred)?;
        let player = &self.players[bus_name];
        Some(NowPlaying {
            bus_name: bus_name.into(),
            identity: player.identity.clone(),
            desktop_entry: player.desktop_entry.clone(),
            status: player.status,
            metadata: player.metadata.clone(),
            capabilities: player.capabilities,
            rate: player.rate,
            position: player.position_at(now),
        })
    }

    fn next_start(&mut self) -> u64 {
        self.starts += 1;
        self.starts
    }
}

fn position_last(properties: impl IntoIterator<Item = Property>) -> Vec<Property> {
    let mut properties = properties.into_iter().collect::<Vec<_>>();
    // A stable sort keeps the other properties in their order.
    properties.sort_by_key(|property| matches!(property, Property::Position(_)));
    properties
}

#[cfg(test)]
mod tests {
    use super::*;

    const VLC: &str = "org.mpris.MediaPlayer2.vlc";
    const FIREFOX: &str = "org.mpris.MediaPlayer2.firefox.instance_1_42";
    const RHYTHMBOX: &str = "org.mpris.MediaPlayer2.rhythmbox";

    fn status(status: PlaybackStatus) -> Property {
        Property::Status(status)
    }

    fn players(entries: &[(&str, PlaybackStatus)], now: Instant) -> Players {
        let mut players = Players::default();
        for (index, (name, playback)) in entries.iter().enumerate() {
            players.insert(name, &format!(":1.{index}"), [status(*playback)], now);
        }
        players
    }

    fn track(title: &str, seconds: u64) -> Metadata {
        Metadata {
            track_id: Some(format!("/track/{title}")),
            title: Some(title.into()),
            artists: vec!["Artist".into()],
            album: None,
            length: Some(Duration::from_secs(seconds)),
        }
    }

    #[test]
    fn bus_names_map_to_players_but_not_to_the_playerctld_mirror() {
        assert!(is_player_bus_name(VLC));
        assert!(is_player_bus_name(FIREFOX));
        assert!(!is_player_bus_name("org.mpris.MediaPlayer2.playerctld"));
        assert!(!is_player_bus_name("org.mpris.MediaPlayer2."));
        assert!(!is_player_bus_name("org.mpris.MediaPlayer2"));
        assert!(!is_player_bus_name("org.gnome.Shell"));
        assert_eq!(player_name(VLC), "vlc");
        assert_eq!(player_name(FIREFOX), "firefox");
        assert_eq!(
            player_name("org.mpris.MediaPlayer2.chromium.instance4242"),
            "chromium"
        );
    }

    #[test]
    fn nothing_is_now_playing_until_a_player_plays_or_pauses() {
        let now = Instant::now();
        let mut players = players(&[(VLC, PlaybackStatus::Stopped)], now);
        assert_eq!(players.selected(), None);
        assert_eq!(players.now_playing(now), None);
        players.update(VLC, status(PlaybackStatus::Paused), now);
        assert_eq!(players.selected(), Some(VLC));
    }

    #[test]
    fn the_player_that_started_last_keeps_the_session_while_paused() {
        let now = Instant::now();
        let mut players = players(
            &[
                (VLC, PlaybackStatus::Paused),
                (FIREFOX, PlaybackStatus::Paused),
            ],
            now,
        );
        players.update(VLC, status(PlaybackStatus::Playing), now);
        assert_eq!(players.selected(), Some(VLC));

        // A video starts: it takes over.
        players.update(FIREFOX, status(PlaybackStatus::Playing), now);
        assert_eq!(players.selected(), Some(FIREFOX));

        // Pausing the video keeps its controls, although music still plays.
        players.update(FIREFOX, status(PlaybackStatus::Paused), now);
        assert_eq!(players.selected(), Some(FIREFOX));

        // Stopping it, or closing the tab, returns them to the music.
        players.update(FIREFOX, status(PlaybackStatus::Stopped), now);
        assert_eq!(players.selected(), Some(VLC));
        players.update(FIREFOX, status(PlaybackStatus::Playing), now);
        assert!(players.remove(FIREFOX));
        assert_eq!(players.selected(), Some(VLC));
    }

    #[test]
    fn players_found_at_startup_rank_playing_first_then_by_name() {
        let now = Instant::now();
        let both_paused = players(
            &[
                (VLC, PlaybackStatus::Paused),
                (RHYTHMBOX, PlaybackStatus::Paused),
            ],
            now,
        );
        assert_eq!(both_paused.selected(), Some(RHYTHMBOX));

        let one_playing = players(
            &[
                (RHYTHMBOX, PlaybackStatus::Paused),
                (VLC, PlaybackStatus::Playing),
            ],
            now,
        );
        assert_eq!(one_playing.selected(), Some(VLC));

        // Discovery lists names in order, so the last playing name started
        // last in every watcher.
        let mut sorted = [
            (VLC, PlaybackStatus::Playing),
            (FIREFOX, PlaybackStatus::Playing),
        ];
        sorted.sort_by_key(|(name, _)| *name);
        assert_eq!(players(&sorted, now).selected(), Some(VLC));
    }

    #[test]
    fn a_preferred_player_wins_only_while_it_plays_or_pauses() {
        let now = Instant::now();
        let mut players = players(
            &[
                (FIREFOX, PlaybackStatus::Playing),
                (VLC, PlaybackStatus::Playing),
            ],
            now,
        );
        // Found playing together, VLC ranks last by name here, but the
        // authority saw Firefox start later.
        assert_eq!(players.selected(), Some(VLC));
        assert_eq!(players.selected_preferring(Some(FIREFOX)), Some(FIREFOX));
        assert_eq!(
            players
                .now_playing_preferring(now, Some(FIREFOX))
                .map(|playing| playing.bus_name),
            Some(FIREFOX.to_owned())
        );
        // A preference for a stopped or unknown player falls back.
        players.update(FIREFOX, status(PlaybackStatus::Stopped), now);
        assert_eq!(players.selected_preferring(Some(FIREFOX)), Some(VLC));
        assert_eq!(players.selected_preferring(Some(RHYTHMBOX)), Some(VLC));
    }

    #[test]
    fn position_advances_only_while_playing_and_stops_at_the_end() {
        let start = Instant::now();
        let mut players = Players::default();
        players.insert(
            VLC,
            ":1.1",
            [
                status(PlaybackStatus::Playing),
                Property::Metadata(track("one", 100)),
                Property::Position(Duration::from_secs(10)),
            ],
            start,
        );
        assert_eq!(
            players.position(VLC, start + Duration::from_secs(5)),
            Some(Duration::from_secs(15))
        );

        // Pausing freezes the position at the moment of the pause.
        players.update(
            VLC,
            status(PlaybackStatus::Paused),
            start + Duration::from_secs(20),
        );
        assert_eq!(
            players.position(VLC, start + Duration::from_secs(60)),
            Some(Duration::from_secs(30))
        );

        // Double speed from the resume onwards, clamped to the track length.
        let resumed = start + Duration::from_secs(60);
        players.update(VLC, status(PlaybackStatus::Playing), resumed);
        players.update(VLC, Property::Rate(2.0), resumed);
        assert_eq!(
            players.position(VLC, resumed + Duration::from_secs(10)),
            Some(Duration::from_secs(50))
        );
        assert_eq!(
            players.position(VLC, resumed + Duration::from_secs(600)),
            Some(Duration::from_secs(100))
        );

        // Nonsense rates are ignored.
        players.update(VLC, Property::Rate(f64::NAN), resumed);
        players.update(VLC, Property::Rate(-1.0), resumed);
        players.update(VLC, Property::Rate(f64::MIN_POSITIVE), resumed);
        players.update(VLC, Property::Rate(f64::INFINITY), resumed);
        assert_eq!(
            players.now_playing(resumed).map(|playing| playing.rate),
            Some(2.0)
        );
    }

    #[test]
    fn a_new_track_forgets_the_old_position_and_asks_for_the_new_one() {
        let now = Instant::now();
        let mut players = Players::default();
        players.insert(
            VLC,
            ":1.1",
            [
                status(PlaybackStatus::Playing),
                Property::Metadata(track("one", 100)),
                Property::Position(Duration::from_secs(42)),
            ],
            now,
        );
        // More metadata for the same track keeps the position.
        let mut longer = track("one", 100);
        longer.album = Some("Album".into());
        assert!(!players.update(VLC, Property::Metadata(longer), now));
        assert_eq!(players.position(VLC, now), Some(Duration::from_secs(42)));

        assert!(players.update(VLC, Property::Metadata(track("two", 100)), now));
        assert_eq!(players.position(VLC, now), None);
        assert_eq!(players.track_id(VLC), Some("/track/two"));
    }

    #[test]
    fn a_position_sent_with_a_new_track_is_kept() {
        let now = Instant::now();
        let mut players = Players::default();
        players.insert(
            VLC,
            ":1.1",
            [
                Property::Position(Duration::from_secs(7)),
                Property::Metadata(track("one", 100)),
            ],
            now,
        );
        assert_eq!(players.position(VLC, now), Some(Duration::from_secs(7)));
        let reread = players.update_all(
            VLC,
            [
                Property::Position(Duration::from_secs(1)),
                Property::Metadata(track("two", 100)),
                status(PlaybackStatus::Playing),
            ],
            now,
        );
        assert!(!reread, "the batch carried the new position");
        assert_eq!(players.position(VLC, now), Some(Duration::from_secs(1)));
        assert!(players.update_all(VLC, [status(PlaybackStatus::Paused)], now));
    }

    #[test]
    fn status_changes_ask_for_a_fresh_position() {
        let now = Instant::now();
        let mut players = players(&[(VLC, PlaybackStatus::Paused)], now);
        assert!(players.update(VLC, status(PlaybackStatus::Playing), now));
        assert!(!players.update(VLC, status(PlaybackStatus::Playing), now));
    }

    #[test]
    fn signals_are_attributed_by_owner_and_now_playing_reports_everything() {
        let now = Instant::now();
        let mut players = Players::default();
        players.insert(
            FIREFOX,
            ":1.7",
            [
                Property::Identity("Firefox".into()),
                Property::DesktopEntry("firefox_firefox".into()),
                status(PlaybackStatus::Playing),
                Property::Metadata(track("video", 300)),
                Property::CanControl(true),
                Property::CanPlay(true),
                Property::CanPause(true),
                Property::CanSeek(true),
                Property::CanGoNext(false),
                Property::CanGoPrevious(false),
            ],
            now,
        );
        assert_eq!(players.owned_by(":1.7"), [FIREFOX]);
        assert!(players.owned_by(":1.8").is_empty());
        assert_eq!(players.owner(FIREFOX), Some(":1.7"));
        let playing = players.now_playing(now).unwrap();
        assert_eq!(playing.player_name(), "firefox");
        assert_eq!(playing.identity.as_deref(), Some("Firefox"));
        assert_eq!(playing.desktop_entry.as_deref(), Some("firefox_firefox"));
        assert_eq!(playing.status, PlaybackStatus::Playing);
        assert_eq!(playing.metadata.title.as_deref(), Some("video"));
        assert!(playing.capabilities.can_seek && !playing.capabilities.can_go_next);
        assert_eq!(playing.position, None);
    }
}
