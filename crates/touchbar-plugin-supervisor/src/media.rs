//! `media.read.v1` and `media.control.v1`: the now-playing media session.
//!
//! The session is the MPRIS player that most recently started playing, as on
//! macOS; `touchbar-mpris` decides it and the session daemon's profile facts
//! use the same rule. A supervisor started after two players began cannot
//! know their order, so where the session daemon records its own choice the
//! supervisor follows it, and the bar controls the player the facts name. A subscription receives the whole state whenever it
//! changes, and while a track plays once per second of the track, so that a
//! component can show the elapsed time without a timer of its own.

use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, Weak},
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use touchbar_broker_schema::{
    MAX_MEDIA_TEXT_BYTES, MediaAction, MediaCommand, MediaPlaybackStatus, MediaReadRequest,
    MediaSession, MediaState, MediaSubscriptionOpened, SchemaError,
};
use touchbar_mpris::{
    Command, CommandError, MprisWatcher, NowPlaying, PlaybackStatus, is_player_bus_name,
    session_file,
};
use touchbar_policy::{CapabilityId, CapabilityScope, MediaControlAction};
use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult};

use crate::{
    ActivationExpectation, ActivationLedger, Backend, BackendRequest, CancellationToken,
    GrantStoreWatcher, OpenedResource, ResourceBackend, ResourceEventSink, ResourceHandle,
    ResourceLimits,
};

pub const MEDIA_SUBSCRIBE_OPERATION: &str = "subscribe";
pub const MEDIA_COMMAND_OPERATION: &str = "command";
const MEDIA_RESERVED_BYTES: usize = 16 * 1024;
/// While a track plays its position is read back this often, to correct the
/// estimate between the player's own reports.
const POSITION_RESYNC: Duration = Duration::from_secs(5);
/// A state held back by the subscription's rate limit is sent again after
/// this long.
const RATE_LIMIT_RETRY: Duration = Duration::from_millis(300);
/// Ticks land this long after the second changes, so the new second shows.
const TICK_MARGIN: Duration = Duration::from_millis(10);
/// How often the follower of the session daemon's choice checks for a stop.
const FOLLOW_POLL: Duration = Duration::from_secs(1);

/// The player boundary. Production uses [`MprisWatcher`] on the session bus.
pub trait MediaSource: Send + Sync + 'static {
    fn now_playing(&self) -> Option<NowPlaying>;
    /// Follows another process's choice of session while it is current.
    fn prefer(&self, bus_name: Option<String>);
    fn refresh_position(&self, bus_name: &str);
    fn command(&self, bus_name: &str, command: &Command) -> Result<(), CommandError>;
}

impl MediaSource for MprisWatcher {
    fn now_playing(&self) -> Option<NowPlaying> {
        MprisWatcher::now_playing(self)
    }

    fn prefer(&self, bus_name: Option<String>) {
        MprisWatcher::prefer(self, bus_name);
    }

    fn refresh_position(&self, bus_name: &str) {
        MprisWatcher::refresh_position(self, bus_name);
    }

    fn command(&self, bus_name: &str, command: &Command) -> Result<(), CommandError> {
        MprisWatcher::command(self, bus_name, command)
    }
}

type Notify = Box<dyn Fn() + Send>;
type Starter = dyn Fn(Notify) -> Result<Arc<dyn MediaSource>, BrokerErrorCode> + Send + Sync;

/// Serves both media capabilities from one player source, started on first
/// use so that packages without a grant never touch the session bus.
pub struct MediaBackend {
    shared: Arc<Shared>,
    starter: Box<Starter>,
    worker: Mutex<Option<JoinHandle<()>>>,
    /// The session daemon's record of its now-playing choice, if any.
    session_record: Option<PathBuf>,
}

struct Shared {
    state: Mutex<State>,
    wake: Condvar,
}

#[derive(Default)]
struct State {
    source: Option<Arc<dyn MediaSource>>,
    subscriptions: BTreeMap<u64, Subscription>,
    generation: u64,
    changed: bool,
    stop: bool,
}

