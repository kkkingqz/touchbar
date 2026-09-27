//! Context events from deterministic replay, Hyprland's event socket, or
//! GNOME Shell through the Window Monitor Pro extension.

use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    io::{BufRead, BufReader},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::Command,
    sync::mpsc::{self, Receiver, Sender, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use touchbar_model::ContextValue;
use zbus::{
    MatchRule,
    blocking::{
        Connection, MessageIterator, Proxy, connection::Builder as ConnectionBuilder,
        fdo::DBusProxy,
    },
    message::{Message, Type},
    names::{BusName, OwnedUniqueName, UniqueName},
    zvariant::OwnedValue,
};

use crate::wake::EventSignal;

const MAX_TRACKED_ACTIVITIES: usize = 128;
const APPLICATION_ID: &str = "application.id";

/// The GNOME Shell extension the GNOME source depends on:
/// https://extensions.gnome.org/extension/8549/window-monitor-pro/
const WINDOW_MONITOR_PRO: &str = "window-monitor-pro@muhammed.hussien2030.gmail.com";
const GNOME_SHELL: &str = "org.gnome.Shell";
const WINDOW_MONITOR_PATH: &str = "/org/gnome/Shell/Extensions/WindowMonitorPro";
const WINDOW_MONITOR_INTERFACE: &str = "org.gnome.Shell.Extensions.WindowMonitorPro";
const SHELL_EXTENSIONS_PATH: &str = "/org/gnome/Shell";
const SHELL_EXTENSIONS_INTERFACE: &str = "org.gnome.Shell.Extensions";
/// GNOME Shell's `ExtensionState.ACTIVE`, reported as a double.
const EXTENSION_ACTIVE: f64 = 1.0;
const GNOME_METHOD_TIMEOUT: Duration = Duration::from_secs(2);

/// A context fact to set, or one to withdraw so that `present` predicates no
/// longer match it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContextChange {
    Set(ContextEvent),
    Cleared(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextEvent {
    pub key: String,
    pub value: ContextValue,
}

impl ContextEvent {
    pub(crate) fn text(key: &str, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: ContextValue::Text(value.into()),
        }
    }

    pub(crate) fn boolean(key: &str, value: bool) -> Self {
        Self {
            key: key.into(),
            value: ContextValue::Boolean(value),
        }
    }

    fn application(value: &str) -> Self {
        Self::text("application.id", value.trim().to_ascii_lowercase())
    }

    fn activity(value: &str, active: bool) -> Self {
        Self {
            key: format!("activity.{value}"),
            value: ContextValue::Boolean(active),
        }
    }
}

#[derive(Default)]
struct HyprlandEventTracker {
    layer_counts: BTreeMap<String, u32>,
}

impl HyprlandEventTracker {
    fn parse(&mut self, line: &str) -> Option<ContextEvent> {
        let (name, payload) = line.split_once(">>")?;
        match name {
            "openlayer" => {
                let activity = normalized_activity(payload)?;
                if !self.layer_counts.contains_key(&activity)
                    && self.layer_counts.len() >= MAX_TRACKED_ACTIVITIES
                {
                    return None;
                }
                let count = self.layer_counts.entry(activity.clone()).or_default();
                *count = count.saturating_add(1);
                (*count == 1).then(|| ContextEvent::activity(&activity, true))
            }
            "closelayer" => {
                let activity = normalized_activity(payload)?;
                let count = self.layer_counts.get_mut(&activity)?;
                if *count > 1 {
                    *count -= 1;
                    None
                } else {
                    self.layer_counts.remove(&activity);
                    Some(ContextEvent::activity(&activity, false))
                }
            }
            _ => parse_hyprland_event(line),
        }
    }
}

fn normalized_activity(value: &str) -> Option<String> {
    let value = value.trim().to_ascii_lowercase();
    (!value.is_empty()
        && value.len() <= 112
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'-' | b'_' | b'.' | b':')
        }))
    .then_some(value)
}

