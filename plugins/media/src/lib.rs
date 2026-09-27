//! Media: the now-playing session of MPRIS players, as on macOS.
//!
//! `media.read.v1` names one session, the player that most recently started
//! playing, kept while it is paused. Its state arrives on every change and
//! once per second while a track plays. `media.control.v1` sends a command to
//! that player for a touch on one of these controls.

use std::{cell::RefCell, time::Instant};

use touchbar_component_sdk::kit::{
    ColorRole, ImageFit, ImageTint, InputEvent, InputKind, NodeId, Pressable, ViewBuilder, content,
    fixed, flexible,
};
use touchbar_component_sdk::touchbar::plugin::{
    broker::{CapabilityStatus, OperationResult},
    ui::{
        LabelNode, Node, PresentationBegin, PresentationCommand, PresentationLifecycle,
        PresentationPlacement, TextAlign,
    },
};
use touchbar_component_sdk::{
    Guest, HostEvent, Item, MediaAction, MediaCommand, MediaPlaybackStatus, MediaSession,
    MediaState, PresentationEvent, RenderRequest, Update, View, decode_media_state,
    decode_media_subscription_opened, media_command, media_subscribe,
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

#[derive(Default)]
struct Media {
    read: Access,
    control: bool,
    subscribe_request: Option<u64>,
    resource: Option<u64>,
    resubscribed: u8,
    session: Option<MediaSession>,
    received: Option<Instant>,
    /// The scrubber's value while a finger drags it.
    scrub: Option<f32>,
    pending_seek: Option<PendingSeek>,
    presenting: bool,
}

impl Guest for Plugin {
    fn items() -> Vec<Item> {
        vec![
            item("now-playing", "Now Playing"),
            item("transport", "Media Transport"),
            item("timeline", "Media Timeline"),
            item("touchbar-logo", "TouchBar Logo"),
        ]
    }

    fn render(request: RenderRequest) -> Result<View, String> {
        let width = request.viewport.width;
        let radius = request.theme.corner_radius;
        MEDIA.with_borrow(|media| {
            Ok(match request.item_id.as_str() {
                "now-playing" => media.now_playing(width),
                "transport" => media.transport(radius),
                "timeline" => media.timeline(width),
                "touchbar-logo" => touchbar_logo(),
                item => return Err(format!("unknown item {item}")),
            })
        })
    }

    fn handle_event(event: InputEvent) -> Result<Update, String> {
        Ok(MEDIA.with_borrow_mut(|media| media.input(&event)))
    }

    fn handle_presentation_event(event: PresentationEvent) -> Result<Update, String> {
        let rerender = MEDIA.with_borrow_mut(|media| {
            let presenting = !matches!(event, PresentationEvent::Ended(_));
            let changed = media.presenting != presenting;
            media.presenting = presenting;
            changed
        });
        Ok(Update {
            rerender,
            presentation: None,
        })
    }

    fn handle_host_event(event: HostEvent) -> Result<Update, String> {
        Ok(Update {
            rerender: MEDIA.with_borrow_mut(|media| media.host_event(event)),
            presentation: None,
        })
    }
}

impl Media {
    fn host_event(&mut self, event: HostEvent) -> bool {
        match event {
            HostEvent::CapabilityChanged((_, state)) if state.capability == READ => {
                if state.status == CapabilityStatus::Granted {
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
                self.forget();
                self.read = Access::Off;
                true
            }
            _ => false,
        }
    }

    fn subscribe(&mut self) {
        if self.resource.is_some() || self.subscribe_request.is_some() {
            return;
        }
        match media_subscribe() {
            Ok(request) => self.subscribe_request = Some(request.into_raw()),
            Err(_) => self.read = Access::Off,
        }
    }

    fn forget(&mut self) {
        self.resource = None;
        self.subscribe_request = None;
        self.session = None;
        self.scrub = None;
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
            (SCRUBBER, InputKind::ValueChanged) => {
                if let Some(value) = event.value.filter(|value| value.is_finite()) {
                    self.scrub = Some(value.clamp(0.0, 1.0));
                    update.rerender = true;
                }
            }
            // Lifting the finger commits the scrub.
            (SCRUBBER, InputKind::Activated) => {
                self.seek();
                update.rerender = true;
            }
            (SCRUBBER, InputKind::Cancelled) => {
                self.scrub = None;
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
            let _ = media_command(&MediaCommand {
                player: session.player.clone(),
                action,
            });
        }
    }

    fn seek(&mut self) {
        let Some(scrub) = self.scrub.take() else {
            return;
        };
        let Some(session) = &self.session else {
            return;
        };
        let Some(length) = session.length_micros else {
            return;
        };
        if !self.control || !session.can_seek {
            return;
        }
        let position_micros = (f64::from(scrub) * length as f64) as u64;
        let command = MediaCommand {
            player: session.player.clone(),
            action: MediaAction::SetPosition {
                track_id: session.track_id.clone(),
                position_micros,
            },
        };
        if let Ok(request) = media_command(&command) {
            self.pending_seek = Some(PendingSeek {
                request: request.into_raw(),
                position_micros,
                states_left: SEEK_STATES,
            });
        }
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

    fn now_playing(&self, width: f32) -> View {
        let mut b = ViewBuilder::new();
        let Some(session) = &self.session else {
            let note = b.image(
                "note",
                "Media",
                ImageFit::Contain,
                ImageTint::Mask(ColorRole::Muted),
            );
            let mut children = vec![fixed(note, 28.0)];
            if width >= 120.0 {
                let message = text(
                    &mut b,
                    self.status_message(),
                    11.0,
                    ColorRole::Muted,
                    TextAlign::Leading,
                );
                children.push(flexible(message, 40.0, 200.0, 1000.0));
            }
            let root = b.row(children, 8.0, 14.0);
            return b.finish(root);
        };
        let playing = session.status == MediaPlaybackStatus::Playing;
        let toggle = glyph_button(
            &mut b,
            COMPACT_TOGGLE,
            toggle_glyph(playing),
            toggle_label(playing),
            self.control && can_toggle(session),
            Pressable::plain(),
            None,
            10.0,
        );
        if width < 100.0 {
            return b.finish(toggle);
        }
        let title = text(
            &mut b,
            headline(session),
            12.0,
            ColorRole::Foreground,
            TextAlign::Leading,
        );
        let detail = match (self.position(session), session.length_micros) {
            (Some(position), Some(length)) if length > 0 => b.progress(
                "Track position",
                fraction(position, length),
                ColorRole::Accent,
            ),
            _ => text(
                &mut b,
                &session.artist,
                10.0,
                ColorRole::Muted,
                TextAlign::Leading,
            ),
        };
        let info = b.column(
            vec![fixed(title, 20.0), flexible(detail, 4.0, 12.0, 16.0)],
            4.0,
            6.0,
        );
        let row = b.row(
            vec![fixed(toggle, 44.0), flexible(info, 40.0, 240.0, 1200.0)],
            4.0,
            2.0,
        );
        // Outside the expanded bar, touching the metadata opens it.
        let root = if self.presenting {
            row
        } else {
            b.pressable(EXPAND, "Show media controls", row, Pressable::plain())
        };
        b.finish(root)
    }

    fn transport(&self, radius: f32) -> View {
        let mut b = ViewBuilder::new();
        let session = self.session.as_ref();
        let playing = session.is_some_and(|session| session.status == MediaPlaybackStatus::Playing);
        let control =
            |allowed: fn(&MediaSession) -> bool| self.control && session.is_some_and(allowed);
        let previous = glyph_button(
            &mut b,
            PREVIOUS,
            "backward",
            "Previous track",
            control(|session| session.can_go_previous),
            Pressable::control(),
            Some(radius),
            12.0,
        );
        let toggle = glyph_button(
            &mut b,
            TOGGLE,
            toggle_glyph(playing),
            toggle_label(playing),
            control(can_toggle),
            Pressable::control(),
            Some(radius),
            12.0,
        );
        let next = glyph_button(
            &mut b,
            NEXT,
            "forward",
            "Next track",
            control(|session| session.can_go_next),
            Pressable::control(),
            Some(radius),
            12.0,
        );
        let root = b.row(
            vec![
                flexible(previous, 40.0, 88.0, 160.0),
                flexible(toggle, 44.0, 88.0, 160.0),
                flexible(next, 40.0, 88.0, 160.0),
            ],
            6.0,
            2.0,
        );
        b.finish(root)
    }

    fn timeline(&self, width: f32) -> View {
        let mut b = ViewBuilder::new();
        let Some(session) = &self.session else {
            let message = text(
                &mut b,
                self.status_message(),
                12.0,
                ColorRole::Muted,
                TextAlign::Center,
            );
            return b.finish(message);
        };
        let (Some(position), Some(length)) = (
            self.position(session),
            session.length_micros.filter(|length| *length > 0),
        ) else {
            // Without a length there is nothing to scrub: a live stream, or a
            // page that does not report its position.
            let detail = match (session.title.is_empty(), session.artist.is_empty()) {
                (false, false) => session.artist.as_str(),
                (false, true) => session.identity.as_str(),
                (true, _) => "",
            };
            let title = text(
                &mut b,
                headline(session),
                14.0,
                ColorRole::Foreground,
                TextAlign::Leading,
            );
            let detail = text(&mut b, detail, 11.0, ColorRole::Muted, TextAlign::Leading);
            let root = b.column(vec![fixed(title, 24.0), fixed(detail, 18.0)], 2.0, 6.0);
            return b.finish(root);
        };
        // "Title — Artist": the artist follows the title and gives way first.
        let title = text(
            &mut b,
            headline(session),
            13.0,
            ColorRole::Foreground,
            TextAlign::Leading,
        );
        let heading = if width >= 320.0 && !session.artist.is_empty() && !session.title.is_empty() {
            let artist = text(
                &mut b,
                format!("— {}", session.artist),
                13.0,
                ColorRole::Muted,
                TextAlign::Leading,
            );
            b.row(
                vec![
                    content(title, 40.0, width * 0.7),
                    flexible(artist, 0.0, 200.0, 3000.0),
                ],
                6.0,
                0.0,
            )
        } else {
            b.row(vec![flexible(title, 40.0, 400.0, 3000.0)], 0.0, 0.0)
        };
        let position = position.min(length);
        let elapsed = text(
            &mut b,
            clock(position),
            11.0,
            ColorRole::Muted,
            TextAlign::Trailing,
        );
        let remaining = text(
            &mut b,
            format!("-{}", clock_rounded_up(length - position)),
            11.0,
            ColorRole::Muted,
            TextAlign::Leading,
        );
        let value = fraction(position, length);
        let track = if self.control && session.can_seek {
            b.slider(SCRUBBER, "Track position", value)
        } else {
            b.progress("Track position", value, ColorRole::Accent)
        };
        let times = b.row(
            vec![
                fixed(elapsed, 50.0),
                flexible(track, 40.0, 600.0, 3000.0),
                fixed(remaining, 56.0),
            ],
            8.0,
            0.0,
        );
        let root = b.column(
            vec![fixed(heading, 20.0), flexible(times, 20.0, 26.0, 36.0)],
            2.0,
            6.0,
        );
        b.finish(root)
    }
}

fn item(id: &str, label: &str) -> Item {
    Item {
        id: id.into(),
        label: label.into(),
    }
}

fn text(
    b: &mut ViewBuilder,
    value: impl Into<String>,
    size: f32,
    color: ColorRole,
    align: TextAlign,
) -> NodeId {
    b.push(Node::Label(LabelNode {
        text: value.into(),
        size,
        color,
        align,
    }))
}

/// A glyph on a button. A control the player or the grant does not allow
/// keeps its place, dimmed, and takes no touches.
#[allow(clippy::too_many_arguments)]
fn glyph_button(
    b: &mut ViewBuilder,
    id: u64,
    glyph: &str,
    label: &str,
    enabled: bool,
    style: Pressable,
    radius: Option<f32>,
    padding: f32,
) -> NodeId {
    let tint = if enabled {
        ColorRole::Foreground
    } else {
        ColorRole::Muted
    };
    let image = b.image(glyph, label, ImageFit::Contain, ImageTint::Mask(tint));
    if enabled {
        return b.pressable(id, label, image, Pressable { padding, ..style });
    }
    match (style.background, radius) {
        (Some(background), Some(radius)) => b.panel(image, background, radius, padding),
        _ => b.row(vec![flexible(image, 16.0, 24.0, 1000.0)], 0.0, padding),
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

fn touchbar_logo() -> View {
    let mut b = ViewBuilder::new();
    let logo = b.image(
        "touchbar-wordmark",
        "TouchBar",
        ImageFit::Contain,
        ImageTint::Mask(ColorRole::Accent),
    );
    let root = b.row(vec![flexible(logo, 80.0, 171.0, 1004.0)], 0.0, 8.0);
    b.finish(root)
}

touchbar_component_sdk::export!(Plugin);

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
    fn every_item_renders_at_every_width_in_every_state() {
        let sessions = [
            None,
            Some(session(MediaPlaybackStatus::Playing, Some(42_000_000))),
            Some(session(MediaPlaybackStatus::Paused, None)),
            Some(MediaSession {
                title: String::new(),
                artist: String::new(),
                length_micros: None,
                ..session(MediaPlaybackStatus::Playing, Some(5_000_000))
            }),
        ];
        for read in [Access::Waiting, Access::On, Access::Off] {
            for session in &sessions {
                for control in [false, true] {
                    let media = Media {
                        read,
                        control,
                        session: session.clone(),
                        received: Some(Instant::now()),
                        ..Media::default()
                    };
                    for width in [80.0, 160.0, 320.0, 1004.0, 2008.0] {
                        for view in [
                            media.now_playing(width),
                            media.transport(9.0),
                            media.timeline(width),
                        ] {
                            assert!((view.root as usize) < view.nodes.len());
                        }
                    }
                }
            }
        }
    }
}
