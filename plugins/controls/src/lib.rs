//! System controls: volume, display brightness, microphone, battery and
//! power profile.
//!
//! Levels are read from the system with `command.run.v1` (`wpctl`,
//! `brightnessctl`, `powerprofilesctl`) when the plugin starts, whenever one
//! of its controls is touched, and whenever the system reports a change:
//! `pactl subscribe` for the volume and microphone, GNOME Shell's
//! `BrightnessChanged` signal for the display. Those watchers stop at the
//! command time limit and start again. Only one read or write runs at a
//! time: a drag queues only its latest value, so the level lands where the
//! finger stops.

use std::{
    cell::RefCell,
    collections::VecDeque,
    time::{Duration, Instant},
};

use touchbar_component_sdk::kit::{
    ColorRole, Icon, InputEvent, InputKind, Pressable, Representation, ViewBuilder, content, fixed,
    flexible,
};
use touchbar_component_sdk::{
    CommandEvent, CommandRunRequest, CommandValue, Guest, HostEvent, Item, PresentationEvent,
    RenderRequest, Update, View, command_run, decode_command_event, decode_command_opened,
    touchbar::plugin::{
        broker::{CapabilityStatus, OperationResult},
        ui::{
            PresentationBegin, PresentationCommand, PresentationDismissal, PresentationLifecycle,
            PresentationPlacement,
        },
    },
};

const COMMAND: &str = "command.run.v1";
const VOLUME_BUTTON: u64 = 100;
const VOLUME_SLIDER: u64 = 101;
const BRIGHTNESS_BUTTON: u64 = 200;
const BRIGHTNESS_SLIDER: u64 = 201;
const MICROPHONE_BUTTON: u64 = 300;
const POWER_BUTTON: u64 = 500;
/// Output beyond this is not a level; the readers print one short line.
const MAXIMUM_OUTPUT_BYTES: usize = 1024;
/// A command not finished by then lost its events; the next one may start.
/// The commands themselves time out after one second.
const ABANDON_AFTER: Duration = Duration::from_secs(3);
/// Watchers stop at 60 seconds; one silent for longer lost its events.
const ABANDON_WATCH_AFTER: Duration = Duration::from_secs(70);
/// A watcher that ends sooner failed; after this many in a row it is not
/// started again until one of its controls is touched.
const WATCH_HEALTHY_AFTER: Duration = Duration::from_secs(5);
const WATCH_ATTEMPTS: u8 = 3;
/// Events of resources whose completion has not arrived yet.
const MAXIMUM_EARLY_EVENTS: usize = 32;

struct Plugin;

thread_local! {
    static CONTROLS: RefCell<Controls> = RefCell::new(Controls::default());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Job {
    ReadVolume,
    ReadBrightness,
    ReadMicrophone,
    ReadPowerProfile,
    SetVolume(u32),
    UnmuteVolume,
    SetBrightness(u32),
    ToggleMicrophone,
    SetPowerProfile(u32),
}

impl Job {
    /// Jobs of one kind replace each other in the queue: a newer level
    /// supersedes an older one, and a second read would read the same.
    fn same_kind(self, other: Self) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&other)
    }

    /// The command for this job. Setting the brightness needs the display's
    /// backlight, known only once the backlights were listed.
    fn request(self, brightness_device: Option<&str>) -> Option<CommandRunRequest> {
        let (command_id, values) = match self {
            Self::ReadVolume => ("get-volume", Vec::new()),
            Self::ReadBrightness => ("list-brightness", Vec::new()),
            Self::ReadMicrophone => ("get-microphone", Vec::new()),
            Self::ReadPowerProfile => ("get-power-profile", Vec::new()),
            Self::SetVolume(value) => ("set-volume", vec![percent_value(value)]),
            Self::UnmuteVolume => ("unmute-volume", Vec::new()),
            Self::SetBrightness(value) => (
                "set-brightness",
                vec![
                    CommandValue::Text {
                        name: "device".into(),
                        value: brightness_device?.into(),
                    },
                    percent_value(value),
                ],
            ),
            Self::ToggleMicrophone => ("toggle-microphone", Vec::new()),
            Self::SetPowerProfile(value) => (
                "set-power-profile",
                vec![CommandValue::FixedEnum {
                    name: "profile".into(),
                    value: profile(value).into(),
                }],
            ),
        };
        Some(CommandRunRequest {
            command_id: command_id.into(),
            values,
        })
    }
}

/// A long-running command that reports system changes line by line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Watch {
    /// `pactl subscribe`: sinks, sources and the server's defaults.
    Audio,
    /// GNOME Shell's `BrightnessChanged` through `gdbus monitor`.
    Brightness,
}

impl Watch {
    const ALL: [Self; 2] = [Self::Audio, Self::Brightness];

    fn index(self) -> usize {
        self as usize
    }