pub struct ContextReplay {
    started: Option<Instant>,
    events: VecDeque<(Duration, ContextEvent)>,
}

impl ContextReplay {
    pub fn demo() -> Self {
        Self {
            started: None,
            events: [
                (
                    Duration::from_millis(150),
                    ContextEvent::text("application.id", "firefox"),
                ),
                (
                    Duration::from_millis(1_100),
                    ContextEvent::text("application.id", "terminal"),
                ),
                (
                    Duration::from_millis(1_600),
                    ContextEvent::text("application.id", "firefox"),
                ),
            ]
            .into(),
        }
    }

    pub fn poll(&mut self) -> Vec<ContextEvent> {
        let started = *self.started.get_or_insert_with(Instant::now);
        let elapsed = started.elapsed();
        let mut ready = Vec::new();
        while self
            .events
            .front()
            .is_some_and(|(scheduled, _)| *scheduled <= elapsed)
        {
            ready.push(self.events.pop_front().unwrap().1);
        }
        ready
    }

    pub fn finished(&self) -> bool {
        self.events.is_empty()
    }
}

pub struct HyprlandContextSource {
    receiver: Receiver<ContextEvent>,
    signal: EventSignal,
}

impl HyprlandContextSource {
    pub fn connect() -> Result<(Self, Vec<ContextEvent>)> {
        let socket = hyprland_event_socket()?;
        let stream = UnixStream::connect(&socket)
            .with_context(|| format!("connect Hyprland event socket {}", socket.display()))?;
        // Open the event stream before querying the snapshot so a focus change
        // cannot disappear between discovery and subscription. Seed the
        // reader's fact cache from that snapshot; title-only `activewindow`
        // traffic must not wake the compositor when the application class did
        // not change.
        let initial = initial_application().into_iter().collect::<Vec<_>>();
        let mut last_values = initial
            .iter()
            .map(|event| (event.key.clone(), event.value.clone()))
            .collect::<BTreeMap<_, _>>();
        let (sender, receiver) = mpsc::channel();
        let signal = EventSignal::new().context("create Hyprland event signal")?;
        let reader_signal = signal.try_clone().context("clone Hyprland event signal")?;
        thread::Builder::new()
            .name("hyprland-touchbar-context".into())
            .spawn(move || {
                let mut tracker = HyprlandEventTracker::default();
                for line in BufReader::new(stream).lines() {
                    let Ok(line) = line else {
                        break;
                    };
                    if let Some(event) = tracker.parse(&line)
                        && retain_changed_fact(&mut last_values, &event)
                    {
                        let closed_activity = (event.key.starts_with("activity.")
                            && event.value == ContextValue::Boolean(false))
                        .then(|| event.key.clone());
                        if sender.send(event).is_err() {
                            break;
                        }
                        if let Some(key) = closed_activity {
                            last_values.remove(&key);
                        }
                        reader_signal.notify();
                    }
                }
                // Also wake the event loop so channel disconnection is
                // observed without a background polling interval.
                reader_signal.notify();
            })
            .context("start Hyprland context reader")?;
        Ok((Self { receiver, signal }, initial))
    }

    pub fn notification_fd(&self) -> std::os::fd::RawFd {
        self.signal.as_raw_fd()
    }

    pub fn poll(&self) -> Result<Vec<ContextEvent>> {
        self.signal.drain().context("drain Hyprland event signal")?;
        let mut events = Vec::new();
        loop {
            match self.receiver.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty) => return Ok(events),
                Err(TryRecvError::Disconnected) => {
                    bail!("Hyprland context event stream disconnected")
                }
            }
        }
    }
}

enum GnomeUpdate {
    /// Window Monitor Pro became active (true), or inactive or uninstalled.
    Provider(bool),
    /// The focused window's application id, or `None` for the bare desktop.
    Focus(Option<String>),
}

