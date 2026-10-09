//! Presses `keyboard.shortcut.v1` shortcuts in the focused application.
//!
//! Supervisors send only a shortcut id; the plugin manager looks it up in
//! the component's grant, so the keys and applications always come from the
//! user's approval. A worker thread then checks the focus once more, just
//! before pressing, and presses the keys through the desktop's
//! RemoteDesktop portal: Wayland gives no other client a way to type into
//! another application. GNOME asks the user once whether TouchBar may
//! control the keyboard; the portal's restore token then keeps the approval
//! across sessions. The portal session opens on the first press and closes
//! after a short idle time, so the desktop's remote-control indicator shows
//! only while shortcuts are in use.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    os::fd::AsRawFd,
    os::unix::fs::OpenOptionsExt,
    path::PathBuf,
    sync::{
        Arc, Mutex, PoisonError,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use touchbar_broker_schema::{ShortcutOutcome, ShortcutResult};
use touchbar_policy::KeyboardShortcut;
use touchbar_protocol::broker_ipc::Seqpacket;
use zbus::{
    MatchRule,
    blocking::{Connection, MessageIterator},
    message::Type,
    zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value},
};

/// The portal session closes after this long without a press.
const IDLE_CLOSE: Duration = Duration::from_secs(20);
/// A declined approval is not asked again for this long.
const DECLINED_PAUSE: Duration = Duration::from_secs(600);
/// The user may take a while to answer GNOME's approval dialog.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
const PORTAL_TIMEOUT: Duration = Duration::from_secs(5);

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";
/// RemoteDesktop device type KEYBOARD.
const KEYBOARD_DEVICE: u32 = 1;
/// Persist the approval until the user revokes it.
const PERSIST_UNTIL_REVOKED: u32 = 2;

/// One granted shortcut to press, and where to report the outcome.
pub struct ShortcutJob {
    pub request_id: u64,
    pub shortcut: KeyboardShortcut,
    pub reply: Seqpacket,
    /// Rechecks the current grant, cancellation and deadline at the side effect.
    pub validate: Box<dyn Fn() -> Result<(), ShortcutResult> + Send>,
}

/// Presses keys in whatever window has the focus. Errors mean the press did
/// not happen; `Declined` that the user refused the approval.
pub trait KeyInjector: Send + 'static {
    /// Completes any blocking desktop approval before authorization is rechecked.
    fn prepare(&mut self) -> Result<(), InjectError> {
        Ok(())
    }
    fn press(&mut self, codes: &[u16]) -> Result<(), InjectError>;
    /// Ends any open session; the next press opens a new one.
    fn close(&mut self);
}

#[derive(Debug)]
pub enum InjectError {
    Declined,
    Unavailable(anyhow::Error),
}

/// The focused application as the context sources report it, shared with
/// the worker so that the focus is checked at the moment of the press.
#[derive(Clone, Default)]
pub struct FocusedApplication(Arc<Mutex<Option<String>>>);

impl FocusedApplication {
    pub fn set(&self, application: Option<String>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = application;
    }