    fn request(self) -> CommandRunRequest {
        CommandRunRequest {
            command_id: match self {
                Self::Audio => "watch-audio",
                Self::Brightness => "watch-brightness",
            }
            .into(),
            values: Vec::new(),
        }
    }

    /// The reads one line of output calls for.
    fn reads(self, line: &str) -> Vec<Job> {
        let mut reads = Vec::new();
        match self {
            Self::Audio => {
                // "Event 'change' on sink #56"; a new default is a server change.
                let server = line.contains(" on server");
                if server || line.contains(" on sink #") {
                    reads.push(Job::ReadVolume);
                }
                if server || line.contains(" on source #") {
                    reads.push(Job::ReadMicrophone);
                }
            }
            Self::Brightness => {
                if line.contains("BrightnessChanged") {
                    reads.push(Job::ReadBrightness);
                }
            }
        }
        reads
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Task {
    Job(Job),
    Watch(Watch),
}

/// A command in flight: a job, or a watcher.
struct Running {
    task: Task,
    request: u64,
    resource: Option<u64>,
    /// A job's whole output, or a watcher's unfinished line.
    stdout: Vec<u8>,
    started: Instant,
}

struct Controls {
    commands: bool,
    /// `None` until read; a control never shows a level it has not seen.
    volume: Option<u32>,
    volume_muted: bool,
    brightness: Option<u32>,
    /// The display's backlight, never the Touch Bar's own.
    brightness_device: Option<String>,
    microphone_muted: bool,
    power_profile: u32,
    /// The slider bar a control opened: 0 none, 1 persistent, 2 transient.
    presentation: u32,
    /// The slider a finger holds; reads do not move it meanwhile.
    dragging: Option<u64>,
    queue: VecDeque<Job>,
    running: Vec<Running>,
    /// Resource events that arrived before their completion.
    early: VecDeque<(u64, OperationResult)>,
    /// The watchers this process's items need, found when they render.
    wanted: [bool; 2],
    /// Watchers that ended early, in a row.
    watch_failures: [u8; 2],
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            commands: false,
            volume: None,
            volume_muted: false,
            brightness: None,
            brightness_device: None,
            microphone_muted: false,
            power_profile: 1,
            presentation: 0,
            dragging: None,
            queue: VecDeque::new(),
            running: Vec::new(),
            early: VecDeque::new(),
            wanted: [false; 2],
            watch_failures: [0; 2],
        }
    }
}

impl Guest for Plugin {
    fn items() -> Vec<Item> {
        vec![
            item("volume", "Volume"),
            item("brightness", "Brightness"),
            item("microphone", "Microphone"),
            item("battery", "Battery"),
            item("power-profile", "Power Profile"),
        ]
    }

    fn render(request: RenderRequest) -> Result<View, String> {
        CONTROLS.with_borrow_mut(|controls| {
            // Each item runs in its own process; only its own watcher runs.
            match request.item_id.as_str() {
                "volume" | "microphone" => controls.wanted[Watch::Audio.index()] = true,
                "brightness" => controls.wanted[Watch::Brightness.index()] = true,
                _ => {}
            }
            Ok(match request.item_id.as_str() {
                "volume" => continuous_view(
                    VOLUME_BUTTON,
                    if controls.volume_muted {
                        Icon::Muted
                    } else {
                        Icon::Volume
                    },
                    "VOL",
                    controls.volume,
                    controls.volume_muted,
                    request.viewport.width,
                ),
                "brightness" => continuous_view(
                    BRIGHTNESS_BUTTON,
                    Icon::ChevronRight,
                    "LIGHT",
                    controls.brightness,
                    false,
                    request.viewport.width,
                ),
                "microphone" => microphone_view(controls.microphone_muted),
                "battery" => battery_view(request.viewport.width),
                "power-profile" => power_view(controls.power_profile),
                item => return Err(format!("unknown item {item}")),
            })
        })
    }

    fn handle_event(event: InputEvent) -> Result<Update, String> {
        Ok(CONTROLS.with_borrow_mut(|controls| {
            let update = controls.input(&event);
            controls.pump();
            update
        }))
    }

    fn handle_presentation_event(event: PresentationEvent) -> Result<Update, String> {
        if matches!(event, PresentationEvent::Ended(_)) {
            CONTROLS.with_borrow_mut(|controls| controls.presentation = 0);
        }
        Ok(Update {
            rerender: true,
            presentation: None,
        })
    }

    fn handle_host_event(event: HostEvent) -> Result<Update, String> {
        let rerender = CONTROLS.with_borrow_mut(|controls| {
            let rerender = controls.host_event(event);
            controls.pump();
            rerender
        });
        Ok(Update {
            rerender,
            presentation: None,
        })
    }
}

