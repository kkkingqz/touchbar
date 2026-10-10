use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use touchbar_component_sdk::kit::{InputEvent, InputKind};
use touchbar_component_sdk::touchbar::plugin::broker::CapabilityStatus;
use touchbar_component_sdk::{CommandRunRequest, CommandValue};
use touchbar_component_sdk::{HostEvent, PresentationEvent, RenderRequest, Update};
use touchbar_logic_sdk::{Guest, description_to_wit, engine::ENGINE, wire};
use touchbar_set_config::{
    ActionDescription, InputKind as LogicKind, LogicDescription, ScalarType, StateType,
};
struct Hyprland;
static WORKSPACE: AtomicU32 = AtomicU32::new(1);
static DESTINATION: AtomicU32 = AtomicU32::new(2);

static COMMAND_AVAILABLE: AtomicBool = AtomicBool::new(false);

static WIDE: AtomicBool = AtomicBool::new(false);

impl Guest for Hyprland {
    fn describe() -> wire::LogicDescription {
        let mut d = LogicDescription::default();
        d.state.insert("command_available".into(), StateType::Bool);
        d.state.insert("wide".into(), StateType::Bool);
        d.state.insert("workspace_label".into(), StateType::Text);
        d.state.insert("workspace_value".into(), StateType::Number);
        d.state.insert("destination_label".into(), StateType::Text);
        d.state
            .insert("destination_value".into(), StateType::Number);
        d.actions.insert(
            "button-10".into(),
            ActionDescription {
                events: vec![LogicKind::ValueChanged],
                value_type: Some(ScalarType::Number),
            },
        );
        d.actions.insert(
            "button-11".into(),
            ActionDescription {
                events: vec![LogicKind::Activated],
                value_type: None,
            },
        );
        d.actions.insert(
            "button-20".into(),
            ActionDescription {
                events: vec![LogicKind::Activated],
                value_type: None,
            },
        );
        d.actions.insert(
            "button-21".into(),
            ActionDescription {
                events: vec![LogicKind::Activated],
                value_type: None,
            },
        );
        d.actions.insert(
            "button-30".into(),
            ActionDescription {
                events: vec![LogicKind::ValueChanged],
                value_type: Some(ScalarType::Number),
            },
        );
        d.actions.insert(
            "button-31".into(),
            ActionDescription {
                events: vec![LogicKind::Activated],
                value_type: None,
            },
        );
        description_to_wit(&d)
    }
    fn initialize(_item: String, _config: String) -> Result<wire::StatePatch, String> {
        Ok(patch())
    }
    fn handle_action(event: wire::ActionEvent) -> Result<wire::LogicUpdate, String> {
        let widget_id = event
            .action
            .strip_prefix("button-")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let value = event.value.and_then(|v| {
            if let wire::ScalarValue::Number(v) = v {
                Some(v as f32)
            } else {
                None
            }
        });
        old_event(InputEvent {
            item_id: String::new(),
            widget_id,
            kind: event.kind,
            value,
            contact_id: None,
        })?;
        Ok(update())
    }
    fn handle_context_changed(request: RenderRequest) -> Result<wire::LogicUpdate, String> {
        WIDE.store(request.viewport.width >= 120.0, Ordering::Relaxed);
        Ok(update())
    }
    fn handle_visibility_changed(_visible: bool) -> Result<wire::LogicUpdate, String> {
        Ok(update())
    }
    fn handle_presentation_event(_event: PresentationEvent) -> Result<wire::LogicUpdate, String> {
        Ok(update())
    }
    fn handle_host_event(event: HostEvent) -> Result<wire::LogicUpdate, String> {
        if let HostEvent::CapabilityChanged((_, cap)) = &event
            && cap.capability == "command.run.v1"
        {
            COMMAND_AVAILABLE.store(cap.status == CapabilityStatus::Granted, Ordering::Relaxed);
        }
        old_host(event)?;
        Ok(update())
    }
}
fn patch() -> wire::StatePatch {
    #[allow(unused_imports)]
    use wire::StateValue::{Boolean as B, Number as N, Text as T};
    let ws = WORKSPACE.load(Ordering::Relaxed);
    let dest = DESTINATION.load(Ordering::Relaxed);
    vec![
        (
            "command_available".into(),
            B(COMMAND_AVAILABLE.load(Ordering::Relaxed)),
        ),
        ("wide".into(), B(WIDE.load(Ordering::Relaxed))),
        ("workspace_label".into(), T(format!("WS {ws}"))),
        ("workspace_value".into(), N(f64::from(ws - 1) / 9.0)),
        ("destination_label".into(), T(format!("MOVE → {dest}"))),
        ("destination_value".into(), N(f64::from(dest - 1) / 9.0)),
    ]
}
fn update() -> wire::LogicUpdate {
    wire::LogicUpdate {
        state: patch(),
        presentation: None,
    }
}
fn old_event(event: InputEvent) -> Result<Update, String> {
    match (event.widget_id, event.kind) {
        (10, InputKind::ValueChanged) => {
            if let Some(value) = event.value {
                let workspace = from_slider(value);
                WORKSPACE.store(workspace, Ordering::Relaxed);
                run_workspace("workspace", workspace);
            }
        }
        (20, InputKind::Activated) => run("fullscreen", Vec::new()),
        (21, InputKind::Activated) => run("toggle-float", Vec::new()),
        (30, InputKind::ValueChanged) => {
            if let Some(value) = event.value {
                DESTINATION.store(from_slider(value), Ordering::Relaxed);
            }
        }
        (31, InputKind::Activated) => {
            run_workspace("move-window", DESTINATION.load(Ordering::Relaxed))
        }
        _ => {}
    }
    Ok(Update {
        rerender: true,
        presentation: None,
    })
}
fn old_host(_event: HostEvent) -> Result<Update, String> {
    Ok(Update {
        rerender: false,
        presentation: None,
    })
}
fn from_slider(value: f32) -> u32 {
    1 + (value.clamp(0.0, 1.0) * 9.0).round() as u32
}
fn run_workspace(command: &str, workspace: u32) {
    run(
        command,
        vec![CommandValue::Integer {
            name: "workspace".into(),
            value: i64::from(workspace),
        }],
    );
}
fn run(command_id: &str, values: Vec<CommandValue>) {
    let _ = ENGINE.commands().run(&CommandRunRequest {
        command_id: command_id.into(),
        values,
    });
}

touchbar_logic_sdk::export!(Hyprland);
