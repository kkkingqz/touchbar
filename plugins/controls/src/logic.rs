//! System controls: volume, display brightness, microphone, battery and
//! power profile.
//!
//! Levels are read from the system with `command.run.v1` (`wpctl`,
//! `brightnessctl`, `powerprofilesctl`) when the plugin starts, whenever one
//! of its controls is touched or comes on the bar unwatched, and whenever
//! the system reports a change: `pactl subscribe` for the volume and
//! microphone, GNOME Shell's `BrightnessChanged` signal for the display.
//! Those watchers start when their item is first shown, stop at the command
//! time limit and start again. Only one read or write runs at a
//! time: a drag queues only its latest value, so the level lands where the
//! finger stops.

use std::{
    cell::RefCell,
    collections::{BTreeMap, VecDeque},
    time::{Duration, Instant},
};
#[cfg(test)]
use touchbar_component_sdk::CommandValue;
use touchbar_component_sdk::kit::{InputEvent, InputKind};
use touchbar_component_sdk::{
    CommandEvent, CommandRunRequest, HostEvent, PresentationEvent, RenderRequest, Update,
    decode_command_event, decode_command_opened,
    touchbar::plugin::{
        broker::{CapabilityStatus, OperationResult},
        ui::{
            PresentationBegin, PresentationCommand, PresentationDismissal, PresentationLifecycle,
            PresentationPlacement,
        },
    },
};
use touchbar_logic_sdk::{
    Guest,
    controls::{ControlOperation, PowerProfile},
    description_to_wit,
    engine::ENGINE,
    wire,
};
use touchbar_set_config::{
    ActionDescription, InputKind as LogicKind, LogicDescription, ScalarType, StateType,
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
    static ITEM: RefCell<String> = const { RefCell::new(String::new()) };
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
        let operation = match self {
            Self::ReadVolume => ControlOperation::ReadVolume,
            Self::ReadBrightness => ControlOperation::ReadBrightness,
            Self::ReadMicrophone => ControlOperation::ReadMicrophone,
            Self::ReadPowerProfile => ControlOperation::ReadPowerProfile,
            Self::SetVolume(value) => ControlOperation::SetVolume(value),
            Self::UnmuteVolume => ControlOperation::UnmuteVolume,
            Self::SetBrightness(percent) => ControlOperation::SetBrightness {
                device: brightness_device?.into(),
                percent,
            },
            Self::ToggleMicrophone => ControlOperation::ToggleMicrophone,
            Self::SetPowerProfile(value) => ControlOperation::SetPowerProfile(match value {
                0 => PowerProfile::PowerSaver,
                2 => PowerProfile::Performance,
                _ => PowerProfile::Balanced,
            }),
        };
        operation.request().ok()
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
        match self {
            Self::Audio => ControlOperation::WatchAudio,
            Self::Brightness => ControlOperation::WatchBrightness,
        }
        .request()
        .expect("watch request has no values")
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
    visible: bool,
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
            visible: true,
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

fn patch(controls: &Controls) -> wire::StatePatch {
    let shown = |value: Option<u32>, muted: bool| {
        if muted {
            "MUTED".into()
        } else {
            value.map(|v| format!("{v}%")).unwrap_or_else(|| "–".into())
        }
    };
    let volume = shown(controls.volume, controls.volume_muted);
    let brightness = shown(controls.brightness, false);
    vec![
        (
            "available".into(),
            wire::StateValue::Boolean(controls.commands),
        ),
        (
            "volume".into(),
            wire::StateValue::Number(f64::from(controls.volume.unwrap_or(0)) / 100.0),
        ),
        (
            "brightness".into(),
            wire::StateValue::Number(f64::from(controls.brightness.unwrap_or(0)) / 100.0),
        ),
        ("volume_text".into(), wire::StateValue::Text(volume.clone())),
        (
            "volume_label".into(),
            wire::StateValue::Text(format!("VOL {volume}")),
        ),
        (
            "brightness_text".into(),
            wire::StateValue::Text(brightness.clone()),
        ),
        (
            "brightness_label".into(),
            wire::StateValue::Text(format!("LIGHT {brightness}")),
        ),
        (
            "volume_icon".into(),
            wire::StateValue::Text(
                if controls.volume_muted {
                    "muted"
                } else {
                    "volume"
                }
                .into(),
            ),
        ),
        (
            "volume_selected".into(),
            wire::StateValue::Boolean(controls.volume_muted || controls.volume == Some(0)),
        ),
        (
            "microphone_icon".into(),
            wire::StateValue::Text(
                if controls.microphone_muted {
                    "muted"
                } else {
                    "volume"
                }
                .into(),
            ),
        ),
        (
            "microphone_text".into(),
            wire::StateValue::Text(
                if controls.microphone_muted {
                    "MUTED"
                } else {
                    "MIC"
                }
                .into(),
            ),
        ),
        (
            "microphone_muted".into(),
            wire::StateValue::Boolean(controls.microphone_muted),
        ),
        (
            "power_text".into(),
            wire::StateValue::Text(profile(controls.power_profile).to_uppercase()),
        ),
    ]
}
fn action_widget(action: &str) -> Option<u64> {
    match action {
        "volume" => Some(VOLUME_BUTTON),
        "volume-level" => Some(VOLUME_SLIDER),
        "brightness" => Some(BRIGHTNESS_BUTTON),
        "brightness-level" => Some(BRIGHTNESS_SLIDER),
        "microphone" => Some(MICROPHONE_BUTTON),
        "power-profile" => Some(POWER_BUTTON),
        _ => None,
    }
}
impl Guest for Plugin {
    fn describe() -> wire::LogicDescription {
        let actions = ["volume", "brightness", "microphone", "power-profile"]
            .into_iter()
            .map(|name| {
                (
                    name.into(),
                    ActionDescription {
                        events: vec![
                            LogicKind::Pressed,
                            LogicKind::Activated,
                            LogicKind::LongPressed,
                        ],
                        value_type: None,
                    },
                )
            })
            .chain(
                ["volume-level", "brightness-level"]
                    .into_iter()
                    .map(|name| {
                        (
                            name.into(),
                            ActionDescription {
                                events: vec![
                                    LogicKind::Pressed,
                                    LogicKind::ValueChanged,
                                    LogicKind::Activated,
                                    LogicKind::Released,
                                    LogicKind::Cancelled,
                                ],
                                value_type: Some(ScalarType::Number),
                            },
                        )
                    }),
            )
            .collect();
        let state = [
            ("available", StateType::Bool),
            ("volume", StateType::Number),
            ("brightness", StateType::Number),
            ("volume_text", StateType::Text),
            ("volume_label", StateType::Text),
            ("brightness_text", StateType::Text),
            ("brightness_label", StateType::Text),
            ("volume_icon", StateType::Text),
            ("volume_selected", StateType::Bool),
            ("microphone_icon", StateType::Text),
            ("microphone_text", StateType::Text),
            ("microphone_muted", StateType::Bool),
            ("power_text", StateType::Text),
        ]
        .into_iter()
        .map(|(name, ty)| (name.into(), ty))
        .collect::<BTreeMap<_, _>>();
        description_to_wit(&LogicDescription { actions, state })
    }
    fn initialize(item: String, config: String) -> Result<wire::StatePatch, String> {
        if !matches!(
            config.as_str(),
            "volume" | "brightness" | "microphone" | "battery" | "power-profile"
        ) {
            return Err("unknown controls configuration".into());
        }
        ITEM.set(item);
        CONTROLS.set(Controls::default());
        CONTROLS.with_borrow_mut(|controls| {
            controls.want(&config);
            Ok(patch(controls))
        })
    }
    fn handle_action(event: wire::ActionEvent) -> Result<wire::LogicUpdate, String> {
        let widget =
            action_widget(&event.action).ok_or_else(|| "unknown controls action".to_owned())?;
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
        CONTROLS.with_borrow_mut(|controls| {
            let update = controls.input(&input);
            controls.pump();
            Ok(wire::LogicUpdate {
                state: patch(controls),
                presentation: update.presentation,
            })
        })
    }
    fn handle_host_event(event: HostEvent) -> Result<wire::LogicUpdate, String> {
        CONTROLS.with_borrow_mut(|controls| {
            controls.host_event(event);
            controls.pump();
            Ok(wire::LogicUpdate {
                state: patch(controls),
                presentation: None,
            })
        })
    }
    fn handle_context_changed(_request: RenderRequest) -> Result<wire::LogicUpdate, String> {
        Ok(wire::LogicUpdate {
            state: vec![],
            presentation: None,
        })
    }
    fn handle_visibility_changed(visible: bool) -> Result<wire::LogicUpdate, String> {
        CONTROLS.with_borrow_mut(|controls| {
            controls.visible = visible;
            if visible {
                ITEM.with_borrow(|item| {
                    controls.want(item);
                    controls.shown(item);
                });
                controls.pump();
            } else {
                controls.hidden();
            }
            Ok(wire::LogicUpdate {
                state: patch(controls),
                presentation: None,
            })
        })
    }
    fn handle_presentation_event(event: PresentationEvent) -> Result<wire::LogicUpdate, String> {
        CONTROLS.with_borrow_mut(|controls| {
            if matches!(event, PresentationEvent::Ended(_)) {
                controls.presentation = 0;
            }
            Ok(wire::LogicUpdate {
                state: patch(controls),
                presentation: None,
            })
        })
    }
}

impl Controls {
    fn want(&mut self, item: &str) {
        match item {
            "volume" | "microphone" => self.wanted[Watch::Audio.index()] = true,
            "brightness" => self.wanted[Watch::Brightness.index()] = true,
            _ => {}
        }
    }
    fn hidden(&mut self) {
        for running in &self.running {
            if let Some(resource) = running.resource {
                let _ = touchbar_component_sdk::close(
                    touchbar_component_sdk::ResourceId::from_raw(resource),
                );
            } else {
                let _ = touchbar_component_sdk::cancel(
                    touchbar_component_sdk::RequestId::from_raw(running.request),
                );
            }
        }
        self.queue.clear();
        self.running.clear();
        self.early.clear();
        self.wanted = [false; 2];
        self.dragging = None;
        self.presentation = 0;
    }
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
                if event.kind == InputKind::Cancelled {
                    self.queue.retain(|job| {
                        !matches!(
                            (event.widget_id, job),
                            (VOLUME_SLIDER, Job::SetVolume(_) | Job::UnmuteVolume)
                                | (BRIGHTNESS_SLIDER, Job::SetBrightness(_))
                        )
                    });
                    self.enqueue(if event.widget_id == VOLUME_SLIDER {
                        Job::ReadVolume
                    } else {
                        Job::ReadBrightness
                    });
                }
                if self.presentation != 0 && event.kind == InputKind::Released {
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

    /// An item comes on the bar showing its last level, which may be stale
    /// if nothing watched it: read it again. The pump that follows starts
    /// the watcher of an item hidden at startup, which first renders now.
    fn shown(&mut self, item: &str) {
        let (watch, job) = match item {
            "volume" => (Some(Watch::Audio), Job::ReadVolume),
            "microphone" => (Some(Watch::Audio), Job::ReadMicrophone),
            "brightness" => (Some(Watch::Brightness), Job::ReadBrightness),
            "power-profile" => (None, Job::ReadPowerProfile),
            _ => return,
        };
        if !watch.is_some_and(|watch| self.watching(watch)) {
            self.enqueue(job);
        }
    }

    fn watching(&self, watch: Watch) -> bool {
        self.running
            .iter()
            .any(|running| running.task == Task::Watch(watch))
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
    /// not running. Broker requests are accepted only in input, host-event
    /// and visibility callbacks, which is where this is called.
    fn pump(&mut self) {
        self.running.retain(|running| {
            let limit = match running.task {
                Task::Job(_) => ABANDON_AFTER,
                Task::Watch(_) => ABANDON_WATCH_AFTER,
            };
            running.started.elapsed() < limit
        });
        if !self.commands || !self.visible {
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
            if self.wanted[watch.index()]
                && !self.watching(watch)
                && self.watch_failures[watch.index()] < WATCH_ATTEMPTS
            {
                self.start(Task::Watch(watch), &watch.request());
            }
        }
    }

    fn start(&mut self, task: Task, request: &CommandRunRequest) {
        let _ = request;
        let submitted = match task {
            Task::Job(Job::ReadVolume) => ENGINE.audio().read_volume(),
            Task::Job(Job::SetVolume(value)) => ENGINE.audio().set_volume(value),
            Task::Job(Job::UnmuteVolume) => ENGINE.audio().unmute(),
            Task::Job(Job::ReadMicrophone) => ENGINE.audio().read_microphone(),
            Task::Job(Job::ToggleMicrophone) => ENGINE.audio().toggle_microphone(),
            Task::Job(Job::ReadBrightness) => ENGINE.display().read_brightness(),
            Task::Job(Job::SetBrightness(value)) => ENGINE
                .display()
                .set_brightness(self.brightness_device.as_deref().unwrap_or(""), value),
            Task::Job(Job::ReadPowerProfile) => ENGINE.power().read_profile(),
            Task::Job(Job::SetPowerProfile(value)) => ENGINE.power().set_profile(match value {
                0 => PowerProfile::PowerSaver,
                2 => PowerProfile::Performance,
                _ => PowerProfile::Balanced,
            }),
            Task::Watch(Watch::Audio) => ENGINE.audio().watch(),
            Task::Watch(Watch::Brightness) => ENGINE.display().watch(),
        };
        match submitted {
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

fn percent(value: f32) -> u32 {
    (value.clamp(0.0, 1.0) * 100.0).round() as u32
}

#[cfg(test)]
fn percent_value(value: u32) -> CommandValue {
    CommandValue::Text {
        name: "percent".into(),
        value: format!("{value}%"),
    }
}

#[cfg(test)]
use touchbar_logic_sdk::controls::parse_brightness as parse_brightnessctl;
use touchbar_logic_sdk::controls::{display_backlight, parse_volume as parse_wpctl_volume};
fn parse_power_profile(output: &str) -> Option<u32> {
    touchbar_logic_sdk::controls::parse_power_profile(output).map(|p| match p {
        PowerProfile::PowerSaver => 0,
        PowerProfile::Balanced => 1,
        PowerProfile::Performance => 2,
    })
}
fn profile(value: u32) -> &'static str {
    match value {
        0 => "power-saver",
        2 => "performance",
        _ => "balanced",
    }
}

touchbar_logic_sdk::export!(Plugin);

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
    fn a_shown_item_reads_its_level_unless_watched() {
        let mut controls = granted();
        controls.shown("volume");
        controls.shown("power-profile");
        controls.shown("battery");
        assert_eq!(controls.queue, [Job::ReadVolume, Job::ReadPowerProfile]);
        controls.queue.clear();
        watching(&mut controls, Watch::Audio, 7);
        controls.shown("volume");
        controls.shown("microphone");
        controls.shown("brightness");
        assert_eq!(controls.queue, [Job::ReadBrightness]);
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
    #[test]
    fn cancelled_drag_discards_unsent_value() {
        let mut controls = granted();
        controls.dragging = Some(VOLUME_SLIDER);
        controls.enqueue(Job::SetVolume(70));
        controls.input(&InputEvent {
            item_id: "volume".into(),
            widget_id: VOLUME_SLIDER,
            kind: InputKind::Cancelled,
            value: None,
            contact_id: Some(1),
        });
        assert!(
            !controls
                .queue
                .iter()
                .any(|job| matches!(job, Job::SetVolume(_))),
            "cancel must not commit the queued value"
        );
    }
    #[test]
    fn revocation_and_early_events_are_instance_local() {
        use touchbar_component_sdk::touchbar::plugin::broker::CapabilityState;
        let mut a = granted();
        let mut b = granted();
        a.running[0].resource = None;
        for resource in 2..50 {
            a.host_event(stdout(resource, "Volume: 0.7\n"));
        }
        assert_eq!(a.early.len(), MAXIMUM_EARLY_EVENTS);
        assert!(b.early.is_empty());
        a.enqueue(Job::SetVolume(70));
        a.host_event(HostEvent::CapabilityChanged((
            2,
            CapabilityState {
                capability: COMMAND.into(),
                required: false,
                status: CapabilityStatus::Denied,
            },
        )));
        a.enqueue(Job::SetVolume(80));
        assert!(a.queue.is_empty());
        assert!(a.running.is_empty());
        assert!(a.early.is_empty());
        b.enqueue(Job::SetVolume(20));
        assert_eq!(b.queue, [Job::SetVolume(20)]);
    }
    #[test]
    fn declarative_controls_bind_to_shared_controller() {
        let description = touchbar_logic_sdk::description_from_wit(Plugin::describe()).unwrap();
        for source in [
            include_str!("../sets/volume.toml"),
            include_str!("../sets/brightness.toml"),
            include_str!("../sets/microphone.toml"),
            include_str!("../sets/battery.toml"),
            include_str!("../sets/power-profile.toml"),
        ] {
            let set = touchbar_set_config::SetDocument::from_toml(source).unwrap();
            set.validate_bindings(&description).unwrap();
            if set.id == "volume" {
                assert_eq!(set.panels[0].preferred_width, 360);
                assert_eq!(set.panels[0].principal_item.as_deref(), Some("volume"));
            }
        }
    }
    #[test]
    fn hiding_controls_clears_watch_intent_and_queues() {
        let mut controls = Controls {
            wanted: [true, true],
            ..Default::default()
        };
        controls.queue.push_back(Job::ReadVolume);
        controls.dragging = Some(VOLUME_SLIDER);
        controls.hidden();
        assert_eq!(controls.wanted, [false, false]);
        assert!(controls.running.is_empty() && controls.queue.is_empty());
        assert_eq!(controls.dragging, None);
    }
}