impl Controls {
    fn input(&mut self, event: &InputEvent) -> Update {
        let mut presentation = None;
        match (event.widget_id, event.kind) {
            // A touch on a control first refreshes what it shows.
            // A touch also gives a failed watcher another chance.
            (VOLUME_BUTTON, InputKind::Pressed) => {
                self.watch_failures[Watch::Audio.index()] = 0;
                self.enqueue(Job::ReadVolume);
            }
            (BRIGHTNESS_BUTTON, InputKind::Pressed) => {
                self.watch_failures[Watch::Brightness.index()] = 0;
                self.enqueue(Job::ReadBrightness);
            }
            (MICROPHONE_BUTTON, InputKind::Pressed) => {
                self.watch_failures[Watch::Audio.index()] = 0;
                self.enqueue(Job::ReadMicrophone);
            }
            (POWER_BUTTON, InputKind::Pressed) => self.enqueue(Job::ReadPowerProfile),
            // As on macOS, a touch opens the slider and a hold opens it until
            // the finger lifts.
            (VOLUME_BUTTON | BRIGHTNESS_BUTTON, InputKind::Activated) => {
                self.presentation = 1;
                presentation = Some(PresentationCommand::Begin(PresentationBegin {
                    placement: PresentationPlacement::Anchored,
                    lifecycle: PresentationLifecycle::Persistent,
                    target: None,
                }));
            }
            (VOLUME_BUTTON | BRIGHTNESS_BUTTON, InputKind::LongPressed) => {
                self.presentation = 2;
                presentation = Some(PresentationCommand::Begin(PresentationBegin {
                    placement: PresentationPlacement::Anchored,
                    lifecycle: PresentationLifecycle::Transient,
                    target: None,
                }));
            }
            (VOLUME_SLIDER | BRIGHTNESS_SLIDER, InputKind::Pressed) => {
                self.dragging = Some(event.widget_id);
            }
            (VOLUME_SLIDER, InputKind::ValueChanged) => {
                if let Some(value) = event.value {
                    let next = percent(value);
                    self.volume = Some(next);
                    // Moving the volume unmutes, as the volume keys do.
                    if self.volume_muted {
                        self.volume_muted = false;
                        self.enqueue(Job::UnmuteVolume);
                    }
                    self.enqueue(Job::SetVolume(next));
                }
            }
            (BRIGHTNESS_SLIDER, InputKind::ValueChanged) => {
                if let Some(value) = event.value {
                    let next = percent(value);
                    self.brightness = Some(next);
                    self.enqueue(Job::SetBrightness(next));
                }
            }
            (VOLUME_SLIDER | BRIGHTNESS_SLIDER, InputKind::Released | InputKind::Cancelled) => {
                self.dragging = None;
                if self.presentation != 0 {
                    presentation = Some(PresentationCommand::End(PresentationDismissal::Selection));
                }
            }
            (MICROPHONE_BUTTON, InputKind::Activated) => {
                self.microphone_muted = !self.microphone_muted;
                self.enqueue(Job::ToggleMicrophone);
            }
            (POWER_BUTTON, InputKind::Activated) => {
                self.power_profile = (self.power_profile + 1) % 3;
                self.enqueue(Job::SetPowerProfile(self.power_profile));
            }
            _ => {}
        }
        Update {
            rerender: true,
            presentation,
        }
    }

    fn host_event(&mut self, event: HostEvent) -> bool {
        match event {
            HostEvent::CapabilityChanged((_, state)) if state.capability == COMMAND => {
                let granted = state.status == CapabilityStatus::Granted;
                let newly = granted && !self.commands;
                self.commands = granted;
                if newly {
                    self.read_everything();
                }
                if !granted {
                    self.queue.clear();
                    self.running.clear();
                    self.early.clear();
                }
                true
            }
            HostEvent::Completion((request, result)) => {
                let Some(index) = self
                    .running
                    .iter()
                    .position(|running| running.request == request)
                else {
                    return false;
                };
                let opened = match result {
                    OperationResult::Success(payload) => decode_command_opened(&payload).ok(),
                    OperationResult::Error(_) => None,
                };
                let Some(resource) = opened.map(|resource| resource.into_raw()) else {
                    let failed = self.running.remove(index);
                    self.ended(&failed);
                    return false;
                };
                self.running[index].resource = Some(resource);
                let early = std::mem::take(&mut self.early);
                let mut rerender = false;
                for (id, result) in early {
                    if id == resource {
                        rerender |= self.resource_event(resource, result);
                    } else {
                        self.early.push_back((id, result));
                    }
                }
                rerender
            }
            HostEvent::ResourceEvent((resource, _, result)) => {
                if self
                    .running
                    .iter()
                    .any(|running| running.resource == Some(resource))
                {
                    return self.resource_event(resource, result);
                }
                // Its completion may still be on the way.
                if self
                    .running
                    .iter()
                    .any(|running| running.resource.is_none())
                {
                    if self.early.len() == MAXIMUM_EARLY_EVENTS {
                        self.early.pop_front();
                    }
                    self.early.push_back((resource, result));
                }
                false
            }
            // Events were dropped: a command may never report its end, and a
            // level may have been missed.
            HostEvent::Overflow(_) => {
                self.running.clear();
                self.early.clear();
                self.read_everything();
                false
            }
            HostEvent::Shutdown(_) => {
                self.queue.clear();
                self.running.clear();
                self.early.clear();
                self.wanted = [false; 2];
                false
            }
            _ => false,
        }
    }