/// `application.id` on GNOME, from Window Monitor Pro's focus signal.
///
/// GNOME has no event socket like Hyprland's. The extension emits a D-Bus
/// signal on every focus change, so nothing is polled. Without the extension
/// the fact is absent: the source follows GNOME Shell's extension state and
/// sets or withdraws the fact as Window Monitor Pro is enabled, disabled or
/// removed.
pub struct GnomeContextSource {
    receiver: Receiver<GnomeUpdate>,
    signal: EventSignal,
    provider_active: bool,
    application: Option<String>,
}

impl GnomeContextSource {
    pub fn start() -> Result<(Self, Option<ContextChange>)> {
        let connection = ConnectionBuilder::session()
            .context("connect to the session bus")?
            .method_timeout(GNOME_METHOD_TIMEOUT)
            .build()
            .context("connect to the session bus")?;
        Self::start_on(connection)
    }

    fn start_on(connection: Connection) -> Result<(Self, Option<ContextChange>)> {
        // Listen before taking the snapshot, as the Hyprland source does, so a
        // change in between cannot be lost. One stream keeps the extension's
        // state changes and its focus signals in bus order.
        let messages = MessageIterator::from(&connection);
        let bus = DBusProxy::new(&connection).context("create the D-Bus daemon proxy")?;
        for rule in [focus_rule()?, extension_rule()?] {
            bus.add_match_rule(rule)
                .context("subscribe to GNOME Shell signals")?;
        }
        let provider_active = window_monitor_active(&connection);
        let application = provider_active
            .then(|| focused_application(&connection))
            .flatten();
        let (sender, receiver) = mpsc::channel();
        let signal = EventSignal::new().context("create GNOME context signal")?;
        let reader_signal = signal.try_clone().context("clone GNOME context signal")?;
        thread::Builder::new()
            .name("gnome-touchbar-context".into())
            .spawn(move || run_gnome_reader(messages, &connection, &sender, &reader_signal))
            .context("start GNOME context reader")?;
        log_gnome_provider(provider_active);
        let mut source = Self {
            receiver,
            signal,
            provider_active,
            application: None,
        };
        let initial = source.settle(application);
        Ok((source, initial))
    }

    pub fn notification_fd(&self) -> std::os::fd::RawFd {
        self.signal.as_raw_fd()
    }

    /// Everything delivered since the last call, coalesced into at most one
    /// change: only the most recently focused application matters.
    pub fn poll(&mut self) -> Result<Option<ContextChange>> {
        self.signal.drain().context("drain GNOME context signal")?;
        let mut application = self.application.clone();
        loop {
            match self.receiver.try_recv() {
                Ok(GnomeUpdate::Provider(active)) => {
                    if active != self.provider_active {
                        self.provider_active = active;
                        log_gnome_provider(active);
                    }
                    if !active {
                        application = None;
                    }
                }
                // A focus signal the extension sent just before it went away
                // must not bring a stale application back.
                Ok(GnomeUpdate::Focus(focused)) if self.provider_active => application = focused,
                Ok(GnomeUpdate::Focus(_)) => {}
                Err(TryRecvError::Empty) => return Ok(self.settle(application)),
                Err(TryRecvError::Disconnected) => bail!("GNOME context reader stopped"),
            }
        }
    }

    /// Withdraws the fact this source set, once the source itself stops.
    pub fn withdraw(&mut self) -> Option<ContextChange> {
        self.settle(None)
    }

    fn settle(&mut self, application: Option<String>) -> Option<ContextChange> {
        if application == self.application {
            return None;
        }
        self.application = application.clone();
        Some(match application {
            Some(application) => {
                ContextChange::Set(ContextEvent::text(APPLICATION_ID, application))
            }
            None => ContextChange::Cleared(APPLICATION_ID.into()),
        })
    }
}

fn log_gnome_provider(active: bool) {
    if active {
        println!("context-source=gnome ready=true provider=window-monitor-pro");
    } else {
        println!("context-source=gnome ready=false reason=window-monitor-pro-inactive");
    }
}

