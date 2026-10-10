//! Media: the now-playing session of MPRIS players, as on macOS.
//!
//! `media.read.v1` names one session, the player that most recently started
//! playing, kept while it is paused. Its state arrives on every change and
//! once per second while a track plays. `media.control.v1` sends a command to
//! that player for a touch on one of these controls.

use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    time::Instant,
};
use touchbar_component_sdk::kit::{InputEvent, InputKind};
use touchbar_component_sdk::{
    HostEvent, MediaAction, MediaCommand, MediaPlaybackStatus, MediaSession, MediaState,
    PresentationEvent, RenderRequest, Update, decode_media_state, decode_media_subscription_opened,
    touchbar::plugin::{
        broker::{CapabilityStatus, OperationResult},
        ui::{
            PresentationBegin, PresentationCommand, PresentationLifecycle, PresentationPlacement,
        },
    },
};
use touchbar_logic_sdk::{Guest, description_to_wit, engine::ENGINE, wire};
use touchbar_set_config::{
    ActionDescription, InputKind as LogicKind, LogicDescription, ScalarType, StateType,
};
const EXPAND: u64 = 10;
const COMPACT_TOGGLE: u64 = 11;
const PREVIOUS: u64 = 20;
const TOGGLE: u64 = 21;
const NEXT: u64 = 22;
const SCRUBBER: u64 = 30;

const READ: &str = "media.read.v1";
const CONTROL: &str = "media.control.v1";
/// A seek counts as done when the player reports a position this close to it.
const SEEK_TOLERANCE_MICROS: u64 = 1_500_000;
/// Otherwise the target is shown for at most this many states.
const SEEK_STATES: u8 = 3;
/// A subscription the broker ends is opened again at most this often in a row.
const RESUBSCRIBE_ATTEMPTS: u8 = 3;

struct Plugin;