    /// One event of a known command. Returns whether a level changed.
    fn resource_event(&mut self, resource: u64, result: OperationResult) -> bool {
        let Some(index) = self
            .running
            .iter()
            .position(|running| running.resource == Some(resource))
        else {
            return false;
        };
        let event = match result {
            OperationResult::Success(payload) => decode_command_event(&payload).ok(),
            OperationResult::Error(_) => None,
        };
        match event {
            Some(CommandEvent::Stdout(bytes)) => {
                let running = &mut self.running[index];
                match running.task {
                    Task::Job(_) => {
                        let room = MAXIMUM_OUTPUT_BYTES.saturating_sub(running.stdout.len());
                        running.stdout.extend(bytes.into_iter().take(room));
                    }
                    Task::Watch(watch) => {
                        running.stdout.extend(bytes);
                        let mut reads = Vec::new();
                        while let Some(end) = running.stdout.iter().position(|byte| *byte == b'\n')
                        {
                            let line = running.stdout.drain(..=end).collect::<Vec<_>>();
                            reads.extend(watch.reads(&String::from_utf8_lossy(&line)));
                        }
                        // A line this long is not one of the watchers' lines.
                        if running.stdout.len() > MAXIMUM_OUTPUT_BYTES {
                            running.stdout.clear();
                        }
                        for read in reads {
                            self.enqueue(read);
                        }
                    }
                }
                false
            }
            Some(CommandEvent::Stderr(_)) => false,
            Some(CommandEvent::Exited { exit_code, .. }) => {
                let finished = self.running.remove(index);
                match finished.task {
                    Task::Job(job) if exit_code == Some(0) => {
                        self.apply_output(job, &finished.stdout);
                        true
                    }
                    _ => {
                        self.ended(&finished);
                        false
                    }
                }
            }
            None => {
                let failed = self.running.remove(index);
                self.ended(&failed);
                false
            }
        }
    }

    /// Books a watcher's end: one that ran a while is simply started again,
    /// one that failed at once counts towards giving up.
    fn ended(&mut self, running: &Running) {
        if let Task::Watch(watch) = running.task {
            let failures = &mut self.watch_failures[watch.index()];
            if running.started.elapsed() >= WATCH_HEALTHY_AFTER {
                *failures = 0;
            } else {
                *failures = failures.saturating_add(1);
            }
        }
    }

    fn read_everything(&mut self) {
        for job in [
            Job::ReadVolume,
            Job::ReadBrightness,
            Job::ReadMicrophone,
            Job::ReadPowerProfile,
        ] {
            self.enqueue(job);
        }
    }

    /// Queues a job, replacing a waiting one of the same kind.
    fn enqueue(&mut self, job: Job) {
        if !self.commands {
            return;
        }
        match self.queue.iter_mut().find(|queued| queued.same_kind(job)) {
            Some(queued) => *queued = job,
            None => self.queue.push_back(job),
        }
    }

    /// Starts the next job if none runs, and every wanted watcher that is
    /// not running. Broker requests are accepted only in input and
    /// host-event callbacks, which is where this is called.
    fn pump(&mut self) {
        self.running.retain(|running| {
            let limit = match running.task {
                Task::Job(_) => ABANDON_AFTER,
                Task::Watch(_) => ABANDON_WATCH_AFTER,
            };
            running.started.elapsed() < limit
        });
        if !self.commands {
            return;
        }
        while !self
            .running
            .iter()
            .any(|running| matches!(running.task, Task::Job(_)))
        {
            let Some(job) = self.queue.pop_front() else {
                break;
            };
            let Some(request) = job.request(self.brightness_device.as_deref()) else {
                continue;
            };
            self.start(Task::Job(job), &request);
        }
        for watch in Watch::ALL {
            let running = self
                .running
                .iter()
                .any(|running| running.task == Task::Watch(watch));
            if self.wanted[watch.index()]
                && !running
                && self.watch_failures[watch.index()] < WATCH_ATTEMPTS
            {
                self.start(Task::Watch(watch), &watch.request());
            }
        }
    }

    fn start(&mut self, task: Task, request: &CommandRunRequest) {
        match command_run(request) {
            Ok(id) => self.running.push(Running {
                task,
                request: id.into_raw(),
                resource: None,
                stdout: Vec::new(),
                started: Instant::now(),
            }),
            // Refused at once, for example by the process quota.
            Err(_) => {
                if let Task::Watch(watch) = task {
                    let failures = &mut self.watch_failures[watch.index()];
                    *failures = failures.saturating_add(1);
                }
            }
        }
    }