fn run_gnome_reader(
    messages: MessageIterator,
    connection: &Connection,
    sender: &Sender<GnomeUpdate>,
    signal: &EventSignal,
) {
    let mut shell = ShellOwner(None);
    for message in messages {
        let Ok(message) = message else {
            continue;
        };
        let updates = gnome_updates(connection, &mut shell, &message);
        if updates.is_empty() {
            continue;
        }
        if updates
            .into_iter()
            .any(|update| sender.send(update).is_err())
        {
            break;
        }
        signal.notify();
    }
    // Also wake the event loop so a closed bus is observed.
    signal.notify();
}

fn gnome_updates(
    connection: &Connection,
    shell: &mut ShellOwner,
    message: &Message,
) -> Vec<GnomeUpdate> {
    if message.message_type() != Type::Signal {
        return Vec::new();
    }
    let header = message.header();
    let (Some(path), Some(interface), Some(member)) =
        (header.path(), header.interface(), header.member())
    else {
        return Vec::new();
    };
    let signal = (path.as_str(), interface.as_str(), member.as_str());
    let focus = signal
        == (
            WINDOW_MONITOR_PATH,
            WINDOW_MONITOR_INTERFACE,
            "WindowFocusChanged",
        );
    let extension = signal
        == (
            SHELL_EXTENSIONS_PATH,
            SHELL_EXTENSIONS_INTERFACE,
            "ExtensionStateChanged",
        );
    if !(focus || extension) || !shell.sent(connection, header.sender()) {
        return Vec::new();
    }
    if focus {
        let Ok((id, _title, class, _pid)) = message
            .body()
            .deserialize::<(String, String, String, String)>()
        else {
            return Vec::new();
        };
        return vec![GnomeUpdate::Focus(focused_window_application(&id, &class))];
    }
    let Ok((uuid, info)) = message
        .body()
        .deserialize::<(String, HashMap<String, OwnedValue>)>()
    else {
        return Vec::new();
    };
    if uuid != WINDOW_MONITOR_PRO {
        return Vec::new();
    }
    let active = extension_active(&info);
    let mut updates = vec![GnomeUpdate::Provider(active)];
    if active {
        updates.push(GnomeUpdate::Focus(focused_application(connection)));
    }
    updates
}

/// The unique name that owns org.gnome.Shell. Any client may address a signal
/// to this connection directly, so every accepted signal must come from it.
struct ShellOwner(Option<OwnedUniqueName>);

impl ShellOwner {
    fn sent(&mut self, connection: &Connection, sender: Option<&UniqueName<'_>>) -> bool {
        let Some(sender) = sender else {
            return false;
        };
        if self.matches(sender) {
            return true;
        }
        // Resolve again in case the shell restarted since the last signal.
        self.0 = shell_owner(connection);
        self.matches(sender)
    }

    fn matches(&self, sender: &UniqueName<'_>) -> bool {
        self.0
            .as_ref()
            .is_some_and(|owner| owner.as_str() == sender.as_str())
    }
}

fn shell_owner(connection: &Connection) -> Option<OwnedUniqueName> {
    let name = BusName::try_from(GNOME_SHELL).ok()?;
    DBusProxy::new(connection).ok()?.get_name_owner(name).ok()
}

fn focus_rule() -> Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(GNOME_SHELL)?
        .path(WINDOW_MONITOR_PATH)?
        .interface(WINDOW_MONITOR_INTERFACE)?
        .member("WindowFocusChanged")?
        .build())
}

fn extension_rule() -> Result<MatchRule<'static>> {
    Ok(MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(GNOME_SHELL)?
        .path(SHELL_EXTENSIONS_PATH)?
        .interface(SHELL_EXTENSIONS_INTERFACE)?
        .member("ExtensionStateChanged")?
        .arg(0, WINDOW_MONITOR_PRO)?
        .build())
}