    fn get(&self) -> Option<String> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

pub struct ShortcutPresser {
    jobs: Sender<ShortcutJob>,
}

impl ShortcutPresser {
    pub fn start(focus: FocusedApplication, injector: impl KeyInjector) -> Result<Self> {
        let (jobs, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("touchbar-shortcuts".into())
            .spawn(move || run(&receiver, &focus, injector))
            .context("start shortcut worker")?;
        Ok(Self { jobs })
    }

    pub fn submit(&self, job: ShortcutJob) {
        // A worker that stopped answers nothing; the supervisor times out.
        let _ = self.jobs.send(job);
    }
}

fn run(jobs: &Receiver<ShortcutJob>, focus: &FocusedApplication, mut injector: impl KeyInjector) {
    let mut declined_until: Option<Instant> = None;
    loop {
        let job = match jobs.recv_timeout(IDLE_CLOSE) {
            Ok(job) => job,
            Err(RecvTimeoutError::Timeout) => {
                injector.close();
                continue;
            }
            Err(RecvTimeoutError::Disconnected) => {
                injector.close();
                return;
            }
        };
        let result = press(
            &job.shortcut,
            focus,
            &mut injector,
            &mut declined_until,
            || {
                (job.validate)()?;
                let mut descriptor = libc::pollfd {
                    fd: job.reply.as_raw_fd(),
                    events: 0,
                    revents: 0,
                };
                // SAFETY: descriptor refers to the job's live reply socket.
                let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
                if result < 0
                    || descriptor.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
                {
                    return Err(ShortcutResult::Unavailable);
                }
                Ok(())
            },
        );
        println!(
            "shortcut id={} result={result:?} focus={}",
            job.shortcut.id,
            focus.get().as_deref().unwrap_or("none")
        );
        let outcome = ShortcutOutcome {
            request_id: job.request_id,
            result,
        };
        if let Ok(payload) = outcome.encode() {
            let _ = job.reply.send_payload(&payload);
        }
    }
}

fn press(
    shortcut: &KeyboardShortcut,
    focus: &FocusedApplication,
    injector: &mut impl KeyInjector,
    declined_until: &mut Option<Instant>,
    validate: impl Fn() -> Result<(), ShortcutResult>,
) -> ShortcutResult {
    if let Err(result) = validate() {
        return result;
    }
    if !focus
        .get()
        .is_some_and(|application| shortcut.applications.contains(&application))
    {
        return ShortcutResult::NotFocused;
    }
    if declined_until.is_some_and(|until| Instant::now() < until) {
        return ShortcutResult::Unavailable;
    }
    if let Err(error) = injector.prepare() {
        return injection_error(error, injector, declined_until);
    }
    // Approval can outlive the operation or a grant, or focus another window.
    if let Err(result) = validate() {
        return result;
    }
    if !focus
        .get()
        .is_some_and(|application| shortcut.applications.contains(&application))
    {
        return ShortcutResult::NotFocused;
    }
    let codes = shortcut
        .keys
        .iter()
        .map(|key| key.evdev_code())
        .collect::<Vec<_>>();
    match injector.press(&codes) {
        Ok(()) => ShortcutResult::Pressed,
        Err(error) => injection_error(error, injector, declined_until),
    }
}

fn injection_error(
    error: InjectError,
    injector: &mut impl KeyInjector,
    declined_until: &mut Option<Instant>,
) -> ShortcutResult {
    match error {
        InjectError::Declined => {
            *declined_until = Some(Instant::now() + DECLINED_PAUSE);
            ShortcutResult::Unavailable
        }
        InjectError::Unavailable(error) => {
            eprintln!("shortcut-input=unavailable error={error:#}");
            injector.close();
            ShortcutResult::Unavailable
        }
    }
}

/// The RemoteDesktop portal on the session bus, keyboard only.
pub struct RemoteDesktopPortal {
    connection: Option<Connection>,
    session: Option<OwnedObjectPath>,
    /// Where the approval's restore token is kept between sessions.
    token_path: PathBuf,
    requests: u64,
}

impl RemoteDesktopPortal {
    pub fn new(token_path: PathBuf) -> Self {
        Self {
            connection: None,
            session: None,
            token_path,
            requests: 0,
        }
    }

    #[cfg(test)]
    fn with_connection(connection: Connection, token_path: PathBuf) -> Self {
        Self {
            connection: Some(connection),
            ..Self::new(token_path)
        }
    }

    fn connection(&mut self) -> Result<Connection> {
        if let Some(connection) = &self.connection {
            return Ok(connection.clone());
        }
        let connection = Connection::session().context("connect to the session bus")?;
        self.connection = Some(connection.clone());
        Ok(connection)
    }