    /// A read shows the system's level unless the user is setting it: a
    /// finger on the slider or a newer level still waiting to be applied.
    fn apply_output(&mut self, job: Job, stdout: &[u8]) {
        let output = String::from_utf8_lossy(stdout);
        let setting = |slider: u64, kind: Job| {
            self.dragging == Some(slider) || self.queue.iter().any(|queued| queued.same_kind(kind))
        };
        match job {
            Job::ReadVolume if !setting(VOLUME_SLIDER, Job::SetVolume(0)) => {
                if let Some((level, muted)) = parse_wpctl_volume(&output) {
                    self.volume = Some(level);
                    self.volume_muted = muted && !self.queue.contains(&Job::UnmuteVolume);
                }
            }
            Job::ReadBrightness => {
                let display = display_backlight(&output);
                let setting = setting(BRIGHTNESS_SLIDER, Job::SetBrightness(0));
                if !setting {
                    self.brightness = display.as_ref().map(|(_, level)| *level);
                }
                self.brightness_device = display.map(|(device, _)| device);
            }
            Job::ReadMicrophone if !self.queue.contains(&Job::ToggleMicrophone) => {
                if let Some((_, muted)) = parse_wpctl_volume(&output) {
                    self.microphone_muted = muted;
                }
            }
            Job::ReadPowerProfile
                if !self
                    .queue
                    .iter()
                    .any(|queued| queued.same_kind(Job::SetPowerProfile(0))) =>
            {
                if let Some(value) = parse_power_profile(&output) {
                    self.power_profile = value;
                }
            }
            _ => {}
        }
    }
}

fn item(id: &str, label: &str) -> Item {
    Item {
        id: id.into(),
        label: label.into(),
    }
}

fn percent(value: f32) -> u32 {
    (value.clamp(0.0, 1.0) * 100.0).round() as u32
}

fn percent_value(value: u32) -> CommandValue {
    CommandValue::Text {
        name: "percent".into(),
        value: format!("{value}%"),
    }
}

/// `wpctl get-volume` prints `Volume: 0.89`, with ` [MUTED]` when muted.
/// Volumes above 100% show as full.
fn parse_wpctl_volume(output: &str) -> Option<(u32, bool)> {
    let rest = output.trim().strip_prefix("Volume:")?.trim();
    let (number, flags) = rest.split_once(' ').unwrap_or((rest, ""));
    let volume = number
        .parse::<f64>()
        .ok()
        .filter(|volume| volume.is_finite())?;
    let level = (volume.clamp(0.0, 1.0) * 100.0).round() as u32;
    Some((level, flags.contains("[MUTED]")))
}

/// The Touch Bar's own backlights, as touchbard knows them: the Intel T2's
/// `hid-appletb-bl` and Apple silicon's display pipe and DSI panel. The
/// Touch Bar service owns them; this plugin sets the display.
const TOUCH_BAR_BACKLIGHTS: &[&str] = &[
    "appletb_backlight",
    "228200000.display-pipe.0",
    "228600000.dsi.0",
];
/// Longer names, or names with other characters, are not sysfs devices.
const MAXIMUM_DEVICE_BYTES: usize = 64;

/// The display's backlight and its level, from `brightnessctl -l -m`: the
/// first backlight that is not the Touch Bar's. brightnessctl alone would
/// take the first backlight of all, which can be the Touch Bar itself.
fn display_backlight(output: &str) -> Option<(String, u32)> {
    output.lines().find_map(|line| {
        let device = line.split(',').next()?;
        let valid = !device.is_empty()
            && device.len() <= MAXIMUM_DEVICE_BYTES
            && device
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:".contains(&byte));
        if !valid || TOUCH_BAR_BACKLIGHTS.contains(&device) {
            return None;
        }
        Some((device.to_owned(), parse_brightnessctl(line)?))
    })
}

/// One line of `brightnessctl -m`: `device,class,current,percent%,maximum`.
/// The percentage is worked out from the raw values; brightnessctl's own one
/// is rounded down.
fn parse_brightnessctl(line: &str) -> Option<u32> {
    let fields = line.trim().split(',').collect::<Vec<_>>();
    let [_, _, current, _, maximum] = fields.as_slice() else {
        return None;
    };
    let current = current.trim().parse::<u64>().ok()?;
    let maximum = maximum
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|maximum| *maximum > 0)?;
    Some(((current.min(maximum) * 100 + maximum / 2) / maximum) as u32)
}

fn parse_power_profile(output: &str) -> Option<u32> {
    match output.trim() {
        "power-saver" => Some(0),
        "balanced" => Some(1),
        "performance" => Some(2),
        _ => None,
    }
}

