//! `media.*` context facts from MPRIS players, for profile rules.
//!
//! The facts describe the same now-playing session that the media plugin
//! shows: the player that most recently started playing, kept while paused.
//! Rules can then show the media controls when the focused application is
//! the one playing, for example a browser playing a video. The chosen player
//! is also recorded for the plugin supervisors (`touchbar_mpris::session_file`),
//! which cannot know which of several players started last when they start
//! after them; following the record, the media items control the player the
//! facts name.

use std::{
    os::fd::RawFd,
    path::PathBuf,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use touchbar_mpris::{MprisWatcher, NowPlaying, PlaybackStatus, session_file};

use crate::{
    context::{ContextChange, ContextEvent},
    wake::EventSignal,
};

/// The player's name without the MPRIS prefix and instance suffix: `vlc`,
/// or `firefox` for `org.mpris.MediaPlayer2.firefox.instance_1_42`.
pub const MEDIA_PLAYER: &str = "media.player";
/// `playing` or `paused`.
pub const MEDIA_PLAYBACK_STATUS: &str = "media.playback-status";
/// True while the session plays and for a while after it stops.
pub const MEDIA_ACTIVE: &str = "media.active";
/// Long enough to resume a paused video from the bar, short enough that a
/// video paused and forgotten in a tab stops claiming the browser's bar.
const ACTIVE_AFTER_PLAYBACK: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Eq, PartialEq)]
struct Facts {
    bus_name: String,
    player: String,
    status: PlaybackStatus,
    active: bool,
}

/// Follows the session bus; without a now-playing session every `media.*`
/// fact is absent.
pub struct MediaContextSource {
    watcher: MprisWatcher,
    signal: EventSignal,
    published: Option<Facts>,
    /// The player last seen playing and when that stops making it active.
    active_until: Option<(String, Instant)>,
    /// Where the chosen player is recorded for the plugin supervisors.
    record: Option<PathBuf>,
}

impl MediaContextSource {
    pub fn start(record: Option<PathBuf>) -> Result<(Self, Vec<ContextChange>)> {
        let signal = EventSignal::new().context("create media context signal")?;
        let watcher_signal = signal.try_clone().context("clone media context signal")?;
        let watcher = MprisWatcher::session(move || watcher_signal.notify())
            .context("follow MPRIS players on the session bus")?;
        Ok(Self::with_watcher(watcher, signal, record))
    }

    fn with_watcher(
        watcher: MprisWatcher,
        signal: EventSignal,
        record: Option<PathBuf>,
    ) -> (Self, Vec<ContextChange>) {
        let mut source = Self {
            watcher,
            signal,
            published: None,
            active_until: None,
            record,
        };
        let initial = source.settle(Instant::now());
        (source, initial)
    }

    pub fn notification_fd(&self) -> RawFd {
        self.signal.as_raw_fd()
    }

    /// The fact changes since the last call, including an expired
    /// `media.active`. The event loop calls this every iteration, so the
    /// players are read only after a bus change or at that deadline.
    pub fn poll(&mut self) -> Result<Vec<ContextChange>> {
        let signalled = self.signal.drain().context("drain media context signal")?;
        if self.watcher.stopped() {
            bail!("the session bus connection closed");
        }
        if !signalled && self.next_deadline().is_none_or(|left| !left.is_zero()) {
            return Ok(Vec::new());
        }
        Ok(self.settle(Instant::now()))
    }

    /// How long until `media.active` turns false without any bus signal.
    pub fn next_deadline(&self) -> Option<Duration> {
        let facts = self.published.as_ref()?;
        if !facts.active || facts.status == PlaybackStatus::Playing {
            return None;
        }
        let (_, until) = self.active_until.as_ref()?;
        Some(until.saturating_duration_since(Instant::now()))
    }

    /// Withdraws the facts this source set, once the source itself stops.
    pub fn withdraw(&mut self) -> Vec<ContextChange> {
        let previous = self.published.take();
        self.record_choice(previous.as_ref(), None);
        changes(previous.as_ref(), None)
    }

    fn settle(&mut self, now: Instant) -> Vec<ContextChange> {
        let facts = self.watcher.now_playing().map(|playing| {
            facts(
                self.published.as_ref(),
                &mut self.active_until,
                &playing,
                now,
            )
        });
        let previous = std::mem::replace(&mut self.published, facts);
        self.record_choice(previous.as_ref(), self.published.as_ref());
        changes(previous.as_ref(), self.published.as_ref())
    }

    /// Records a change of player; the facts stand even if that fails, and
    /// the supervisors then rank the players on their own.
    fn record_choice(&self, previous: Option<&Facts>, next: Option<&Facts>) {
        fn bus_name(facts: Option<&Facts>) -> Option<&str> {
            facts.map(|facts| facts.bus_name.as_str())
        }
        let Some(record) = &self.record else {
            return;
        };
        if bus_name(previous) == bus_name(next) {
            return;
        }
        if let Err(error) = session_file::write(record, bus_name(next)) {
            eprintln!(
                "context-source=media record={} error={error}",
                record.display()
            );
        }
    }
}