    /// Calls a portal method that answers through a Request object and
    /// returns the results of its `Response`.
    fn request(
        &mut self,
        method: &str,
        body: impl FnOnce(String) -> RequestBody,
        timeout: Duration,
    ) -> Result<HashMap<String, OwnedValue>, InjectError> {
        let connection = self.connection().map_err(InjectError::Unavailable)?;
        self.requests += 1;
        let token = format!("touchbar{}_{}", std::process::id(), self.requests);
        let sender = connection
            .unique_name()
            .map(|name| name.trim_start_matches(':').replace('.', "_"))
            .ok_or_else(|| InjectError::Unavailable(anyhow::anyhow!("no unique bus name")))?;
        let path = format!("{PORTAL_PATH}/request/{sender}/{token}");
        let rule = MatchRule::builder()
            .msg_type(Type::Signal)
            .interface(REQUEST)
            .and_then(|rule| rule.member("Response"))
            .and_then(|rule| rule.path(path.as_str()))
            .map(|rule| rule.build().to_owned())
            .map_err(|error| InjectError::Unavailable(error.into()))?;
        let responses = MessageIterator::for_match_rule(rule, &connection, Some(4))
            .map_err(|error| InjectError::Unavailable(error.into()))?;
        let body = body(token);
        let call = match &body {
            RequestBody::Options(options) => connection.call_method(
                Some(PORTAL),
                PORTAL_PATH,
                Some(REMOTE_DESKTOP),
                method,
                &(options,),
            ),
            RequestBody::Session(session, options) => connection.call_method(
                Some(PORTAL),
                PORTAL_PATH,
                Some(REMOTE_DESKTOP),
                method,
                &(session, options),
            ),
            RequestBody::Start(session, options) => connection.call_method(
                Some(PORTAL),
                PORTAL_PATH,
                Some(REMOTE_DESKTOP),
                method,
                &(session, "", options),
            ),
        };
        call.map_err(|error| InjectError::Unavailable(error.into()))?;
        let (code, results) = wait_response(responses, timeout)?;
        match code {
            0 => Ok(results),
            1 => Err(InjectError::Declined),
            _ => Err(InjectError::Unavailable(anyhow::anyhow!(
                "portal {method} failed with response {code}"
            ))),
        }
    }

    fn open(&mut self) -> Result<OwnedObjectPath, InjectError> {
        if let Some(session) = &self.session {
            return Ok(session.clone());
        }
        let created = self.request(
            "CreateSession",
            |token| {
                RequestBody::Options(HashMap::from([
                    ("handle_token", Value::from(token.clone())),
                    ("session_handle_token", Value::from(token)),
                ]))
            },
            PORTAL_TIMEOUT,
        )?;
        let session = created
            .get("session_handle")
            .and_then(|value| String::try_from(value.clone()).ok())
            .and_then(|path| OwnedObjectPath::try_from(path).ok())
            .ok_or_else(|| InjectError::Unavailable(anyhow::anyhow!("portal gave no session")))?;
        let restore = fs::read_to_string(&self.token_path).ok();
        let selected_session = session.clone();
        self.request(
            "SelectDevices",
            move |token| {
                let mut options = HashMap::from([
                    ("handle_token", Value::from(token)),
                    ("types", Value::from(KEYBOARD_DEVICE)),
                    ("persist_mode", Value::from(PERSIST_UNTIL_REVOKED)),
                ]);
                if let Some(restore) = restore.map(|token| token.trim().to_owned())
                    && !restore.is_empty()
                {
                    options.insert("restore_token", Value::from(restore));
                }
                RequestBody::Session(selected_session, options)
            },
            PORTAL_TIMEOUT,
        )?;
        let started_session = session.clone();
        let started = self.request(
            "Start",
            move |token| {
                RequestBody::Start(
                    started_session,
                    HashMap::from([("handle_token", Value::from(token))]),
                )
            },
            APPROVAL_TIMEOUT,
        )?;
        let devices = started
            .get("devices")
            .and_then(|value| u32::try_from(value.clone()).ok())
            .unwrap_or_default();
        if devices & KEYBOARD_DEVICE == 0 {
            return Err(InjectError::Declined);
        }
        // Each start hands out a new single-use token.
        if let Some(token) = started
            .get("restore_token")
            .and_then(|value| String::try_from(value.clone()).ok())
            && let Err(error) = save_token(&self.token_path, &token)
        {
            eprintln!("shortcut-input=token-not-saved error={error:#}");
        }
        println!("shortcut-input=ready portal=remote-desktop");
        self.session = Some(session.clone());
        Ok(session)
    }
}

enum RequestBody {
    Options(HashMap<&'static str, Value<'static>>),
    Session(OwnedObjectPath, HashMap<&'static str, Value<'static>>),
    Start(OwnedObjectPath, HashMap<&'static str, Value<'static>>),
}

/// Waits for a Request's `Response`. The iterator blocks, so it runs on a
/// helper thread that ends with the connection or its first message.
fn wait_response(
    responses: MessageIterator,
    timeout: Duration,
) -> Result<(u32, HashMap<String, OwnedValue>), InjectError> {
    let (sender, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("touchbar-portal-response".into())
        .spawn(move || {
            for message in responses {
                let Ok(message) = message else {
                    continue;
                };
                if let Ok(response) = message
                    .body()
                    .deserialize::<(u32, HashMap<String, OwnedValue>)>()
                {
                    let _ = sender.send(response);
                    return;
                }
            }
        })
        .map_err(|error| InjectError::Unavailable(error.into()))?;
    receiver.recv_timeout(timeout).map_err(|_| {
        InjectError::Unavailable(anyhow::anyhow!("portal did not answer within {timeout:?}"))
    })
}

fn save_token(path: &PathBuf, token: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    let staging = path.with_extension("new");
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&staging)
        .with_context(|| format!("write {}", staging.display()))?;
    file.write_all(token.as_bytes())?;
    drop(file);
    fs::rename(&staging, path).with_context(|| format!("replace {}", path.display()))
}

impl KeyInjector for RemoteDesktopPortal {
    fn prepare(&mut self) -> Result<(), InjectError> {
        self.open().map(drop)
    }