fn window_monitor_active(connection: &Connection) -> bool {
    Proxy::new(
        connection,
        GNOME_SHELL,
        SHELL_EXTENSIONS_PATH,
        SHELL_EXTENSIONS_INTERFACE,
    )
    .and_then(|proxy| {
        proxy.call::<_, _, HashMap<String, OwnedValue>>("GetExtensionInfo", &(WINDOW_MONITOR_PRO,))
    })
    .is_ok_and(|info| extension_active(&info))
}

fn focused_application(connection: &Connection) -> Option<String> {
    let proxy = Proxy::new(
        connection,
        GNOME_SHELL,
        WINDOW_MONITOR_PATH,
        WINDOW_MONITOR_INTERFACE,
    )
    .ok()?;
    let class: String = proxy.call("FocusClass", &()).ok()?;
    application_id(&class)
}

fn extension_active(info: &HashMap<String, OwnedValue>) -> bool {
    info.get("state")
        .and_then(|state| f64::try_from(state).ok())
        .is_some_and(|state| state == EXTENSION_ACTIVE)
}

/// Window Monitor Pro reports the bare desktop as window 0 of class "Desktop".
fn focused_window_application(id: &str, class: &str) -> Option<String> {
    if id == "0" {
        return None;
    }
    application_id(class)
}

/// The same normalization as Hyprland's classes: trimmed and lowercased.
fn application_id(class: &str) -> Option<String> {
    let id = class.trim().to_ascii_lowercase();
    (!id.is_empty()).then_some(id)
}

fn retain_changed_fact(
    last_values: &mut BTreeMap<String, ContextValue>,
    event: &ContextEvent,
) -> bool {
    if last_values.get(&event.key) == Some(&event.value) {
        return false;
    }
    last_values.insert(event.key.clone(), event.value.clone());
    true
}

fn hyprland_event_socket() -> Result<PathBuf> {
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")
        .context("HYPRLAND_INSTANCE_SIGNATURE is not set")?;
    let mut candidates = Vec::new();
    if let Some(runtime) = std::env::var_os("XDG_RUNTIME_DIR") {
        candidates.push(
            PathBuf::from(runtime)
                .join("hypr")
                .join(&signature)
                .join(".socket2.sock"),
        );
    }
    candidates.push(
        PathBuf::from("/tmp/hypr")
            .join(signature)
            .join(".socket2.sock"),
    );
    candidates
        .into_iter()
        .find(|path| path.exists())
        .context("Hyprland event socket was not found")
}

fn initial_application() -> Option<ContextEvent> {
    let output = Command::new("hyprctl").arg("activewindow").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8(output.stdout).ok()?;
    stdout.lines().find_map(|line| {
        line.trim()
            .strip_prefix("class:")
            .map(str::trim)
            .filter(|class| !class.is_empty())
            .map(ContextEvent::application)
    })
}