struct Subscription {
    events: ResourceEventSink,
    /// The session last delivered, to skip states that look the same.
    sent: Option<MediaSession>,
}

impl MediaBackend {
    /// `session_record` is the session daemon's now-playing record; without
    /// one the supervisor ranks the players on its own.
    pub fn new(session_record: Option<PathBuf>) -> Self {
        let mut backend = Self::with_starter(|notify| {
            MprisWatcher::session(notify)
                .map(|watcher| Arc::new(watcher) as Arc<dyn MediaSource>)
                .map_err(|_| BrokerErrorCode::Unavailable)
        });
        backend.session_record = session_record;
        backend
    }

    fn with_starter(
        starter: impl Fn(Notify) -> Result<Arc<dyn MediaSource>, BrokerErrorCode>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                wake: Condvar::new(),
            }),
            starter: Box::new(starter),
            worker: Mutex::new(None),
            session_record: None,
        }
    }

    fn source(&self) -> Result<Arc<dyn MediaSource>, BrokerErrorCode> {
        let mut state = lock(&self.shared.state);
        if let Some(source) = &state.source {
            return Ok(Arc::clone(source));
        }
        let weak = Arc::downgrade(&self.shared);
        let source = (self.starter)(Box::new(move || notify(&weak)))?;
        // The source is kept only once its worker runs: without one, every
        // later subscription would open and then never hear of a change.
        let shared = Arc::clone(&self.shared);
        let worker = thread::Builder::new()
            .name("touchbar-media".into())
            .spawn(move || run(&shared))
            .map_err(|_| BrokerErrorCode::Unavailable)?;
        *self.worker.lock().unwrap_or_else(PoisonError::into_inner) = Some(worker);
        if let Some(path) = &self.session_record {
            // The first state already follows the record; later changes
            // arrive through the follower.
            let preferred = session_file::read(path);
            source.prefer(preferred.clone());
            let shared = Arc::downgrade(&self.shared);
            let path = path.clone();
            // Without the follower the supervisor still ranks on its own.
            let _ = thread::Builder::new()
                .name("touchbar-media-session".into())
                .spawn(move || follow_session_record(&path, &shared, preferred));
        }
        state.source = Some(Arc::clone(&source));
        Ok(source)
    }
}

/// Applies every change of the session daemon's record until the backend
/// stops, or the record's directory goes away with the session daemon.
fn follow_session_record(path: &Path, shared: &Weak<Shared>, mut applied: Option<String>) {
    let Ok(mut watcher) = GrantStoreWatcher::new(path) else {
        return;
    };
    loop {
        let preferred = session_file::read(path);
        let Some(strong) = shared.upgrade() else {
            return;
        };
        let source = {
            let state = lock(&strong.state);
            if state.stop {
                return;
            }
            state.source.clone()
        };
        if preferred != applied
            && let Some(source) = source
        {
            source.prefer(preferred.clone());
            applied = preferred;
            notify(shared);
        }
        drop(strong);
        if watcher.wait_for_change(Some(FOLLOW_POLL)).is_err() {
            return;
        }
    }
}

impl Default for MediaBackend {
    fn default() -> Self {
        Self::new(None)
    }
}

impl Drop for MediaBackend {
    fn drop(&mut self) {
        lock(&self.shared.state).stop = true;
        self.shared.wake.notify_all();
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = worker.join();
        }
    }
}

fn notify(shared: &Weak<Shared>) {
    if let Some(shared) = shared.upgrade() {
        lock(&shared.state).changed = true;
        shared.wake.notify_all();
    }
}