    fn press(&mut self, codes: &[u16]) -> Result<(), InjectError> {
        let session = self.session.clone().ok_or_else(|| {
            InjectError::Unavailable(anyhow::anyhow!("portal session was not prepared"))
        })?;
        let connection = self.connection().map_err(InjectError::Unavailable)?;
        let key = |code: u16, state: u32| {
            connection.call_method(
                Some(PORTAL),
                PORTAL_PATH,
                Some(REMOTE_DESKTOP),
                "NotifyKeyboardKeycode",
                &(
                    ObjectPath::from(&session),
                    HashMap::<&str, Value<'_>>::new(),
                    i32::from(code),
                    state,
                ),
            )
        };
        // Press in order, release in reverse; release whatever went down
        // even if a later key failed, so no modifier stays held.
        let mut pressed = Vec::new();
        let mut failure = None;
        for &code in codes {
            match key(code, 1) {
                Ok(_) => pressed.push(code),
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        for &code in pressed.iter().rev() {
            if let Err(error) = key(code, 0) {
                failure.get_or_insert(error);
            }
        }
        match failure {
            None => Ok(()),
            Some(error) => {
                // The user may have stopped the session from the panel.
                self.session = None;
                Err(InjectError::Unavailable(error.into()))
            }
        }
    }

    fn close(&mut self) {
        if let (Some(session), Some(connection)) = (self.session.take(), &self.connection) {
            let _ = connection.call_method(Some(PORTAL), &session, Some(SESSION), "Close", &());
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use touchbar_policy::ShortcutKey;

    use super::*;
    use crate::test_bus::PrivateBus;

    #[derive(Clone, Default)]
    struct Recorder {
        presses: Arc<Mutex<Vec<Vec<u16>>>>,
        decline: bool,
    }

    impl KeyInjector for Recorder {
        fn press(&mut self, codes: &[u16]) -> Result<(), InjectError> {
            if self.decline {
                return Err(InjectError::Declined);
            }
            self.presses.lock().unwrap().push(codes.to_vec());
            Ok(())
        }

        fn close(&mut self) {}
    }

    fn back() -> KeyboardShortcut {
        KeyboardShortcut {
            id: "back".into(),
            keys: vec![ShortcutKey::Alt, ShortcutKey::Left],
            applications: BTreeSet::from(["firefox_firefox".into()]),
        }
    }

    fn outcome(reply: &Seqpacket) -> ShortcutOutcome {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(payload) = reply.try_recv_payload().unwrap() {
                return ShortcutOutcome::decode(&payload).unwrap();
            }
            assert!(Instant::now() < deadline, "no outcome");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// RemoteDesktop as xdg-desktop-portal offers it: every call answers on
    /// a Request object named by the caller's handle token.
    struct FakePortal {
        keys: Arc<Mutex<Vec<(i32, u32)>>>,
        restore_tokens: Arc<Mutex<Vec<Option<String>>>>,
        starts: Arc<Mutex<u32>>,
        decline: bool,
        before_start: Option<Box<dyn Fn() + Send + Sync>>,
    }

    async fn respond(
        connection: &zbus::Connection,
        header: &zbus::message::Header<'_>,
        options: &HashMap<String, OwnedValue>,
        code: u32,
        results: HashMap<&str, Value<'_>>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let sender = header.sender().unwrap().to_string();
        let token = String::try_from(options["handle_token"].clone()).unwrap();
        let path = format!(
            "{PORTAL_PATH}/request/{}/{token}",
            sender.trim_start_matches(':').replace('.', "_")
        );
        connection
            .emit_signal(
                Some(sender.as_str()),
                path.as_str(),
                REQUEST,
                "Response",
                &(code, results),
            )
            .await?;
        Ok(OwnedObjectPath::try_from(path).unwrap())
    }

    #[zbus::interface(name = "org.freedesktop.portal.RemoteDesktop")]
    impl FakePortal {
        async fn create_session(
            &self,
            options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] connection: &zbus::Connection,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            let token = String::try_from(options["session_handle_token"].clone()).unwrap();
            let session = format!("{PORTAL_PATH}/session/test/{token}");
            respond(
                connection,
                &header,
                &options,
                0,
                HashMap::from([("session_handle", Value::from(session))]),
            )
            .await
        }

        async fn select_devices(
            &self,
            _session: OwnedObjectPath,
            options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] connection: &zbus::Connection,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            assert_eq!(u32::try_from(options["types"].clone()).unwrap(), 1);
            assert_eq!(u32::try_from(options["persist_mode"].clone()).unwrap(), 2);
            self.restore_tokens.lock().unwrap().push(
                options
                    .get("restore_token")
                    .map(|token| String::try_from(token.clone()).unwrap()),
            );
            respond(connection, &header, &options, 0, HashMap::new()).await
        }

        async fn start(
            &self,
            _session: OwnedObjectPath,
            _parent_window: String,
            options: HashMap<String, OwnedValue>,
            #[zbus(header)] header: zbus::message::Header<'_>,
            #[zbus(connection)] connection: &zbus::Connection,
        ) -> zbus::fdo::Result<OwnedObjectPath> {
            if let Some(before_start) = &self.before_start {
                before_start();
            }
            if self.decline {
                return respond(connection, &header, &options, 1, HashMap::new()).await;
            }
            let start = {
                let mut starts = self.starts.lock().unwrap();
                *starts += 1;
                *starts
            };
            respond(
                connection,
                &header,
                &options,
                0,
                HashMap::from([
                    ("devices", Value::from(1_u32)),
                    ("restore_token", Value::from(format!("token-{start}"))),
                ]),
            )
            .await
        }

        fn notify_keyboard_keycode(
            &self,
            _session: OwnedObjectPath,
            _options: HashMap<String, OwnedValue>,
            keycode: i32,
            state: u32,
        ) {
            self.keys.lock().unwrap().push((keycode, state));
        }
    }

    fn portal(bus: &PrivateBus, decline: bool) -> (Connection, FakePortalLog) {
        portal_with_start(bus, decline, None)
    }

    fn portal_with_start(
        bus: &PrivateBus,
        decline: bool,
        before_start: Option<Box<dyn Fn() + Send + Sync>>,
    ) -> (Connection, FakePortalLog) {
        let log = FakePortalLog::default();
        let connection = zbus::blocking::connection::Builder::address(bus.address.as_str())
            .unwrap()
            .name(PORTAL)
            .unwrap()
            .serve_at(
                PORTAL_PATH,
                FakePortal {
                    keys: Arc::clone(&log.keys),
                    restore_tokens: Arc::clone(&log.restore_tokens),
                    starts: Arc::default(),
                    decline,
                    before_start,
                },
            )
            .unwrap()
            .build()
            .unwrap();
        (connection, log)
    }

    #[derive(Default)]
    struct FakePortalLog {
        keys: Arc<Mutex<Vec<(i32, u32)>>>,
        restore_tokens: Arc<Mutex<Vec<Option<String>>>>,
    }

    #[test]
    fn a_focus_change_during_portal_approval_presses_nothing() {
        let bus = PrivateBus::start();
        let focus = FocusedApplication::default();
        focus.set(Some("firefox_firefox".into()));
        let approval_focus = focus.clone();
        let (_portal, log) = portal_with_start(
            &bus,
            false,
            Some(Box::new(move || {
                approval_focus.set(Some("org.gnome.ptyxis".into()));
            })),
        );
        let mut injector =
            RemoteDesktopPortal::with_connection(bus.connect(), bus.file("remote-desktop.token"));
        let result = press(&back(), &focus, &mut injector, &mut None, || Ok(()));
        assert_eq!(result, ShortcutResult::NotFocused);
        assert!(log.keys.lock().unwrap().is_empty());
    }

    #[test]
    fn the_portal_presses_in_order_and_keeps_the_approval() {
        let bus = PrivateBus::start();
        let (_portal, log) = portal(&bus, false);
        let token_path = bus.file("remote-desktop.token");
        let mut injector = RemoteDesktopPortal::with_connection(bus.connect(), token_path.clone());

        injector.prepare().unwrap();
        injector.press(&[56, 105]).unwrap();
        assert_eq!(
            *log.keys.lock().unwrap(),
            [(56, 1), (105, 1), (105, 0), (56, 0)],
            "modifier first, released last"
        );
        assert_eq!(fs::read_to_string(&token_path).unwrap(), "token-1");
        // The open session is reused.
        injector.press(&[29, 48]).unwrap();
        assert_eq!(log.restore_tokens.lock().unwrap().len(), 1);

        // After the idle close, the next session restores the approval.
        injector.close();
        injector.prepare().unwrap();
        injector.press(&[29, 48]).unwrap();
        assert_eq!(
            *log.restore_tokens.lock().unwrap(),
            [None, Some("token-1".to_owned())]
        );
        assert_eq!(fs::read_to_string(&token_path).unwrap(), "token-2");
    }

    #[test]
    fn a_refused_approval_presses_nothing() {
        let bus = PrivateBus::start();
        let (_portal, log) = portal(&bus, true);
        let mut injector =
            RemoteDesktopPortal::with_connection(bus.connect(), bus.file("remote-desktop.token"));
        assert!(matches!(injector.prepare(), Err(InjectError::Declined)));
        assert!(log.keys.lock().unwrap().is_empty());
    }

    #[test]
    fn a_shortcut_is_pressed_only_in_its_own_focused_application() {
        let focus = FocusedApplication::default();
        let recorder = Recorder::default();
        let presser = ShortcutPresser::start(focus.clone(), recorder.clone()).unwrap();
        let (daemon, supervisor) = Seqpacket::pair().unwrap();
        let submit = |request_id| {
            presser.submit(ShortcutJob {
                request_id,
                shortcut: back(),
                reply: daemon.try_clone().unwrap(),
                validate: Box::new(|| Ok(())),
            });
            outcome(&supervisor)
        };

        assert_eq!(
            submit(1).result,
            ShortcutResult::NotFocused,
            "nothing focused"
        );
        focus.set(Some("org.gnome.ptyxis".into()));
        assert_eq!(submit(2).result, ShortcutResult::NotFocused, "a terminal");
        focus.set(Some("firefox_firefox".into()));
        let pressed = submit(3);
        assert_eq!(pressed.request_id, 3);
        assert_eq!(pressed.result, ShortcutResult::Pressed);
        assert_eq!(*recorder.presses.lock().unwrap(), [vec![56, 105]]);
    }

    #[test]
    fn a_declined_approval_is_not_asked_again_at_once() {
        let focus = FocusedApplication::default();
        focus.set(Some("firefox_firefox".into()));
        let mut recorder = Recorder {
            decline: true,
            ..Recorder::default()
        };
        let mut declined_until = None;
        assert_eq!(
            press(&back(), &focus, &mut recorder, &mut declined_until, || Ok(
                ()
            )),
            ShortcutResult::Unavailable
        );
        assert!(declined_until.is_some());
        recorder.decline = false;
        assert_eq!(
            press(&back(), &focus, &mut recorder, &mut declined_until, || Ok(
                ()
            )),
            ShortcutResult::Unavailable,
            "no second dialog right after a refusal"
        );
        assert!(recorder.presses.lock().unwrap().is_empty());
    }

    #[test]
    fn a_disconnected_supervisor_cannot_press_a_queued_shortcut() {
        let focus = FocusedApplication::default();
        focus.set(Some("firefox_firefox".into()));
        let recorder = Recorder::default();
        let (daemon, supervisor) = Seqpacket::pair().unwrap();
        let (submit, jobs) = mpsc::channel();
        submit
            .send(ShortcutJob {
                request_id: 1,
                shortcut: back(),
                reply: daemon,
                validate: Box::new(|| Ok(())),
            })
            .unwrap();
        drop(supervisor);
        drop(submit);
        run(&jobs, &focus, recorder.clone());
        assert!(recorder.presses.lock().unwrap().is_empty());
    }
}