fn parse_hyprland_event(line: &str) -> Option<ContextEvent> {
    let (name, payload) = line.split_once(">>")?;
    match name {
        "activewindow" => Some(ContextEvent::application(
            payload.split_once(',').map_or(payload, |(class, _)| class),
        )),
        "workspace" | "workspacev2" => Some(ContextEvent::text(
            "workspace.id",
            payload.rsplit_once(',').map_or(payload, |(_, name)| name),
        )),
        "focusedmon" => payload
            .split_once(',')
            .map(|(_, workspace)| ContextEvent::text("workspace.id", workspace)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_application_and_workspace_events() {
        assert_eq!(
            parse_hyprland_event("activewindow>>Firefox,Documentation"),
            Some(ContextEvent::application("firefox"))
        );
        assert_eq!(
            parse_hyprland_event("workspacev2>>3,coding"),
            Some(ContextEvent::text("workspace.id", "coding"))
        );
        assert_eq!(
            parse_hyprland_event("focusedmon>>eDP-1,web"),
            Some(ContextEvent::text("workspace.id", "web"))
        );
        assert_eq!(parse_hyprland_event("openwindow>>abc,1,class,title"), None);
    }

    #[test]
    fn unchanged_hyprland_facts_do_not_wake_the_compositor() {
        let mut last = BTreeMap::new();
        let foot = ContextEvent::application("foot");
        assert!(retain_changed_fact(&mut last, &foot));
        assert!(!retain_changed_fact(&mut last, &foot));

        let firefox = ContextEvent::application("firefox");
        assert!(retain_changed_fact(&mut last, &firefox));
        assert!(!retain_changed_fact(&mut last, &firefox));

        let workspace = ContextEvent::text("workspace.id", "2");
        assert!(retain_changed_fact(&mut last, &workspace));
        assert!(!retain_changed_fact(&mut last, &workspace));
    }

    fn gnome_source(provider_active: bool) -> (GnomeContextSource, Sender<GnomeUpdate>) {
        let (sender, receiver) = mpsc::channel();
        let source = GnomeContextSource {
            receiver,
            signal: EventSignal::new().unwrap(),
            provider_active,
            application: None,
        };
        (source, sender)
    }

    fn set(application: &str) -> Option<ContextChange> {
        Some(ContextChange::Set(ContextEvent::text(
            APPLICATION_ID,
            application,
        )))
    }

    fn cleared() -> Option<ContextChange> {
        Some(ContextChange::Cleared(APPLICATION_ID.into()))
    }

    #[test]
    fn gnome_classes_normalize_like_hyprland_and_the_desktop_has_none() {
        assert_eq!(
            application_id(" Org.Gnome.Ptyxis\n").as_deref(),
            Some("org.gnome.ptyxis")
        );
        assert_eq!(application_id(""), None);
        assert_eq!(focused_window_application("0", "Desktop"), None);
        assert_eq!(
            focused_window_application("16341828", "firefox_firefox").as_deref(),
            Some("firefox_firefox")
        );
    }

    #[test]
    fn only_an_active_extension_state_counts() {
        let state = |value: f64| {
            HashMap::from([(
                "state".to_owned(),
                zbus::zvariant::Value::from(value).try_to_owned().unwrap(),
            )])
        };
        assert!(extension_active(&state(1.0)));
        assert!(!extension_active(&state(2.0)));
        assert!(!extension_active(&HashMap::new()));
    }

    #[test]
    fn gnome_updates_coalesce_to_the_last_focused_application() {
        let (mut source, sender) = gnome_source(true);
        for focused in ["firefox_firefox", "org.gnome.ptyxis", "firefox_firefox"] {
            sender
                .send(GnomeUpdate::Focus(Some(focused.into())))
                .unwrap();
        }
        assert_eq!(source.poll().unwrap(), set("firefox_firefox"));
        sender
            .send(GnomeUpdate::Focus(Some("firefox_firefox".into())))
            .unwrap();
        assert_eq!(source.poll().unwrap(), None, "same application, no change");
        sender.send(GnomeUpdate::Focus(None)).unwrap();
        assert_eq!(
            source.poll().unwrap(),
            cleared(),
            "the bare desktop withdraws the fact"
        );
    }

    #[test]
    fn gnome_facts_follow_the_extension_state() {
        let (mut source, sender) = gnome_source(false);
        sender
            .send(GnomeUpdate::Focus(Some("firefox_firefox".into())))
            .unwrap();
        assert_eq!(
            source.poll().unwrap(),
            None,
            "no facts without the extension"
        );

        sender.send(GnomeUpdate::Provider(true)).unwrap();
        sender
            .send(GnomeUpdate::Focus(Some("org.gnome.ptyxis".into())))
            .unwrap();
        assert_eq!(source.poll().unwrap(), set("org.gnome.ptyxis"));

        sender.send(GnomeUpdate::Provider(false)).unwrap();
        // Emitted by the extension just before it was disabled.
        sender
            .send(GnomeUpdate::Focus(Some("firefox_firefox".into())))
            .unwrap();
        assert_eq!(source.poll().unwrap(), cleared());
        assert_eq!(source.withdraw(), None);

        sender.send(GnomeUpdate::Provider(true)).unwrap();
        sender.send(GnomeUpdate::Focus(Some("mpv".into()))).unwrap();
        assert_eq!(source.poll().unwrap(), set("mpv"));
        drop(sender);
        assert!(source.poll().is_err(), "a stopped reader is reported");
        assert_eq!(source.withdraw(), cleared());
    }

    mod gnome_shell_bus {
        use std::{
            os::fd::AsRawFd as _,
            sync::{Arc, Mutex},
        };

        use zbus::zvariant::Value;

        use super::*;

        struct PrivateBus {
            address: String,
            pid: libc::pid_t,
            _directory: tempfile::TempDir,
        }

        impl PrivateBus {
            fn start() -> Self {
                let directory = tempfile::tempdir().unwrap();
                let address = format!("unix:path={}", directory.path().join("bus").display());
                let output = Command::new("dbus-daemon")
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
                ConnectionBuilder::address(self.address.as_str())
                    .unwrap()
                    .method_timeout(GNOME_METHOD_TIMEOUT)
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

        #[derive(Clone, Default)]
        struct Desktop {
            extension_active: Arc<Mutex<bool>>,
            focused_class: Arc<Mutex<String>>,
        }

        struct FakeExtensions(Desktop);

        #[zbus::interface(name = "org.gnome.Shell.Extensions")]
        impl FakeExtensions {
            fn get_extension_info(&self, uuid: &str) -> HashMap<String, OwnedValue> {
                if uuid != WINDOW_MONITOR_PRO {
                    return HashMap::new();
                }
                let state = if *self.0.extension_active.lock().unwrap() {
                    1.0
                } else {
                    2.0
                };
                HashMap::from([(
                    "state".to_owned(),
                    Value::from(state).try_to_owned().unwrap(),
                )])
            }
        }

        struct FakeWindowMonitor(Desktop);

        #[zbus::interface(name = "org.gnome.Shell.Extensions.WindowMonitorPro")]
        impl FakeWindowMonitor {
            fn focus_class(&self) -> String {
                self.0.focused_class.lock().unwrap().clone()
            }
        }

        struct FakeShell {
            connection: Connection,
            desktop: Desktop,
        }

        impl FakeShell {
            fn start(bus: &PrivateBus, active: bool, focused: &str) -> Self {
                let desktop = Desktop::default();
                *desktop.extension_active.lock().unwrap() = active;
                *desktop.focused_class.lock().unwrap() = focused.into();
                let connection = ConnectionBuilder::address(bus.address.as_str())
                    .unwrap()
                    .name(GNOME_SHELL)
                    .unwrap()
                    .serve_at(SHELL_EXTENSIONS_PATH, FakeExtensions(desktop.clone()))
                    .unwrap()
                    .serve_at(WINDOW_MONITOR_PATH, FakeWindowMonitor(desktop.clone()))
                    .unwrap()
                    .build()
                    .unwrap();
                Self {
                    connection,
                    desktop,
                }
            }

            fn set_extension(&self, active: bool) {
                *self.desktop.extension_active.lock().unwrap() = active;
                let state = if active { 1.0 } else { 2.0 };
                let info = HashMap::from([("state", Value::from(state))]);
                self.connection
                    .emit_signal(
                        None::<()>,
                        SHELL_EXTENSIONS_PATH,
                        SHELL_EXTENSIONS_INTERFACE,
                        "ExtensionStateChanged",
                        &(WINDOW_MONITOR_PRO, info),
                    )
                    .unwrap();
            }

            fn focus(&self, id: &str, class: &str) {
                *self.desktop.focused_class.lock().unwrap() = class.into();
                emit_focus(&self.connection, None, id, class);
            }
        }

        fn emit_focus(connection: &Connection, destination: Option<&str>, id: &str, class: &str) {
            connection
                .emit_signal(
                    destination,
                    WINDOW_MONITOR_PATH,
                    WINDOW_MONITOR_INTERFACE,
                    "WindowFocusChanged",
                    &(id, "Title", class, "4242"),
                )
                .unwrap();
        }

        /// The next change the source reports, waiting on its eventfd.
        fn next_change(source: &mut GnomeContextSource) -> Option<ContextChange> {
            let deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < deadline {
                let mut poll_fd = libc::pollfd {
                    fd: source.notification_fd().as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                unsafe { libc::poll(&mut poll_fd, 1, 50) };
                if let Some(change) = source.poll().unwrap() {
                    return Some(change);
                }
            }
            None
        }

        /// No change within a short window.
        fn quiet(source: &mut GnomeContextSource) -> bool {
            thread::sleep(Duration::from_millis(300));
            source.poll().unwrap().is_none()
        }

        #[test]
        #[ignore = "requires a running GNOME session bus"]
        fn live_gnome_session_reports_its_provider_state() {
            let (source, initial) = GnomeContextSource::start().unwrap();
            println!(
                "provider_active={} initial={initial:?}",
                source.provider_active
            );
        }

        #[test]
        fn window_monitor_pro_drives_application_id() {
            let bus = PrivateBus::start();
            let shell = FakeShell::start(&bus, true, "Org.Gnome.Ptyxis");

            let (mut source, initial) = GnomeContextSource::start_on(bus.connect()).unwrap();
            assert_eq!(initial, set("org.gnome.ptyxis"), "snapshot at start");

            shell.focus("7", "firefox_firefox");
            assert_eq!(next_change(&mut source), set("firefox_firefox"));

            shell.focus("0", "Desktop");
            assert_eq!(next_change(&mut source), cleared());

            shell.focus("8", "mpv");
            assert_eq!(next_change(&mut source), set("mpv"));

            // Disabling the extension withdraws the fact; focus changes are
            // not reported while it is off.
            shell.set_extension(false);
            assert_eq!(next_change(&mut source), cleared());

            // Enabling it again reports the window focused at that moment.
            *shell.desktop.focused_class.lock().unwrap() = "Spotify".into();
            shell.set_extension(true);
            assert_eq!(next_change(&mut source), set("spotify"));
        }

        #[test]
        fn nothing_is_reported_without_the_extension() {
            let bus = PrivateBus::start();
            let shell = FakeShell::start(&bus, false, "firefox_firefox");

            let (mut source, initial) = GnomeContextSource::start_on(bus.connect()).unwrap();
            assert_eq!(initial, None);
            shell.focus("7", "firefox_firefox");
            assert!(quiet(&mut source));
        }

        #[test]
        fn signals_from_other_clients_are_ignored() {
            let bus = PrivateBus::start();
            let _shell = FakeShell::start(&bus, true, "mpv");
            let target = bus.connect();
            let unique = target.unique_name().unwrap().to_string();
            let (mut source, initial) = GnomeContextSource::start_on(target).unwrap();
            assert_eq!(initial, set("mpv"));

            // A signal addressed straight to the source, bypassing the
            // sender match rule, from a client that is not GNOME Shell.
            let impostor = bus.connect();
            emit_focus(&impostor, Some(&unique), "9", "impostor");
            emit_focus(&impostor, None, "9", "impostor");
            assert!(quiet(&mut source));
        }
    }

    #[test]
    fn layer_namespaces_become_refcounted_activity_facts() {
        let mut tracker = HyprlandEventTracker::default();
        assert_eq!(
            tracker.parse("openlayer>>Omarchy-Image-Selector"),
            Some(ContextEvent::activity("omarchy-image-selector", true))
        );
        assert_eq!(tracker.parse("openlayer>>omarchy-image-selector"), None);
        assert_eq!(tracker.parse("closelayer>>omarchy-image-selector"), None);
        assert_eq!(
            tracker.parse("closelayer>>omarchy-image-selector"),
            Some(ContextEvent::activity("omarchy-image-selector", false))
        );
        assert_eq!(tracker.parse("closelayer>>omarchy-image-selector"), None);
        assert_eq!(tracker.parse("openlayer>>not a namespace"), None);
    }
}