/// A level not read yet shows as a dash and an empty slider; a muted one
/// says so and keeps its level on the slider.
fn continuous_view(
    base: u64,
    icon: Icon,
    short: &str,
    value: Option<u32>,
    muted: bool,
    width: f32,
) -> View {
    let mut b = ViewBuilder::new();
    let shown = match value {
        _ if muted => "MUTED".to_owned(),
        Some(value) => format!("{value}%"),
        None => "–".to_owned(),
    };
    let compact_icon = b.icon(icon, ColorRole::Foreground, short);
    let compact_label = b.label(format!("{short} {shown}"), 11.0, ColorRole::Foreground);
    let compact_content = b.row(
        vec![
            fixed(compact_icon, 24.0),
            flexible(compact_label, 36.0, 80.0, 160.0),
        ],
        3.0,
        2.0,
    );
    let compact = b.pressable(
        base,
        format!("Expand {short}"),
        compact_content,
        Pressable {
            selected: Some(muted || value == Some(0)),
            hold_ms: Some(420),
            ..Pressable::control()
        },
    );
    let icon = b.icon(icon, ColorRole::Foreground, short);
    let slider = b.slider(base + 1, short, value.unwrap_or(0) as f32 / 100.0);
    let label = b.label(shown, 11.0, ColorRole::Foreground);
    let full = b.row(
        vec![
            fixed(icon, 30.0),
            flexible(slider, 60.0, 120.0, 800.0),
            content(label, 34.0, 48.0),
        ],
        5.0,
        4.0,
    );
    let root = if width < 200.0 {
        compact
    } else {
        b.responsive(
            base + 10,
            vec![
                (Representation::Minimal, 0.0, compact),
                (Representation::Full, 120.0, full),
            ],
        )
    };
    b.finish(root)
}

fn microphone_view(muted: bool) -> View {
    let mut b = ViewBuilder::new();
    let icon = b.icon(
        if muted { Icon::Muted } else { Icon::Volume },
        if muted {
            ColorRole::Destructive
        } else {
            ColorRole::Foreground
        },
        "Microphone",
    );
    let label = b.label(
        if muted { "MUTED" } else { "MIC" },
        11.0,
        if muted {
            ColorRole::Destructive
        } else {
            ColorRole::Foreground
        },
    );
    let row = b.row(
        vec![fixed(icon, 28.0), content(label, 28.0, 80.0)],
        4.0,
        3.0,
    );
    let root = b.pressable(
        MICROPHONE_BUTTON,
        "Toggle microphone",
        row,
        Pressable {
            selected: Some(muted),
            ..Pressable::control()
        },
    );
    b.finish(root)
}

fn battery_view(width: f32) -> View {
    let mut b = ViewBuilder::new();
    let level = 0.82;
    let label = b.label("82%", 11.0, ColorRole::Foreground);
    let progress = b.progress("Battery", level, ColorRole::Accent);
    let full = b.row(
        vec![
            content(label, 32.0, 42.0),
            flexible(progress, 40.0, 100.0, 800.0),
        ],
        5.0,
        5.0,
    );
    let root = if width < 100.0 { label } else { full };
    b.finish(root)
}

fn power_view(current: u32) -> View {
    let mut b = ViewBuilder::new();
    let label = b.label(profile(current).to_uppercase(), 10.0, ColorRole::OnAccent);
    let root = b.pressable(
        POWER_BUTTON,
        "Cycle power profile",
        label,
        Pressable::accent(),
    );
    b.finish(root)
}

fn profile(value: u32) -> &'static str {
    match value {
        0 => "power-saver",
        2 => "performance",
        _ => "balanced",
    }
}

touchbar_component_sdk::export!(Plugin);

#[cfg(test)]
mod tests {
    use touchbar_component_sdk::CommandOpened;

    use super::*;

    #[test]
    fn levels_are_read_from_the_tools_output() {
        assert_eq!(parse_wpctl_volume("Volume: 0.89\n"), Some((89, false)));
        assert_eq!(
            parse_wpctl_volume("Volume: 1.00 [MUTED]\n"),
            Some((100, true))
        );
        assert_eq!(parse_wpctl_volume("Volume: 1.35\n"), Some((100, false)));
        assert_eq!(parse_wpctl_volume("Could not connect to PipeWire"), None);
        // 17413 of 65535 is 26.57%: brightnessctl itself says 27% here.
        assert_eq!(
            parse_brightnessctl("gmux_backlight,backlight,17413,27%,65535\n"),
            Some(27)
        );
        assert_eq!(parse_brightnessctl("x,backlight,5,50%,0"), None);
        assert_eq!(parse_power_profile("performance\n"), Some(2));
    }

    #[test]
    fn brightness_is_the_displays_never_the_touch_bars() {
        // This MacBookPro16,1 lists the display first, but order is not a
        // promise: here the Touch Bar comes first.
        let listing = "appletb_backlight,backlight,2,100%,2\n\
                       gmux_backlight,backlight,17413,27%,65535\n";
        assert_eq!(
            display_backlight(listing),
            Some(("gmux_backlight".to_owned(), 27))
        );
        assert_eq!(
            display_backlight(
                "228200000.display-pipe.0,backlight,1,1%,255\n\
                               apple-panel-bl,backlight,250,50%,500\n"
            ),
            Some(("apple-panel-bl".to_owned(), 50))
        );
        assert_eq!(
            display_backlight("appletb_backlight,backlight,2,100%,2\n"),
            None
        );
        assert_eq!(display_backlight("-d x,backlight,1,1%,2\n"), None);
        assert_eq!(display_backlight(""), None);
    }