thread_local! {
    static ITEM: RefCell<String> = const { RefCell::new(String::new()) };
    static WIDE_NOW: Cell<bool> = const { Cell::new(false) };
    static WIDE_EMPTY: Cell<bool> = const { Cell::new(false) };
    static WIDE_TIMELINE: Cell<bool> = const { Cell::new(false) };
    static MEDIA: RefCell<Media> = RefCell::new(Media::default());
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum Access {
    /// Before the host has reported the capability.
    #[default]
    Waiting,
    On,
    Off,
}

#[derive(Clone, Copy, Debug)]
struct PendingSeek {
    /// The command's request, whose failure withdraws the seek.
    request: u64,
    position_micros: u64,
    states_left: u8,
}

struct Media {
    visible: bool,
    needs_media: bool,
    read_granted: bool,
    read: Access,
    control: bool,
    subscribe_request: Option<u64>,
    resource: Option<u64>,
    resubscribed: u8,
    session: Option<MediaSession>,
    received: Option<Instant>,
    /// The scrubber's value while a finger drags it.
    scrub: Option<f32>,
    /// Cleared on track replacement; subsequent motion needs a fresh Down.
    scrub_contact: Option<u32>,
    pending_seek: Option<PendingSeek>,
    presenting: bool,
}

impl Default for Media {
    fn default() -> Self {
        Self {
            visible: true,
            needs_media: true,
            read_granted: false,
            read: Access::Waiting,
            control: false,
            subscribe_request: None,
            resource: None,
            resubscribed: 0,
            session: None,
            received: None,
            scrub: None,
            scrub_contact: None,
            pending_seek: None,
            presenting: false,
        }
    }
}
fn patch(media: &Media) -> wire::StatePatch {
    use wire::StateValue::{Boolean as B, Number as N, Text as T};
    let session = media.session.as_ref();
    let playing = session.is_some_and(|s| s.status == MediaPlaybackStatus::Playing);
    let timed =
        session.and_then(|s| Some((media.position(s)?, s.length_micros.filter(|v| *v > 0)?)));
    let (position, length) = timed.unwrap_or((0, 0));
    let position = position.min(length);
    let title = session.map(headline).unwrap_or("");
    let artist = session.map(|s| s.artist.as_str()).unwrap_or("");
    let enabled =
        |allowed: fn(&MediaSession) -> bool| media.control && session.is_some_and(allowed);
    vec![
        (
            "player".into(),
            T(session.map(|s| s.player.as_str()).unwrap_or("").into()),
        ),
        (
            "track_id".into(),
            T(session.map(|s| s.track_id.as_str()).unwrap_or("").into()),
        ),
        (
            "playback".into(),
            T(if playing { "playing" } else { "paused" }.into()),
        ),
        (
            "seek_preview".into(),
            B(media.scrub.is_some() || media.pending_seek.is_some()),
        ),
        ("has_session".into(), B(session.is_some())),
        ("no_session".into(), B(session.is_none())),
        ("has_length".into(), B(timed.is_some())),
        (
            "metadata_only".into(),
            B(session.is_some() && timed.is_none()),
        ),
        ("no_length".into(), B(timed.is_none())),
        ("title".into(), T(title.into())),
        ("artist".into(), T(artist.into())),
        (
            "detail".into(),
            T(session
                .map(|s| {
                    if s.title.is_empty() {
                        ""
                    } else if s.artist.is_empty() {
                        &s.identity
                    } else {
                        &s.artist
                    }
                })
                .unwrap_or("")
                .into()),
        ),
        ("artist_heading".into(), T(format!("— {artist}"))),
        ("status".into(), T(media.status_message().into())),
        (
            "position".into(),
            N(if length > 0 {
                f64::from(fraction(position, length))
            } else {
                0.0
            }),
        ),
        ("duration_micros".into(), N(length as f64)),
        ("position_micros".into(), N(position as f64)),
        ("elapsed".into(), T(clock(position))),
        (
            "remaining".into(),
            T(format!("-{}", clock_rounded_up(length - position))),
        ),
        ("toggle_asset".into(), T(toggle_glyph(playing).into())),
        ("toggle_label".into(), T(toggle_label(playing).into())),
        ("can_toggle".into(), B(enabled(can_toggle))),
        ("can_previous".into(), B(enabled(|s| s.can_go_previous))),
        ("can_next".into(), B(enabled(|s| s.can_go_next))),
        (
            "can_seek".into(),
            B(enabled(|s| s.can_seek) && timed.is_some()),
        ),
        ("can_expand".into(), B(!media.presenting)),
        ("wide_now".into(), B(WIDE_NOW.get())),
        ("wide_empty".into(), B(WIDE_EMPTY.get())),
        (
            "show_artist_heading".into(),
            B(WIDE_TIMELINE.get()
                && !artist.is_empty()
                && session.is_some_and(|s| !s.title.is_empty())),
        ),
    ]
}
fn description() -> LogicDescription {
    let actions = [
        ("expand", vec![LogicKind::Activated, LogicKind::LongPressed]),
        ("compact-toggle", vec![LogicKind::Activated]),
        ("previous", vec![LogicKind::Activated]),
        ("toggle", vec![LogicKind::Activated]),
        ("next", vec![LogicKind::Activated]),
        (
            "seek",
            vec![
                LogicKind::Pressed,
                LogicKind::ValueChanged,
                LogicKind::Activated,
                LogicKind::Released,
                LogicKind::Cancelled,
            ],
        ),
    ]
    .into_iter()
    .map(|(name, events)| {
        (
            name.into(),
            ActionDescription {
                events,
                value_type: if name == "seek" {
                    Some(ScalarType::Number)
                } else {
                    None
                },
            },
        )
    })
    .collect();
    let state = patch(&Media::default())
        .into_iter()
        .map(|(name, value)| {
            (
                name,
                match value {
                    wire::StateValue::Boolean(_) => StateType::Bool,
                    wire::StateValue::Number(_) => StateType::Number,
                    _ => StateType::Text,
                },
            )
        })
        .collect::<BTreeMap<_, _>>();
    LogicDescription { actions, state }
}
fn action_widget(action: &str) -> Option<u64> {
    match action {
        "expand" => Some(EXPAND),
        "compact-toggle" => Some(COMPACT_TOGGLE),
        "previous" => Some(PREVIOUS),
        "toggle" => Some(TOGGLE),
        "next" => Some(NEXT),
        "seek" => Some(SCRUBBER),
        _ => None,
    }
}
impl Guest for Plugin {
    fn describe() -> wire::LogicDescription {
        description_to_wit(&description())
    }
    fn initialize(item: String, config: String) -> Result<wire::StatePatch, String> {
        if !matches!(
            config.as_str(),
            "now-playing" | "transport" | "timeline" | "touchbar-logo"
        ) {
            return Err("unknown media configuration".into());
        }
        ITEM.set(item);
        MEDIA.set(Media {
            needs_media: config != "touchbar-logo",
            ..Media::default()
        });
        MEDIA.with_borrow(|media| Ok(patch(media)))
    }
    fn handle_action(event: wire::ActionEvent) -> Result<wire::LogicUpdate, String> {
        let widget =
            action_widget(&event.action).ok_or_else(|| "unknown media action".to_owned())?;
        let input = InputEvent {
            item_id: ITEM.with_borrow(Clone::clone),
            widget_id: widget,
            kind: event.kind,
            value: match event.value {
                Some(wire::ScalarValue::Number(v)) => Some(v as f32),
                _ => None,
            },
            contact_id: Some(event.contact_id as u32),
        };
        MEDIA.with_borrow_mut(|media| {
            let update = media.input(&input);
            Ok(wire::LogicUpdate {
                state: patch(media),
                presentation: update.presentation,
            })
        })
    }
    fn handle_host_event(event: HostEvent) -> Result<wire::LogicUpdate, String> {
        MEDIA.with_borrow_mut(|media| {
            media.host_event(event);
            Ok(wire::LogicUpdate {
                state: patch(media),
                presentation: None,
            })
        })
    }
    fn handle_context_changed(request: RenderRequest) -> Result<wire::LogicUpdate, String> {
        WIDE_NOW.set(request.viewport.width >= 100.0);
        WIDE_EMPTY.set(request.viewport.width >= 120.0);
        WIDE_TIMELINE.set(request.viewport.width >= 320.0);
        MEDIA.with_borrow(|media| {
            Ok(wire::LogicUpdate {
                state: patch(media),
                presentation: None,
            })
        })
    }
    fn handle_visibility_changed(visible: bool) -> Result<wire::LogicUpdate, String> {
        MEDIA.with_borrow_mut(|media| {
            media.visible = visible;
            if visible {
                if media.read_granted {
                    media.subscribe();
                }
            } else {
                media.stop();
            }
            Ok(wire::LogicUpdate {
                state: patch(media),
                presentation: None,
            })
        })
    }
    fn handle_presentation_event(event: PresentationEvent) -> Result<wire::LogicUpdate, String> {
        MEDIA.with_borrow_mut(|media| {
            media.presenting = !matches!(event, PresentationEvent::Ended(_));
            Ok(wire::LogicUpdate {
                state: patch(media),
                presentation: None,
            })
        })
    }
}

impl Media {
    fn host_event(&mut self, event: HostEvent) -> bool {
        match event {
            HostEvent::CapabilityChanged((_, state)) if state.capability == READ => {
                self.read_granted = state.status == CapabilityStatus::Granted;
                if self.read_granted {
                    self.resubscribed = 0;
                    self.subscribe();
                } else {
                    self.forget();
                    self.read = Access::Off;
                }
                true
            }
            HostEvent::CapabilityChanged((_, state)) if state.capability == CONTROL => {
                self.control = state.status == CapabilityStatus::Granted;
                true
            }
            HostEvent::Completion((request, result)) if Some(request) == self.subscribe_request => {
                self.subscribe_request = None;
                let opened = match result {
                    OperationResult::Success(payload) => {
                        decode_media_subscription_opened(&payload).ok()
                    }
                    OperationResult::Error(_) => None,
                };
                match opened {
                    Some((resource, state)) => {
                        self.resource = Some(resource.into_raw());
                        self.read = Access::On;
                        self.apply(state);
                    }
                    None => self.read = Access::Off,
                }
                true
            }
            // The player refused the seek, or the track changed first: show
            // where it really is instead of waiting for states that, while
            // paused, may never come.
            HostEvent::Completion((request, OperationResult::Error(_)))
                if self
                    .pending_seek
                    .is_some_and(|pending| pending.request == request) =>
            {
                self.pending_seek = None;
                true
            }
            HostEvent::ResourceEvent((resource, _, result)) if Some(resource) == self.resource => {
                match result {
                    OperationResult::Success(payload) => match decode_media_state(&payload) {
                        Ok(state) => {
                            self.resubscribed = 0;
                            self.apply(state);
                            true
                        }
                        Err(_) => false,
                    },
                    // The broker ended the subscription, for example after a
                    // burst it could not deliver; open a new one.
                    OperationResult::Error(_) => {
                        self.forget();
                        if self.resubscribed < RESUBSCRIBE_ATTEMPTS {
                            self.resubscribed += 1;
                            self.subscribe();
                        } else {
                            self.read = Access::Off;
                        }
                        true
                    }
                }
            }
            HostEvent::Shutdown(_) => {
                self.stop();
                self.read = Access::Off;
                true
            }
            _ => false,
        }
    }

    fn subscribe(&mut self) {
        if !self.visible || !self.needs_media || !self.read_granted {
            return;
        }
        if self.resource.is_some() || self.subscribe_request.is_some() {
            return;
        }
        match ENGINE.media().subscribe() {
            Ok(request) => self.subscribe_request = Some(request.into_raw()),
            Err(_) => self.read = Access::Off,
        }
    }

    fn stop(&mut self) {
        if let Some(resource) = self.resource {
            let _ = touchbar_component_sdk::close(touchbar_component_sdk::ResourceId::from_raw(
                resource,
            ));
        }
        if let Some(request) = self.subscribe_request {
            let _ = touchbar_component_sdk::cancel(touchbar_component_sdk::RequestId::from_raw(
                request,
            ));
        }
        if let Some(pending) = self.pending_seek {
            let _ = touchbar_component_sdk::cancel(touchbar_component_sdk::RequestId::from_raw(
                pending.request,
            ));
        }
        self.forget();
        self.presenting = false;
    }
    fn forget(&mut self) {
        self.resource = None;
        self.subscribe_request = None;
        self.session = None;
        self.scrub = None;
        self.scrub_contact = None;
        self.pending_seek = None;
    }

    fn apply(&mut self, state: MediaState) {
        let same_track = match (&self.session, &state.session) {
            (Some(old), Some(new)) => {
                old.player == new.player && old.track_id == new.track_id && old.title == new.title
            }
            _ => false,
        };
        // A scrub or seek meant for another track no longer applies.
        if !same_track {
            self.scrub = None;
            self.scrub_contact = None;
            self.pending_seek = None;
        }
        if let (Some(pending), Some(session)) = (&mut self.pending_seek, &state.session) {
            let arrived = session.position_micros.is_some_and(|position| {
                position.abs_diff(pending.position_micros) < SEEK_TOLERANCE_MICROS
            });
            pending.states_left = pending.states_left.saturating_sub(1);
            if arrived || pending.states_left == 0 {
                self.pending_seek = None;
            }
        }
        self.session = state.session;
        self.received = Some(Instant::now());
    }

    fn input(&mut self, event: &InputEvent) -> Update {
        let mut update = Update {
            rerender: false,
            presentation: None,
        };
        match (event.widget_id, event.kind) {
            (EXPAND, InputKind::Activated | InputKind::LongPressed) if !self.presenting => {
                self.presenting = true;
                update.rerender = true;
                update.presentation = Some(PresentationCommand::Begin(PresentationBegin {
                    placement: PresentationPlacement::Anchored,
                    lifecycle: if event.kind == InputKind::LongPressed {
                        PresentationLifecycle::Transient
                    } else {
                        PresentationLifecycle::Persistent
                    },
                    target: None,
                }));
            }
            (COMPACT_TOGGLE | TOGGLE, InputKind::Activated) => self.command(MediaAction::PlayPause),
            (PREVIOUS, InputKind::Activated) => self.command(MediaAction::Previous),
            (NEXT, InputKind::Activated) => self.command(MediaAction::Next),
            (SCRUBBER, InputKind::Pressed) => {
                self.scrub = None;
                self.scrub_contact = self
                    .session
                    .as_ref()
                    .filter(|session| {
                        self.control && session.can_seek && session.length_micros.is_some()
                    })
                    .map(|_| event.contact_id.unwrap_or(0));
            }
            (SCRUBBER, InputKind::ValueChanged)
                if self.scrub_contact == Some(event.contact_id.unwrap_or(0)) =>
            {
                if let Some(value) = event.value.filter(|value| value.is_finite()) {
                    self.scrub = Some(value.clamp(0.0, 1.0));
                    update.rerender = true;
                }
            }
            // Lifting the finger commits the scrub.
            (SCRUBBER, InputKind::Activated)
                if self.scrub_contact == Some(event.contact_id.unwrap_or(0)) =>
            {
                self.seek();
                self.scrub_contact = None;
                update.rerender = true;
            }
            (SCRUBBER, InputKind::Cancelled | InputKind::Released) => {
                self.scrub = None;
                self.scrub_contact = None;
                update.rerender = true;
            }
            _ => {}
        }
        update
    }

    fn command(&self, action: MediaAction) {
        let Some(session) = &self.session else {
            return;
        };
        if self.control {
            let _ = ENGINE.media().command(&MediaCommand {
                player: session.player.clone(),
                action,
            });
        }
    }

    fn seek(&mut self) {
        let Some(command) = self.seek_command() else {
            self.scrub = None;
            return;
        };
        self.scrub = None;
        let MediaAction::SetPosition {
            position_micros, ..
        } = &command.action
        else {
            return;
        };
        let position_micros = *position_micros;
        if let Ok(request) = ENGINE.media().command(&command) {
            self.pending_seek = Some(PendingSeek {
                request: request.into_raw(),
                position_micros,
                states_left: SEEK_STATES,
            });
        }
    }

    fn seek_command(&self) -> Option<MediaCommand> {
        let scrub = self.scrub?;
        let session = self.session.as_ref()?;
        let length = session.length_micros?;
        if !self.control || !session.can_seek {
            return None;
        }
        Some(MediaCommand {
            player: session.player.clone(),
            action: MediaAction::SetPosition {
                track_id: session.track_id.clone(),
                position_micros: (f64::from(scrub) * length as f64) as u64,
            },
        })
    }

    /// The position to show: the scrub in progress, a seek the player has not
    /// confirmed yet, or the reported position plus the time since it came.
    fn position(&self, session: &MediaSession) -> Option<u64> {
        if let (Some(scrub), Some(length)) = (self.scrub, session.length_micros) {
            return Some((f64::from(scrub) * length as f64) as u64);
        }
        if let Some(pending) = self.pending_seek {
            return Some(pending.position_micros);
        }
        let position = session.position_micros?;
        let elapsed = match (session.status, self.received) {
            (MediaPlaybackStatus::Playing, Some(received)) => {
                u64::try_from(received.elapsed().as_micros()).unwrap_or(0)
            }
            _ => 0,
        };
        let position = position.saturating_add(elapsed);
        Some(
            session
                .length_micros
                .map_or(position, |length| position.min(length)),
        )
    }

    fn status_message(&self) -> &'static str {
        match self.read {
            Access::Waiting => "",
            Access::Off => "Media access is off",
            Access::On => "Nothing playing",
        }
    }
}