fn lock(state: &Mutex<State>) -> MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Delivers changes, and the passing seconds of a playing track.
fn run(shared: &Shared) {
    let mut wait: Option<Duration> = None;
    let mut resynced = Instant::now();
    loop {
        let source = {
            let mut state = lock(&shared.state);
            let deadline = wait.map(|wait| Instant::now() + wait);
            while !state.changed && !state.stop {
                let Some(deadline) = deadline else {
                    state = shared
                        .wake
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                    continue;
                };
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                state = shared
                    .wake
                    .wait_timeout(state, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
            if state.stop {
                return;
            }
            state.changed = false;
            // Nobody listens: sleep until a subscription opens and wakes this.
            if state.subscriptions.is_empty() {
                wait = None;
                continue;
            }
            match &state.source {
                Some(source) => Arc::clone(source),
                None => continue,
            }
        };
        let mut playing = source.now_playing();
        if let Some(bus_name) = playing
            .as_ref()
            .filter(|playing| playing.status == PlaybackStatus::Playing)
            .map(|playing| playing.bus_name.clone())
            && resynced.elapsed() >= POSITION_RESYNC
        {
            source.refresh_position(&bus_name);
            resynced = Instant::now();
            playing = source.now_playing();
        }
        let held_back = deliver(&mut lock(&shared.state), playing.as_ref().map(session));
        wait = match (playing.as_ref().and_then(next_second), held_back) {
            (Some(tick), true) => Some(tick.min(RATE_LIMIT_RETRY)),
            (Some(tick), false) => Some(tick),
            (None, true) => Some(RATE_LIMIT_RETRY),
            (None, false) => None,
        };
    }
}

/// Sends the session to every subscription that has not seen it. Returns
/// true if a rate limit held one back.
fn deliver(state: &mut State, session: Option<MediaSession>) -> bool {
    let stale = state
        .subscriptions
        .values()
        .any(|subscription| !looks_the_same(subscription.sent.as_ref(), session.as_ref()));
    if !stale {
        return false;
    }
    state.generation += 1;
    let Ok(payload) = (MediaState {
        generation: state.generation,
        session: session.clone(),
    })
    .encode() else {
        return false;
    };
    let mut held_back = false;
    state.subscriptions.retain(|_, subscription| {
        if subscription.events.is_finished() {
            return false;
        }
        if looks_the_same(subscription.sent.as_ref(), session.as_ref()) {
            return true;
        }
        match subscription.events.emit(payload.clone()) {
            Ok(()) => {
                subscription.sent = session.clone();
                true
            }
            Err(BrokerErrorCode::RateLimited) => {
                held_back = true;
                true
            }
            Err(_) => false,
        }
    });
    held_back
}

/// Equal apart from the fraction of a second of the position, which only
/// changes because time passes.
fn looks_the_same(left: Option<&MediaSession>, right: Option<&MediaSession>) -> bool {
    let seconds = |session: &MediaSession| session.position_micros.map(|micros| micros / 1_000_000);
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            seconds(left) == seconds(right)
                && MediaSession {
                    position_micros: None,
                    ..left.clone()
                } == MediaSession {
                    position_micros: None,
                    ..right.clone()
                }
        }
        _ => false,
    }
}

/// How long until the playing track's position reaches its next second.
fn next_second(playing: &NowPlaying) -> Option<Duration> {
    if playing.status != PlaybackStatus::Playing || playing.rate <= 0.0 {
        return None;
    }
    let position = playing.position?;
    if playing
        .metadata
        .length
        .is_some_and(|length| position >= length)
    {
        return None;
    }
    let left = Duration::from_micros(u64::from(1_000_000 - position.subsec_micros()));
    Some((left.div_f64(playing.rate) + TICK_MARGIN).min(Duration::from_secs(1) + TICK_MARGIN))
}

fn session(playing: &NowPlaying) -> MediaSession {
    let capabilities = playing.capabilities;
    let control = capabilities.can_control;
    MediaSession {
        player: playing.bus_name.clone(),
        identity: bounded(
            playing
                .identity
                .clone()
                .unwrap_or_else(|| playing.player_name().to_owned()),
        ),
        status: match playing.status {
            PlaybackStatus::Playing => MediaPlaybackStatus::Playing,
            PlaybackStatus::Paused => MediaPlaybackStatus::Paused,
            PlaybackStatus::Stopped => MediaPlaybackStatus::Stopped,
        },
        track_id: bounded(playing.metadata.track_id.clone().unwrap_or_default()),
        title: bounded(playing.metadata.title.clone().unwrap_or_default()),
        artist: bounded(playing.metadata.artists.join(", ")),
        album: bounded(playing.metadata.album.clone().unwrap_or_default()),
        length_micros: playing.metadata.length.map(micros),
        position_micros: playing.position.map(micros),
        can_play: control && capabilities.can_play,
        can_pause: control && capabilities.can_pause,
        can_go_next: control && capabilities.can_go_next,
        can_go_previous: control && capabilities.can_go_previous,
        can_seek: control && capabilities.can_seek,
    }
}