    #[test]
    fn brightness_is_set_only_on_a_known_display() {
        assert_eq!(Job::SetBrightness(40).request(None), None);
        let request = Job::SetBrightness(40)
            .request(Some("gmux_backlight"))
            .unwrap();
        assert_eq!(request.command_id, "set-brightness");
        assert_eq!(
            request.values,
            [
                CommandValue::Text {
                    name: "device".into(),
                    value: "gmux_backlight".into()
                },
                percent_value(40)
            ]
        );
    }

    #[test]
    fn a_power_profile_parses() {
        assert_eq!(parse_power_profile("performance\n"), Some(2));
        assert_eq!(parse_power_profile("custom"), None);
    }

    fn granted() -> Controls {
        let mut controls = Controls {
            commands: true,
            ..Controls::default()
        };
        controls.running.push(Running {
            task: Task::Job(Job::ReadVolume),
            request: 1,
            resource: Some(1),
            stdout: Vec::new(),
            started: Instant::now(),
        });
        controls
    }

    fn watching(controls: &mut Controls, watch: Watch, resource: u64) {
        controls.running.push(Running {
            task: Task::Watch(watch),
            request: 100 + resource,
            resource: Some(resource),
            stdout: Vec::new(),
            started: Instant::now(),
        });
    }

    fn stdout(resource: u64, text: &str) -> HostEvent {
        HostEvent::ResourceEvent((
            resource,
            1,
            OperationResult::Success(
                CommandEvent::Stdout(text.as_bytes().to_vec())
                    .encode()
                    .unwrap(),
            ),
        ))
    }

    fn exited(resource: u64) -> HostEvent {
        HostEvent::ResourceEvent((
            resource,
            2,
            OperationResult::Success(
                CommandEvent::Exited {
                    exit_code: Some(0),
                    signal: None,
                    stdout_bytes: 13,
                    stderr_bytes: 0,
                }
                .encode()
                .unwrap(),
            ),
        ))
    }

    #[test]
    fn output_that_overtakes_the_completion_still_counts() {
        let mut controls = granted();
        controls.running[0].resource = None;
        assert!(!controls.host_event(stdout(9, "Volume: 0.89\n")));
        assert!(!controls.host_event(exited(9)));
        assert_eq!(controls.volume, None, "held until the completion names it");
        let opened = CommandOpened { resource_id: 9 }.encode().unwrap();
        let completion = HostEvent::Completion((1, OperationResult::Success(opened)));
        assert!(controls.host_event(completion));
        assert_eq!(controls.volume, Some(89));
        assert!(controls.running.is_empty() && controls.early.is_empty());
    }

    #[test]
    fn another_resource_is_not_mistaken_for_the_running_command() {
        let mut controls = granted();
        assert!(!controls.host_event(stdout(2, "Volume: 0.10\n")));
        assert!(!controls.host_event(exited(2)));
        assert_eq!(controls.running.len(), 1);
        assert!(
            controls.early.is_empty(),
            "every command's resource is known"
        );
        assert_eq!(controls.volume, None);
    }

    #[test]
    fn watchers_turn_system_changes_into_reads() {
        let mut controls = granted();
        watching(&mut controls, Watch::Audio, 7);
        watching(&mut controls, Watch::Brightness, 8);
        // A line may arrive in pieces; streams and their clients are ignored.
        controls.host_event(stdout(7, "Event 'change' on sink-input #91\nEvent 'chan"));
        assert!(controls.queue.is_empty());
        controls.host_event(stdout(7, "ge' on sink #56\n"));
        assert_eq!(controls.queue, [Job::ReadVolume]);
        controls.host_event(stdout(7, "Event 'change' on source #57\n"));
        controls.host_event(stdout(7, "Event 'change' on server #-1\n"));
        assert_eq!(controls.queue, [Job::ReadVolume, Job::ReadMicrophone]);
        controls.host_event(stdout(
            8,
            "/org/gnome/Shell/Brightness: org.gnome.Shell.Brightness.BrightnessChanged ()\n",
        ));
        assert_eq!(
            controls.queue,
            [Job::ReadVolume, Job::ReadMicrophone, Job::ReadBrightness]
        );
    }

