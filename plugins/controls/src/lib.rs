//! System controls: volume, display brightness, microphone, battery and
//! power profile.
//!
//! Levels are read from the system with `command.run.v1` (`wpctl`,
//! `brightnessctl`, `powerprofilesctl`) when the plugin starts and whenever
//! one of its controls is touched, so a slider starts where the system is
//! rather than at a guess. Only one command runs at a time: a drag queues
//! only its latest value, so the level lands where the finger stops.

use std::{cell::RefCell, collections::VecDeque};

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

    fn request(self) -> CommandRunRequest {
        let (command_id, values) = match self {
            Self::ReadVolume => ("get-volume", Vec::new()),
            Self::ReadBrightness => ("get-brightness", Vec::new()),
            Self::ReadMicrophone => ("get-microphone", Vec::new()),
            Self::ReadPowerProfile => ("get-power-profile", Vec::new()),
            Self::SetVolume(value) => ("set-volume", vec![percent_value(value)]),
            Self::SetBrightness(value) => ("set-brightness", vec![percent_value(value)]),
            Self::ToggleMicrophone => ("toggle-microphone", Vec::new()),
            Self::SetPowerProfile(value) => (
                "set-power-profile",
                vec![CommandValue::FixedEnum {
                    name: "profile".into(),
                    value: profile(value).into(),
                }],
            ),
        };
        CommandRunRequest {
            command_id: command_id.into(),
            values,
        }
    }
}

struct Running {
    job: Job,
    request: u64,
    resource: Option<u64>,
    stdout: Vec<u8>,
}

struct Controls {
    commands: bool,
    /// `None` until read; a control never shows a level it has not seen.
    volume: Option<u32>,
    brightness: Option<u32>,
    microphone_muted: bool,
    power_profile: u32,
    /// 0 = compact, 1 = persistent, 2 = transient.
    volume_presentation: u32,
    /// The slider a finger holds; reads do not move it meanwhile.
    dragging: Option<u64>,
    queue: VecDeque<Job>,
    running: Option<Running>,
}