fn can_toggle(session: &MediaSession) -> bool {
    if session.status == MediaPlaybackStatus::Playing {
        session.can_pause
    } else {
        session.can_play
    }
}

fn toggle_glyph(playing: bool) -> &'static str {
    if playing { "pause" } else { "play" }
}

fn toggle_label(playing: bool) -> &'static str {
    if playing { "Pause" } else { "Play" }
}

/// The title, or the player's name for media without one.
fn headline(session: &MediaSession) -> &str {
    if session.title.is_empty() {
        &session.identity
    } else {
        &session.title
    }
}

fn fraction(position: u64, length: u64) -> f32 {
    (position as f64 / length as f64).clamp(0.0, 1.0) as f32
}

fn clock(micros: u64) -> String {
    let seconds = micros / 1_000_000;
    let (hours, minutes, seconds) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// Remaining time rounds up, so that elapsed plus remaining is the length.
fn clock_rounded_up(micros: u64) -> String {
    clock(micros.div_ceil(1_000_000) * 1_000_000)
}

touchbar_logic_sdk::export!(Plugin);

#[cfg(test)]
mod tests {
    use super::*;

    fn session(status: MediaPlaybackStatus, position: Option<u64>) -> MediaSession {
        MediaSession {
            player: "org.mpris.MediaPlayer2.vlc".into(),
            identity: "VLC media player".into(),
            status,
            track_id: "/org/videolan/vlc/playlist/3".into(),
            title: "Song".into(),
            artist: "One, Two".into(),
            album: String::new(),
            length_micros: Some(200_000_000),
            position_micros: position,
            can_play: true,
            can_pause: true,
            can_go_next: true,
            can_go_previous: false,
            can_seek: true,
        }
    }

    fn state(generation: u64, session: Option<MediaSession>) -> MediaState {
        MediaState {
            generation,
            session,
        }
    }

    #[test]
    fn clocks_read_like_the_macos_scrubber() {
        assert_eq!(clock(0), "0:00");
        assert_eq!(clock(61_900_000), "1:01");
        assert_eq!(clock(3_725_000_000), "1:02:05");
        assert_eq!(clock_rounded_up(138_500_000), "2:19");
        assert_eq!(clock_rounded_up(138_000_000), "2:18");
    }

    #[test]
    fn a_scrub_shows_its_own_time_until_the_player_confirms_the_seek() {
        let mut media = Media {
            control: true,
            ..Media::default()
        };
        media.apply(state(
            1,
            Some(session(MediaPlaybackStatus::Paused, Some(10_000_000))),
        ));
        let current = media.session.clone().unwrap();
        assert_eq!(media.position(&current), Some(10_000_000));

        media.scrub = Some(0.5);
        assert_eq!(media.position(&current), Some(100_000_000));

        // The seek itself goes through the broker; model its result.
        media.scrub = None;
        media.pending_seek = Some(PendingSeek {
            request: 7,
            position_micros: 100_000_000,
            states_left: SEEK_STATES,
        });
        media.apply(state(
            2,
            Some(session(MediaPlaybackStatus::Paused, Some(10_000_000))),
        ));
        assert_eq!(
            media.position(media.session.as_ref().unwrap()),
            Some(100_000_000),
            "a state from before the seek does not pull the scrubber back"
        );
        media.apply(state(
            3,
            Some(session(MediaPlaybackStatus::Paused, Some(100_400_000))),
        ));
        assert!(media.pending_seek.is_none());
        assert_eq!(
            media.position(media.session.as_ref().unwrap()),
            Some(100_400_000)
        );
    }

    #[test]
    fn a_refused_seek_shows_the_real_position_again() {
        use touchbar_component_sdk::touchbar::plugin::broker::ErrorCode;

        let mut media = Media {
            control: true,
            ..Media::default()
        };
        media.apply(state(
            1,
            Some(session(MediaPlaybackStatus::Paused, Some(10_000_000))),
        ));
        media.pending_seek = Some(PendingSeek {
            request: 7,
            position_micros: 100_000_000,
            states_left: SEEK_STATES,
        });
        let refused = |request| {
            HostEvent::Completion((request, OperationResult::Error(ErrorCode::InvalidRequest)))
        };
        assert!(!media.host_event(refused(8)), "another request's failure");
        assert!(media.pending_seek.is_some());
        assert!(media.host_event(refused(7)));
        assert_eq!(
            media.position(media.session.as_ref().unwrap()),
            Some(10_000_000)
        );
    }

    #[test]
    fn another_track_drops_a_scrub_in_progress() {
        let mut media = Media::default();
        media.apply(state(
            1,
            Some(session(MediaPlaybackStatus::Playing, Some(0))),
        ));
        media.scrub = Some(0.9);
        let mut next = session(MediaPlaybackStatus::Playing, Some(0));
        next.track_id = "/org/videolan/vlc/playlist/4".into();
        media.apply(state(2, Some(next)));
        assert_eq!(media.scrub, None);
    }

    #[test]
    fn every_session_emits_a_valid_typed_snapshot() {
        for control in [false, true] {
            for session in [
                None,
                Some(session(MediaPlaybackStatus::Playing, Some(42_000_000))),
                Some(session(MediaPlaybackStatus::Paused, None)),
            ] {
                let media = Media {
                    control,
                    session,
                    ..Media::default()
                };
                let mut snapshot = touchbar_set_config::StateSnapshot::default();
                snapshot
                    .apply(
                        &touchbar_logic_sdk::patch_from_wit(patch(&media)).unwrap(),
                        &description(),
                    )
                    .unwrap();
            }
        }
    }
    #[test]
    fn declarative_media_preserves_primary_items() {
        for name in ["now-playing", "transport", "timeline", "touchbar-logo"] {
            let path =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("sets/{name}.toml"));
            let set = touchbar_set_config::SetDocument::from_toml(
                &std::fs::read_to_string(path).unwrap(),
            )
            .unwrap();
            assert_eq!(set.id, name);
        }
    }
    #[test]
    fn cancel_and_lost_player_withdraw_scrub_and_seek() {
        let mut media = Media::default();
        media.apply(state(
            1,
            Some(session(MediaPlaybackStatus::Paused, Some(10_000_000))),
        ));
        media.scrub = Some(0.5);
        media.input(&InputEvent {
            item_id: "timeline".into(),
            widget_id: SCRUBBER,
            kind: InputKind::Cancelled,
            value: None,
            contact_id: Some(1),
        });
        assert_eq!(
            media.position(media.session.as_ref().unwrap()),
            Some(10_000_000)
        );
        media.scrub = Some(0.8);
        media.pending_seek = Some(PendingSeek {
            request: 7,
            position_micros: 100_000_000,
            states_left: 3,
        });
        media.apply(state(2, None));
        assert!(media.session.is_none() && media.scrub.is_none() && media.pending_seek.is_none());
    }

    #[test]
    fn seek_uses_original_track_and_track_change_cancels_commit() {
        let original = session(MediaPlaybackStatus::Paused, Some(0));
        let mut media = Media {
            control: true,
            ..Media::default()
        };
        media.apply(state(1, Some(original.clone())));
        media.scrub = Some(0.5);
        assert_eq!(
            media.seek_command(),
            Some(MediaCommand {
                player: original.player,
                action: MediaAction::SetPosition {
                    track_id: original.track_id,
                    position_micros: 100_000_000
                }
            })
        );
        let mut next = session(MediaPlaybackStatus::Paused, Some(0));
        next.track_id = "/new-track".into();
        media.apply(state(2, Some(next)));
        assert!(media.seek_command().is_none());
    }
}