    #[test]
    fn a_watcher_that_fails_at_once_is_given_up_until_a_touch() {
        let mut controls = granted();
        for _ in 0..WATCH_ATTEMPTS {
            watching(&mut controls, Watch::Audio, 7);
            controls.host_event(exited(7));
        }
        assert_eq!(
            controls.watch_failures[Watch::Audio.index()],
            WATCH_ATTEMPTS
        );
        controls.input(&InputEvent {
            item_id: "volume".into(),
            widget_id: VOLUME_BUTTON,
            kind: InputKind::Pressed,
            value: None,
            contact_id: Some(1),
        });
        assert_eq!(controls.watch_failures[Watch::Audio.index()], 0);

        // One that ran its minute is simply started again.
        controls.running.push(Running {
            task: Task::Watch(Watch::Audio),
            request: 200,
            resource: Some(20),
            stdout: Vec::new(),
            started: Instant::now() - Duration::from_secs(60),
        });
        controls.host_event(exited(20));
        assert_eq!(controls.watch_failures[Watch::Audio.index()], 0);
    }

    #[test]
    fn a_drag_queues_only_its_latest_level() {
        let mut controls = granted();
        for level in [10, 20, 30] {
            controls.enqueue(Job::SetVolume(level));
        }
        controls.enqueue(Job::ReadBrightness);
        controls.enqueue(Job::ReadBrightness);
        assert_eq!(
            controls.queue,
            [Job::SetVolume(30), Job::ReadBrightness],
            "one command runs at a time, so older levels are pointless"
        );
    }

    #[test]
    fn a_read_does_not_move_a_slider_being_set() {
        let mut controls = granted();
        controls.volume = Some(40);
        controls.dragging = Some(VOLUME_SLIDER);
        controls.apply_output(Job::ReadVolume, b"Volume: 0.89\n");
        assert_eq!(controls.volume, Some(40));

        controls.dragging = None;
        controls.enqueue(Job::SetVolume(40));
        controls.apply_output(Job::ReadVolume, b"Volume: 0.89\n");
        assert_eq!(controls.volume, Some(40));

        controls.queue.clear();
        controls.apply_output(Job::ReadVolume, b"Volume: 0.89\n");
        assert_eq!(controls.volume, Some(89));
    }

    #[test]
    fn the_grant_reads_every_level() {
        use touchbar_component_sdk::touchbar::plugin::broker::CapabilityState;

        // Hold the queue: pump would call into the host otherwise.
        let mut controls = Controls {
            running: granted().running,
            ..Controls::default()
        };
        let state = |status| {
            HostEvent::CapabilityChanged((
                1,
                CapabilityState {
                    capability: COMMAND.into(),
                    required: false,
                    status,
                },
            ))
        };
        assert!(controls.host_event(state(CapabilityStatus::Granted)));
        assert_eq!(
            controls.queue,
            [
                Job::ReadVolume,
                Job::ReadBrightness,
                Job::ReadMicrophone,
                Job::ReadPowerProfile
            ]
        );
        controls.host_event(state(CapabilityStatus::Denied));
        assert!(controls.queue.is_empty() && controls.running.is_empty());
    }

    #[test]
    fn a_muted_volume_shows_and_moving_it_unmutes() {
        let mut controls = granted();
        controls.apply_output(Job::ReadVolume, b"Volume: 0.40 [MUTED]\n");
        assert!(controls.volume_muted);
        assert_eq!(controls.volume, Some(40));
        let slide = InputEvent {
            item_id: "volume".into(),
            widget_id: VOLUME_SLIDER,
            kind: InputKind::ValueChanged,
            value: Some(0.6),
            contact_id: Some(1),
        };
        controls.input(&slide);
        assert!(!controls.volume_muted);
        assert_eq!(
            controls.queue,
            [Job::UnmuteVolume, Job::SetVolume(60)],
            "unmuted first, then set"
        );
        // A read that raced the unmute does not mute the control again.
        controls.apply_output(Job::ReadVolume, b"Volume: 0.60 [MUTED]\n");
        assert!(!controls.volume_muted);
    }

    #[test]
    fn touching_brightness_opens_its_slider_like_volume() {
        let mut controls = granted();
        let touch = |kind| InputEvent {
            item_id: "brightness".into(),
            widget_id: BRIGHTNESS_BUTTON,
            kind,
            value: None,
            contact_id: Some(1),
        };
        let opened = controls.input(&touch(InputKind::Activated));
        assert!(matches!(
            opened.presentation,
            Some(PresentationCommand::Begin(PresentationBegin {
                lifecycle: PresentationLifecycle::Persistent,
                ..
            }))
        ));
        assert!(controls.queue.is_empty(), "a touch no longer sets a level");
        let lifted = controls.input(&InputEvent {
            widget_id: BRIGHTNESS_SLIDER,
            ..touch(InputKind::Released)
        });
        assert!(matches!(
            lifted.presentation,
            Some(PresentationCommand::End(PresentationDismissal::Selection))
        ));
    }

    #[test]
    fn nothing_is_queued_without_the_command_grant() {
        let mut controls = Controls::default();
        controls.enqueue(Job::ReadVolume);
        assert!(controls.queue.is_empty());
        assert_eq!(controls.volume, None);
    }
}