fn facts(
    published: Option<&Facts>,
    active_until: &mut Option<(String, Instant)>,
    playing: &NowPlaying,
    now: Instant,
) -> Facts {
    // A pause that arrives between two checks still counts from the moment
    // it was noticed, since the session was playing until then.
    let was_playing = published.is_some_and(|facts| {
        facts.bus_name == playing.bus_name && facts.status == PlaybackStatus::Playing
    });
    if playing.status == PlaybackStatus::Playing || was_playing {
        *active_until = Some((playing.bus_name.clone(), now + ACTIVE_AFTER_PLAYBACK));
    }
    let active = playing.status == PlaybackStatus::Playing
        || active_until
            .as_ref()
            .is_some_and(|(bus_name, until)| *bus_name == playing.bus_name && now < *until);
    Facts {
        bus_name: playing.bus_name.clone(),
        player: playing.player_name().to_owned(),
        status: playing.status,
        active,
    }
}

fn changes(previous: Option<&Facts>, next: Option<&Facts>) -> Vec<ContextChange> {
    let Some(next) = next else {
        return match previous {
            Some(_) => [MEDIA_PLAYER, MEDIA_PLAYBACK_STATUS, MEDIA_ACTIVE]
                .map(|key| ContextChange::Cleared(key.into()))
                .into(),
            None => Vec::new(),
        };
    };
    let mut changes = Vec::new();
    if previous.map(|facts| facts.player.as_str()) != Some(next.player.as_str()) {
        changes.push(ContextChange::Set(ContextEvent::text(
            MEDIA_PLAYER,
            next.player.clone(),
        )));
    }
    if previous.map(|facts| facts.status) != Some(next.status) {
        changes.push(ContextChange::Set(ContextEvent::text(
            MEDIA_PLAYBACK_STATUS,
            next.status.as_str(),
        )));
    }
    if previous.map(|facts| facts.active) != Some(next.active) {
        changes.push(ContextChange::Set(ContextEvent::boolean(
            MEDIA_ACTIVE,
            next.active,
        )));
    }
    changes
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        os::fd::AsRawFd,
        sync::{Arc, Mutex},
    };

    use zbus::{
        blocking::{Connection, connection::Builder},
        zvariant::{OwnedValue, Value},
    };

    use super::*;
    use crate::test_bus::PrivateBus;

    fn set_text(key: &str, value: &str) -> ContextChange {
        ContextChange::Set(ContextEvent::text(key, value))
    }

    fn set_active(value: bool) -> ContextChange {
        ContextChange::Set(ContextEvent::boolean(MEDIA_ACTIVE, value))
    }

    fn published(bus_name: &str, status: PlaybackStatus, active: bool) -> Facts {
        Facts {
            bus_name: bus_name.into(),
            player: touchbar_mpris::player_name(bus_name).into(),
            status,
            active,
        }
    }

    #[test]
    fn only_changed_facts_are_reported_and_all_are_withdrawn_together() {
        let firefox = "org.mpris.MediaPlayer2.firefox.instance_1_7";
        let playing = published(firefox, PlaybackStatus::Playing, true);
        assert_eq!(
            changes(None, Some(&playing)),
            [
                set_text(MEDIA_PLAYER, "firefox"),
                set_text(MEDIA_PLAYBACK_STATUS, "playing"),
                set_active(true),
            ]
        );
        let paused = published(firefox, PlaybackStatus::Paused, true);
        assert_eq!(
            changes(Some(&playing), Some(&paused)),
            [set_text(MEDIA_PLAYBACK_STATUS, "paused")]
        );
        assert_eq!(
            changes(Some(&paused), None),
            [
                ContextChange::Cleared(MEDIA_PLAYER.into()),
                ContextChange::Cleared(MEDIA_PLAYBACK_STATUS.into()),
                ContextChange::Cleared(MEDIA_ACTIVE.into()),
            ]
        );
        assert!(changes(None, None).is_empty());
    }

    struct FakePlayer(Arc<Mutex<&'static str>>);

    #[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
    impl FakePlayer {
        #[zbus(property)]
        fn playback_status(&self) -> String {
            (*self.0.lock().unwrap()).into()
        }

        #[zbus(property)]
        fn metadata(&self) -> HashMap<String, OwnedValue> {
            HashMap::new()
        }
    }

    fn player(
        bus: &PrivateBus,
        name: &str,
        status: &'static str,
    ) -> (Connection, Arc<Mutex<&'static str>>) {
        let shared = Arc::new(Mutex::new(status));
        let connection = Builder::address(bus.address.as_str())
            .unwrap()
            .name(name)
            .unwrap()
            .serve_at("/org/mpris/MediaPlayer2", FakePlayer(Arc::clone(&shared)))
            .unwrap()
            .build()
            .unwrap();
        (connection, shared)
    }

    fn set_status(connection: &Connection, shared: &Mutex<&'static str>, status: &'static str) {
        *shared.lock().unwrap() = status;
        connection
            .emit_signal(
                None::<&str>,
                "/org/mpris/MediaPlayer2",
                "org.freedesktop.DBus.Properties",
                "PropertiesChanged",
                &(
                    "org.mpris.MediaPlayer2.Player",
                    HashMap::from([("PlaybackStatus", Value::from(status))]),
                    Vec::<String>::new(),
                ),
            )
            .unwrap();
    }

    fn source(bus: &PrivateBus) -> (MediaContextSource, Vec<ContextChange>) {
        let signal = EventSignal::new().unwrap();
        let watcher_signal = signal.try_clone().unwrap();
        let watcher = MprisWatcher::start(bus.connect(), move || watcher_signal.notify()).unwrap();
        MediaContextSource::with_watcher(watcher, signal, Some(bus.file("media-session")))
    }

    /// The next non-empty batch of changes, waiting on the source's eventfd.
    fn next_changes(source: &mut MediaContextSource) -> Vec<ContextChange> {
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            let mut poll_fd = libc::pollfd {
                fd: source.notification_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            unsafe { libc::poll(&mut poll_fd, 1, 50) };
            let changes = source.poll().unwrap();
            if !changes.is_empty() {
                return changes;
            }
        }
        Vec::new()
    }

    #[test]
    fn a_browser_video_sets_the_facts_and_a_pause_keeps_them_active() {
        let bus = PrivateBus::start();
        let (_music, _) = player(&bus, "org.mpris.MediaPlayer2.vlc", "Paused");
        let (mut source, initial) = source(&bus);
        assert_eq!(
            initial,
            [
                set_text(MEDIA_PLAYER, "vlc"),
                set_text(MEDIA_PLAYBACK_STATUS, "paused"),
                set_active(false),
            ],
            "a player found paused has not been active"
        );
        let recorded = || session_file::read(&bus.file("media-session"));
        assert_eq!(recorded().as_deref(), Some("org.mpris.MediaPlayer2.vlc"));

        let (video, status) = player(
            &bus,
            "org.mpris.MediaPlayer2.firefox.instance_1_9",
            "Stopped",
        );
        set_status(&video, &status, "Playing");
        let mut changes = next_changes(&mut source);
        // The appearance may be reported before the status change.
        if changes.is_empty() || !changes.contains(&set_text(MEDIA_PLAYER, "firefox")) {
            changes.extend(next_changes(&mut source));
        }
        assert!(
            changes.contains(&set_text(MEDIA_PLAYER, "firefox")),
            "{changes:?}"
        );
        assert!(
            changes.contains(&set_text(MEDIA_PLAYBACK_STATUS, "playing")),
            "{changes:?}"
        );
        assert!(changes.contains(&set_active(true)), "{changes:?}");
        assert_eq!(
            recorded().as_deref(),
            Some("org.mpris.MediaPlayer2.firefox.instance_1_9")
        );

        set_status(&video, &status, "Paused");
        assert_eq!(
            next_changes(&mut source),
            [set_text(MEDIA_PLAYBACK_STATUS, "paused")]
        );
        let deadline = source.next_deadline().expect("the pause starts the minute");
        assert!(deadline > Duration::from_secs(55) && deadline <= ACTIVE_AFTER_PLAYBACK);

        // The video's tab closes: the session returns to the paused music.
        video.close().unwrap();
        let changes = next_changes(&mut source);
        assert!(
            changes.contains(&set_text(MEDIA_PLAYER, "vlc")),
            "{changes:?}"
        );
        assert!(changes.contains(&set_active(false)), "{changes:?}");
        assert_eq!(source.next_deadline(), None);
        assert_eq!(recorded().as_deref(), Some("org.mpris.MediaPlayer2.vlc"));
        source.withdraw();
        assert_eq!(recorded(), None);
    }

    fn now_playing(bus_name: &str, status: PlaybackStatus) -> NowPlaying {
        NowPlaying {
            bus_name: bus_name.into(),
            identity: None,
            desktop_entry: None,
            status,
            metadata: Default::default(),
            capabilities: Default::default(),
            rate: 1.0,
            position: None,
        }
    }

    #[test]
    fn active_lasts_a_minute_after_playback_stops() {
        let firefox = "org.mpris.MediaPlayer2.firefox.instance_1_9";
        let start = Instant::now();
        let at = |seconds| start + Duration::from_secs(seconds);
        let mut until = None;
        let playing = facts(
            None,
            &mut until,
            &now_playing(firefox, PlaybackStatus::Playing),
            at(0),
        );
        assert!(playing.active);
        // Paused ten seconds in: the minute runs from the pause.
        let paused = now_playing(firefox, PlaybackStatus::Paused);
        let first = facts(Some(&playing), &mut until, &paused, at(10));
        assert!(first.active);
        assert!(facts(Some(&first), &mut until, &paused, at(69)).active);
        let expired = facts(Some(&first), &mut until, &paused, at(70));
        assert!(!expired.active);
        // Another player that was only ever paused is not active.
        let music = now_playing("org.mpris.MediaPlayer2.vlc", PlaybackStatus::Paused);
        assert!(!facts(Some(&expired), &mut until, &music, at(71)).active);
    }
}