fn micros(duration: Duration) -> u64 {
    u64::try_from(duration.as_micros()).unwrap_or(u64::MAX)
}

fn bounded(mut text: String) -> String {
    if text.len() > MAX_MEDIA_TEXT_BYTES {
        let mut end = MAX_MEDIA_TEXT_BYTES;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    text
}

fn schema_error(error: SchemaError) -> BrokerErrorCode {
    match error {
        SchemaError::Malformed => BrokerErrorCode::InvalidRequest,
        SchemaError::LimitExceeded => BrokerErrorCode::QuotaExceeded,
    }
}

fn decode_subscription(request: &BackendRequest) -> Result<(), BrokerErrorCode> {
    if request.capability != CapabilityId::MediaReadV1
        || request.operation != MEDIA_SUBSCRIBE_OPERATION
        || !matches!(request.authorized_scope, CapabilityScope::MediaRead(_))
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    MediaReadRequest::decode(&request.payload)
        .map(drop)
        .map_err(schema_error)
}

fn decode_command(request: &BackendRequest) -> Result<(String, Command), BrokerErrorCode> {
    let CapabilityScope::MediaControl(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    if request.capability != CapabilityId::MediaControlV1
        || request.operation != MEDIA_COMMAND_OPERATION
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let command = MediaCommand::decode(&request.payload).map_err(schema_error)?;
    if !is_player_bus_name(&command.player) {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let (action, command_value) = match command.action {
        MediaAction::PlayPause => (MediaControlAction::PlayPause, Command::PlayPause),
        MediaAction::Play => (MediaControlAction::Play, Command::Play),
        MediaAction::Pause => (MediaControlAction::Pause, Command::Pause),
        MediaAction::Next => (MediaControlAction::Next, Command::Next),
        MediaAction::Previous => (MediaControlAction::Previous, Command::Previous),
        MediaAction::SetPosition {
            track_id,
            position_micros,
        } => (
            MediaControlAction::Seek,
            Command::SetPosition {
                track_id,
                position: Duration::from_micros(position_micros),
            },
        ),
    };
    if !scope.actions.contains(&action) {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok((command.player, command_value))
}

impl Backend for MediaBackend {
    /// Every command needs a fresh touch on the component's own control.
    fn authorize(
        &self,
        request: &BackendRequest,
        activations: &mut ActivationLedger,
        now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        decode_command(request)?;
        let activation = request
            .activation
            .as_ref()
            .ok_or(BrokerErrorCode::ActivationRequired)?;
        activations.consume(
            Some(activation),
            ActivationExpectation {
                surface_instance: activation.surface_instance,
                item_id: &activation.item_id,
                widget_id: activation.widget_id,
                now_monotonic_micros,
            },
        )?;
        Ok(())
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult {
        let result = (|| {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            let (player, command) = decode_command(request)?;
            self.source()?
                .command(&player, &command)
                .map_err(|error| match error {
                    CommandError::UnknownPlayer => BrokerErrorCode::Unavailable,
                    CommandError::Unsupported => BrokerErrorCode::Unsupported,
                    CommandError::StaleTrack => BrokerErrorCode::InvalidRequest,
                    CommandError::Failed(_) => BrokerErrorCode::BackendFailed,
                })?;
            Ok(BrokerResult::Success {
                payload: Vec::new(),
            })
        })();
        result.unwrap_or_else(BrokerResult::Error)
    }
}

impl ResourceBackend for MediaBackend {
    fn limits(&self, request: &BackendRequest) -> Option<ResourceLimits> {
        let CapabilityScope::MediaRead(scope) = &request.authorized_scope else {
            return None;
        };
        (request.capability == CapabilityId::MediaReadV1
            && request.operation == MEDIA_SUBSCRIBE_OPERATION)
            .then_some(ResourceLimits {
                reserved_buffered_bytes: MEDIA_RESERVED_BYTES,
                maximum_events_per_second: scope.maximum_updates_per_second,
            })
    }

    fn authorize(
        &self,
        request: &BackendRequest,
        _activations: &mut ActivationLedger,
        _now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        decode_subscription(request)
    }

    fn open(
        &self,
        resource_id: u64,
        request: &BackendRequest,
        events: ResourceEventSink,
    ) -> Result<OpenedResource, BrokerErrorCode> {
        decode_subscription(request)?;
        let source = self.source()?;
        let session = source.now_playing().as_ref().map(session);
        let mut state = lock(&self.shared.state);
        if state.subscriptions.contains_key(&resource_id) {
            return Err(BrokerErrorCode::InvalidRequest);
        }
        state.generation += 1;
        let response_payload = MediaSubscriptionOpened {
            resource_id,
            state: MediaState {
                generation: state.generation,
                session: session.clone(),
            },
        }
        .encode()
        .map_err(schema_error)?;
        state.subscriptions.insert(
            resource_id,
            Subscription {
                events,
                sent: session,
            },
        );
        // Let the worker schedule the seconds of a track already playing.
        state.changed = true;
        drop(state);
        self.shared.wake.notify_all();
        Ok(OpenedResource {
            handle: Box::new(MediaSubscriptionHandle {
                resource_id,
                shared: Arc::downgrade(&self.shared),
            }),
            response_payload,
        })
    }
}

struct MediaSubscriptionHandle {
    resource_id: u64,
    shared: Weak<Shared>,
}

impl ResourceHandle for MediaSubscriptionHandle {
    fn close(&mut self) {
        if let Some(shared) = self.shared.upgrade() {
            lock(&shared.state).subscriptions.remove(&self.resource_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use semver::Version;
    use touchbar_mpris::{Capabilities, Metadata};
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        GrantBindings, MediaControlScope, MediaReadScope, PackageInstance, Provenance, RuntimeKind,
    };
    use touchbar_protocol::broker_ipc::{ActivationContext, ActivationOrigin};

    use super::*;
    use crate::{ConnectionIdentity, HostEvent, HostEventQueue, ResourceManager};

    const VLC: &str = "org.mpris.MediaPlayer2.vlc";

    #[derive(Default)]
    struct FakeSource {
        playing: Mutex<Option<NowPlaying>>,
        commands: Mutex<Vec<(String, Command)>>,
        notify: Mutex<Option<Notify>>,
        preferred: Mutex<Vec<Option<String>>>,
    }

    impl FakeSource {
        fn set(&self, playing: Option<NowPlaying>) {
            *self.playing.lock().unwrap() = playing;
            if let Some(notify) = self.notify.lock().unwrap().as_ref() {
                notify();
            }
        }
    }

    impl MediaSource for FakeSource {
        fn now_playing(&self) -> Option<NowPlaying> {
            self.playing.lock().unwrap().clone()
        }

        fn prefer(&self, bus_name: Option<String>) {
            self.preferred.lock().unwrap().push(bus_name);
        }

        fn refresh_position(&self, _bus_name: &str) {}

        fn command(&self, bus_name: &str, command: &Command) -> Result<(), CommandError> {
            if bus_name != VLC {
                return Err(CommandError::UnknownPlayer);
            }
            self.commands
                .lock()
                .unwrap()
                .push((bus_name.into(), command.clone()));
            Ok(())
        }
    }

    fn backend(source: &Arc<FakeSource>) -> Arc<MediaBackend> {
        let source = Arc::clone(source);
        Arc::new(MediaBackend::with_starter(move |notify| {
            *source.notify.lock().unwrap() = Some(notify);
            Ok(Arc::clone(&source) as Arc<dyn MediaSource>)
        }))
    }

    fn playing(status: PlaybackStatus, position: Option<Duration>) -> NowPlaying {
        NowPlaying {
            bus_name: VLC.into(),
            identity: Some("VLC media player".into()),
            desktop_entry: Some("vlc".into()),
            status,
            metadata: Metadata {
                track_id: Some("/org/videolan/vlc/playlist/3".into()),
                title: Some("Song".into()),
                artists: vec!["One".into(), "Two".into()],
                album: None,
                length: Some(Duration::from_secs(200)),
            },
            capabilities: Capabilities {
                can_control: true,
                can_play: true,
                can_pause: true,
                can_go_next: true,
                can_go_previous: false,
                can_seek: true,
            },
            rate: 1.0,
            position,
        }
    }

    fn request(
        capability: CapabilityId,
        scope: CapabilityScope,
        operation: &str,
        payload: Vec<u8>,
        activation: Option<ActivationContext>,
    ) -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 5,
                package: PackageInstance {
                    source: GithubSource::new("alice", "media-test").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: 1,
            capability,
            authorized_scope: scope,
            bindings: GrantBindings::default(),
            activation,
            operation: operation.into(),
            payload,
        }
    }

    fn subscribe_request(rate: u16) -> BackendRequest {
        request(
            CapabilityId::MediaReadV1,
            CapabilityScope::MediaRead(MediaReadScope {
                maximum_updates_per_second: rate,
            }),
            MEDIA_SUBSCRIBE_OPERATION,
            MediaReadRequest.encode().unwrap(),
            None,
        )
    }

    fn command_request(
        action: MediaAction,
        allowed: &[MediaControlAction],
        activation: Option<ActivationContext>,
    ) -> BackendRequest {
        request(
            CapabilityId::MediaControlV1,
            CapabilityScope::MediaControl(MediaControlScope {
                actions: allowed.iter().copied().collect(),
            }),
            MEDIA_COMMAND_OPERATION,
            MediaCommand {
                player: VLC.into(),
                action,
            }
            .encode()
            .unwrap(),
            activation,
        )
    }

    fn activation(sequence: u64) -> ActivationContext {
        ActivationContext {
            origin: ActivationOrigin::Physical,
            surface_instance: 4,
            item_id: "transport".into(),
            widget_id: 2,
            input_sequence: sequence,
            deadline_monotonic_micros: u64::MAX,
        }
    }

    fn token() -> CancellationToken {
        CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(1)).unwrap()
    }

    fn next_state(resources: &mut ResourceManager, resource_id: u64) -> MediaState {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let mut events = HostEventQueue::new(4, 8);
            resources.pump(&mut events, 1);
            if let Some(HostEvent::ResourceEvent {
                resource_id: id,
                result: BrokerResult::Success { payload },
                ..
            }) = events.pop()
            {
                assert_eq!(id, resource_id);
                return MediaState::decode(&payload).unwrap();
            }
            assert!(Instant::now() < deadline, "no media state arrived");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn the_session_daemons_choice_is_followed_from_the_start_and_on_change() {
        const FIREFOX: &str = "org.mpris.MediaPlayer2.firefox.instance_1_7";
        let directory = tempfile::tempdir().unwrap();
        let record = directory.path().join("media-session");
        session_file::write(&record, Some(FIREFOX)).unwrap();
        let source = Arc::new(FakeSource::default());
        let fake = Arc::clone(&source);
        let mut backend = MediaBackend::with_starter(move |notify| {
            *fake.notify.lock().unwrap() = Some(notify);
            Ok(Arc::clone(&fake) as Arc<dyn MediaSource>)
        });
        backend.session_record = Some(record.clone());
        backend.source().unwrap();
        let last = || source.preferred.lock().unwrap().last().cloned().flatten();
        assert_eq!(last().as_deref(), Some(FIREFOX), "before the first state");

        let wait_for = |expected: Option<&str>| {
            let deadline = Instant::now() + Duration::from_secs(3);
            while last().as_deref() != expected {
                assert!(Instant::now() < deadline, "{expected:?} was not followed");
                thread::sleep(Duration::from_millis(10));
            }
        };
        session_file::write(&record, Some(VLC)).unwrap();
        wait_for(Some(VLC));
        session_file::write(&record, None).unwrap();
        wait_for(None);
    }

    #[test]
    fn subscription_opens_with_the_session_and_follows_changes() {
        let source = Arc::new(FakeSource::default());
        source.set(Some(playing(
            PlaybackStatus::Paused,
            Some(Duration::from_secs(61)),
        )));
        let backend = backend(&source);
        let request = subscribe_request(10);
        let mut resources = ResourceManager::new(8).unwrap();
        resources.register(CapabilityId::MediaReadV1, backend.clone());
        let limits = ResourceBackend::limits(backend.as_ref(), &request).unwrap();
        let opened = resources
            .open(7, &request, limits, &mut ActivationLedger::new(1), 0)
            .unwrap();
        let opened = MediaSubscriptionOpened::decode(&opened).unwrap();
        assert_eq!(opened.resource_id, 7);
        let session = opened.state.session.unwrap();
        assert_eq!(session.player, VLC);
        assert_eq!(session.identity, "VLC media player");
        assert_eq!(session.status, MediaPlaybackStatus::Paused);
        assert_eq!(session.artist, "One, Two");
        assert_eq!(session.position_micros, Some(61_000_000));
        assert!(session.can_seek && !session.can_go_previous);

        source.set(Some(playing(
            PlaybackStatus::Playing,
            Some(Duration::from_secs(61)),
        )));
        let state = next_state(&mut resources, 7);
        assert!(state.generation > opened.state.generation);
        assert_eq!(
            state.session.map(|session| session.status),
            Some(MediaPlaybackStatus::Playing)
        );

        source.set(None);
        assert_eq!(next_state(&mut resources, 7).session, None);
        assert!(resources.close(7));
        assert!(lock(&backend.shared.state).subscriptions.is_empty());
    }

    #[test]
    fn a_playing_track_ticks_once_per_second_of_position() {
        let source = Arc::new(FakeSource::default());
        let backend = backend(&source);
        let request = subscribe_request(10);
        let mut resources = ResourceManager::new(8).unwrap();
        resources.register(CapabilityId::MediaReadV1, backend.clone());
        let limits = ResourceBackend::limits(backend.as_ref(), &request).unwrap();
        resources
            .open(3, &request, limits, &mut ActivationLedger::new(1), 0)
            .unwrap();
        // The fake reports a later second every time it is asked, as a
        // playing track would.
        source.set(Some(playing(
            PlaybackStatus::Playing,
            Some(Duration::from_millis(10_950)),
        )));
        let first = next_state(&mut resources, 3);
        source.set(Some(playing(
            PlaybackStatus::Playing,
            Some(Duration::from_millis(11_010)),
        )));
        let second = next_state(&mut resources, 3);
        assert_eq!(
            first
                .session
                .unwrap()
                .position_micros
                .map(|micros| micros / 1_000_000),
            Some(10)
        );
        assert_eq!(
            second
                .session
                .unwrap()
                .position_micros
                .map(|micros| micros / 1_000_000),
            Some(11)
        );
        assert!(second.generation > first.generation);

        let paused = playing(PlaybackStatus::Paused, Some(Duration::from_secs(11)));
        assert_eq!(next_second(&paused), None);
        let almost = playing(PlaybackStatus::Playing, Some(Duration::from_millis(11_750)));
        assert_eq!(next_second(&almost), Some(Duration::from_millis(260)));
        let finished = playing(PlaybackStatus::Playing, Some(Duration::from_secs(200)));
        assert_eq!(next_second(&finished), None);
    }

    #[test]
    fn looks_the_same_ignores_only_fractions_of_a_second() {
        let base = session(&playing(
            PlaybackStatus::Playing,
            Some(Duration::from_millis(5_100)),
        ));
        let later = session(&playing(
            PlaybackStatus::Playing,
            Some(Duration::from_millis(5_900)),
        ));
        let next = session(&playing(
            PlaybackStatus::Playing,
            Some(Duration::from_millis(6_000)),
        ));
        let paused = session(&playing(
            PlaybackStatus::Paused,
            Some(Duration::from_millis(5_100)),
        ));
        assert!(looks_the_same(Some(&base), Some(&later)));
        assert!(!looks_the_same(Some(&base), Some(&next)));
        assert!(!looks_the_same(Some(&base), Some(&paused)));
        assert!(!looks_the_same(Some(&base), None));
        assert!(looks_the_same(None, None));
    }

    #[test]
    fn commands_need_scope_and_one_fresh_touch_each() {
        let source = Arc::new(FakeSource::default());
        source.set(Some(playing(PlaybackStatus::Playing, None)));
        let backend = backend(&source);
        let mut ledger = ActivationLedger::new(8);
        let allowed = [MediaControlAction::PlayPause, MediaControlAction::Seek];

        let untouched = command_request(MediaAction::PlayPause, &allowed, None);
        assert_eq!(
            Backend::authorize(backend.as_ref(), &untouched, &mut ledger, 0),
            Err(BrokerErrorCode::ActivationRequired)
        );
        let outside = command_request(MediaAction::Next, &allowed, Some(activation(1)));
        assert_eq!(
            Backend::authorize(backend.as_ref(), &outside, &mut ledger, 0),
            Err(BrokerErrorCode::OutOfScope)
        );

        let toggle = command_request(MediaAction::PlayPause, &allowed, Some(activation(2)));
        Backend::authorize(backend.as_ref(), &toggle, &mut ledger, 0).unwrap();
        assert_eq!(
            Backend::authorize(backend.as_ref(), &toggle, &mut ledger, 0),
            Err(BrokerErrorCode::ActivationRequired),
            "a touch authorizes one command"
        );
        assert_eq!(
            backend.execute(&toggle, &token()),
            BrokerResult::Success {
                payload: Vec::new()
            }
        );
        let seek = command_request(
            MediaAction::SetPosition {
                track_id: "/org/videolan/vlc/playlist/3".into(),
                position_micros: 30_000_000,
            },
            &allowed,
            Some(activation(3)),
        );
        Backend::authorize(backend.as_ref(), &seek, &mut ledger, 0).unwrap();
        assert!(matches!(
            backend.execute(&seek, &token()),
            BrokerResult::Success { .. }
        ));
        assert_eq!(
            *source.commands.lock().unwrap(),
            [
                (VLC.to_owned(), Command::PlayPause),
                (
                    VLC.to_owned(),
                    Command::SetPosition {
                        track_id: "/org/videolan/vlc/playlist/3".into(),
                        position: Duration::from_secs(30),
                    }
                ),
            ]
        );

        let mut elsewhere = command_request(MediaAction::PlayPause, &allowed, Some(activation(4)));
        elsewhere.payload = MediaCommand {
            player: "org.gnome.Shell".into(),
            action: MediaAction::PlayPause,
        }
        .encode()
        .unwrap();
        assert_eq!(
            Backend::authorize(backend.as_ref(), &elsewhere, &mut ledger, 0),
            Err(BrokerErrorCode::InvalidRequest),
            "commands only address MPRIS players"
        );
    }

    #[test]
    fn requests_for_the_wrong_operation_or_scope_are_refused() {
        let source = Arc::new(FakeSource::default());
        let backend = backend(&source);
        let mut wrong_operation = subscribe_request(4);
        wrong_operation.operation = "snapshot".into();
        assert_eq!(
            ResourceBackend::authorize(
                backend.as_ref(),
                &wrong_operation,
                &mut ActivationLedger::new(1),
                0
            ),
            Err(BrokerErrorCode::InvalidRequest)
        );
        let mut garbage = subscribe_request(4);
        garbage.payload = b"OMMEDRQ1x".to_vec();
        assert_eq!(
            ResourceBackend::authorize(
                backend.as_ref(),
                &garbage,
                &mut ActivationLedger::new(1),
                0
            ),
            Err(BrokerErrorCode::InvalidRequest)
        );
        // Nothing was started for refused requests.
        assert!(lock(&backend.shared.state).source.is_none());
    }
}