impl Default for Controls {
    fn default() -> Self {
        Self {
            commands: false,
            volume: None,
            brightness: None,
            microphone_muted: false,
            power_profile: 1,
            volume_presentation: 0,
            dragging: None,
            queue: VecDeque::new(),
            running: None,
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
        CONTROLS.with_borrow(|controls| {
            Ok(match request.item_id.as_str() {
                "volume" => continuous_view(
                    VOLUME_BUTTON,
                    Icon::Volume,
                    "VOL",
                    controls.volume,
                    request.viewport.width,
                ),
                "brightness" => continuous_view(
                    BRIGHTNESS_BUTTON,
                    Icon::ChevronRight,
                    "LIGHT",
                    controls.brightness,
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
            CONTROLS.with_borrow_mut(|controls| controls.volume_presentation = 0);
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
            (VOLUME_BUTTON, InputKind::Pressed) => self.enqueue(Job::ReadVolume),
            (BRIGHTNESS_BUTTON, InputKind::Pressed) => self.enqueue(Job::ReadBrightness),
            (MICROPHONE_BUTTON, InputKind::Pressed) => self.enqueue(Job::ReadMicrophone),
            (POWER_BUTTON, InputKind::Pressed) => self.enqueue(Job::ReadPowerProfile),
            (VOLUME_BUTTON, InputKind::Activated) => {
                self.volume_presentation = 1;
                presentation = Some(PresentationCommand::Begin(PresentationBegin {
                    placement: PresentationPlacement::Anchored,
                    lifecycle: PresentationLifecycle::Persistent,
                    target: None,
                }));
            }
            (VOLUME_BUTTON, InputKind::LongPressed) => {
                self.volume_presentation = 2;
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
                if event.widget_id == VOLUME_SLIDER && self.volume_presentation != 0 {
                    presentation = Some(PresentationCommand::End(PresentationDismissal::Selection));
                }
            }
            (BRIGHTNESS_BUTTON, InputKind::Activated) => {
                let next = if self.brightness.is_some_and(|level| level <= 10) {
                    70
                } else {
                    10
                };
                self.brightness = Some(next);
                self.enqueue(Job::SetBrightness(next));
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
                    for job in [
                        Job::ReadVolume,
                        Job::ReadBrightness,
                        Job::ReadMicrophone,
                        Job::ReadPowerProfile,
                    ] {
                        self.enqueue(job);
                    }
                }
                if !granted {
                    self.queue.clear();
                    self.running = None;
                }
                true
            }
            HostEvent::Completion((request, result))
                if self
                    .running
                    .as_ref()
                    .is_some_and(|running| running.request == request) =>
            {
                let opened = match result {
                    OperationResult::Success(payload) => decode_command_opened(&payload).ok(),
                    OperationResult::Error(_) => None,
                };
                match (opened, &mut self.running) {
                    (Some(resource), Some(running)) => running.resource = Some(resource.into_raw()),
                    _ => self.running = None,
                }
                false
            }
            HostEvent::ResourceEvent((resource, _, result))
                if self
                    .running
                    .as_ref()
                    .is_some_and(|running| running.resource == Some(resource)) =>
            {
                let Some(running) = &mut self.running else {
                    return false;
                };
                let event = match result {
                    OperationResult::Success(payload) => decode_command_event(&payload).ok(),
                    OperationResult::Error(_) => None,
                };
                match event {
                    Some(CommandEvent::Stdout(bytes)) => {
                        let room = MAXIMUM_OUTPUT_BYTES.saturating_sub(running.stdout.len());
                        running.stdout.extend(bytes.into_iter().take(room));
                        false
                    }
                    Some(CommandEvent::Stderr(_)) => false,
                    Some(CommandEvent::Exited { exit_code, .. }) => {
                        let finished = self.running.take();
                        if let Some(finished) = finished
                            && exit_code == Some(0)
                        {
                            self.apply_output(finished.job, &finished.stdout);
                        }
                        true
                    }
                    None => {
                        self.running = None;
                        false
                    }
                }
            }
            HostEvent::Shutdown(_) => {
                self.queue.clear();
                self.running = None;
                false
            }
            _ => false,
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

    /// Starts the next job if none runs. Broker requests are accepted only
    /// in input and host-event callbacks, which is where this is called.
    fn pump(&mut self) {
        while self.running.is_none() {
            let Some(job) = self.queue.pop_front() else {
                return;
            };
            if let Ok(request) = command_run(&job.request()) {
                self.running = Some(Running {
                    job,
                    request: request.into_raw(),
                    resource: None,
                    stdout: Vec::new(),
                });
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
                if let Some((level, _)) = parse_wpctl_volume(&output) {
                    self.volume = Some(level);
                }
            }
            Job::ReadBrightness if !setting(BRIGHTNESS_SLIDER, Job::SetBrightness(0)) => {
                if let Some(level) = parse_brightnessctl(&output) {
                    self.brightness = Some(level);
                }
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

/// `brightnessctl -m` prints `device,class,current,percent%,maximum`. The
/// percentage is worked out from the raw values; brightnessctl's own one is
/// rounded down.
fn parse_brightnessctl(output: &str) -> Option<u32> {
    let fields = output.lines().next()?.split(',').collect::<Vec<_>>();
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

/// A level not read yet shows as a dash and an empty slider.
fn continuous_view(base: u64, icon: Icon, short: &str, value: Option<u32>, width: f32) -> View {
    let mut b = ViewBuilder::new();
    let shown = value.map_or_else(|| "–".to_owned(), |value| format!("{value}%"));
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
            selected: Some(value == Some(0)),
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
        assert_eq!(parse_power_profile("custom"), None);
    }

    fn granted() -> Controls {
        let mut controls = Controls {
            commands: true,
            ..Controls::default()
        };
        controls.running = Some(Running {
            job: Job::ReadVolume,
            request: 1,
            resource: Some(1),
            stdout: Vec::new(),
        });
        controls
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
        assert!(controls.queue.is_empty() && controls.running.is_none());
    }

    #[test]
    fn nothing_is_queued_without_the_command_grant() {
        let mut controls = Controls::default();
        controls.enqueue(Job::ReadVolume);
        assert!(controls.queue.is_empty());
        assert_eq!(controls.volume, None);
    }
}
