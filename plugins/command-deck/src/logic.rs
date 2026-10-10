use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use touchbar_component_sdk::kit::{InputEvent, InputKind};
use touchbar_component_sdk::touchbar::plugin::broker::CapabilityStatus;
use touchbar_component_sdk::{
    CommandEvent, CommandRunRequest, LocalConnect, decode_command_event, decode_command_opened,
    decode_local_connection_opened, decode_local_frame,
};
use touchbar_component_sdk::{
    HostEvent, PresentationEvent, RenderRequest, RequestId, ResourceId, Update,
};
use touchbar_logic_sdk::{Guest, description_to_wit, engine::ENGINE, wire};
use touchbar_set_config::{ActionDescription, InputKind as LogicKind, LogicDescription, StateType};
struct CommandDeck;
static CUSTOM_REQUEST: AtomicU64 = AtomicU64::new(0);
static CUSTOM_RESOURCE: AtomicU64 = AtomicU64::new(0);
static CUSTOM_RESPONSE_BYTES: AtomicU64 = AtomicU64::new(0);
static PENDING_ACTION: AtomicU32 = AtomicU32::new(0);
static PROBE_REQUEST: AtomicU64 = AtomicU64::new(0);
static PROBE_RESOURCE: AtomicU64 = AtomicU64::new(0);
static PROBE_STATE: AtomicU32 = AtomicU32::new(PROBE_IDLE);

const PROBE_IDLE: u32 = 0;
const PROBE_PENDING: u32 = 1;
const PROBE_RUNNING: u32 = 2;
const PROBE_SUCCEEDED: u32 = 3;
const PROBE_FAILED: u32 = 4;

static COMMAND_AVAILABLE: AtomicBool = AtomicBool::new(false);
static LOCAL_AVAILABLE: AtomicBool = AtomicBool::new(false);

impl Guest for CommandDeck {
    fn describe() -> wire::LogicDescription {
        let mut d = LogicDescription::default();
        d.state.insert("command_available".into(), StateType::Bool);
        d.state.insert("local_available".into(), StateType::Bool);
        d.state.insert("probe_label".into(), StateType::Text);
        d.state.insert("response_label".into(), StateType::Text);
        d.actions.insert(
            "button-1".into(),
            ActionDescription {
                events: vec![LogicKind::Activated],
                value_type: None,
            },
        );
        d.actions.insert(
            "button-2".into(),
            ActionDescription {
                events: vec![LogicKind::Activated],
                value_type: None,
            },
        );
        d.actions.insert(
            "button-3".into(),
            ActionDescription {
                events: vec![LogicKind::Activated],
                value_type: None,
            },
        );
        d.actions.insert(
            "button-10".into(),
            ActionDescription {
                events: vec![LogicKind::Activated],
                value_type: None,
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
            "button-12".into(),
            ActionDescription {
                events: vec![LogicKind::Activated],
                value_type: None,
            },
        );
        d.actions.insert(
            "button-20".into(),
            ActionDescription {
                events: vec![LogicKind::LongPressed],
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
            "button-100".into(),
            ActionDescription {
                events: vec![LogicKind::Activated],
                value_type: None,
            },
        );
        d.actions.insert(
            "button-101".into(),
            ActionDescription {
                events: vec![LogicKind::Activated],
                value_type: None,
            },
        );
        d.actions.insert(
            "button-102".into(),
            ActionDescription {
                events: vec![LogicKind::Activated],
                value_type: None,
            },
        );
        d.actions.insert(
            "button-103".into(),
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
    fn handle_context_changed(_request: RenderRequest) -> Result<wire::LogicUpdate, String> {
        Ok(update())
    }
    fn handle_visibility_changed(visible: bool) -> Result<wire::LogicUpdate, String> {
        if !visible {
            let request = CUSTOM_REQUEST.swap(0, Ordering::Relaxed);
            if request != 0 {
                let _ = touchbar_component_sdk::cancel(RequestId::from_raw(request));
            }
            let request = PROBE_REQUEST.swap(0, Ordering::Relaxed);
            if request != 0 {
                let _ = touchbar_component_sdk::cancel(RequestId::from_raw(request));
            }
            let resource = CUSTOM_RESOURCE.swap(0, Ordering::Relaxed);
            if resource != 0 {
                let _ = touchbar_component_sdk::close(ResourceId::from_raw(resource));
            }
            let resource = PROBE_RESOURCE.swap(0, Ordering::Relaxed);
            if resource != 0 {
                let _ = touchbar_component_sdk::close(ResourceId::from_raw(resource));
            }
            PENDING_ACTION.store(0, Ordering::Relaxed);
        }
        Ok(update())
    }
    fn handle_presentation_event(_event: PresentationEvent) -> Result<wire::LogicUpdate, String> {
        Ok(update())
    }
    fn handle_host_event(event: HostEvent) -> Result<wire::LogicUpdate, String> {
        if let HostEvent::CapabilityChanged((_, cap)) = &event {
            if cap.capability == "command.run.v1" {
                COMMAND_AVAILABLE.store(cap.status == CapabilityStatus::Granted, Ordering::Relaxed);
            }
            if cap.capability == "local.connect.v1" {
                LOCAL_AVAILABLE.store(cap.status == CapabilityStatus::Granted, Ordering::Relaxed);
            }
        }
        old_host(event)?;
        Ok(update())
    }
}
fn patch() -> wire::StatePatch {
    #[allow(unused_imports)]
    use wire::StateValue::{Boolean as B, Number as N, Text as T};
    let probe = match PROBE_STATE.load(Ordering::Relaxed) {
        PROBE_PENDING | PROBE_RUNNING => "RUN",
        PROBE_SUCCEEDED => "OK",
        PROBE_FAILED => "ERR",
        _ => "PROBE",
    };
    let bytes = CUSTOM_RESPONSE_BYTES.load(Ordering::Relaxed);
    vec![
        (
            "command_available".into(),
            B(COMMAND_AVAILABLE.load(Ordering::Relaxed)),
        ),
        (
            "local_available".into(),
            B(LOCAL_AVAILABLE.load(Ordering::Relaxed)),
        ),
        ("probe_label".into(), T(probe.into())),
        (
            "response_label".into(),
            T(if bytes == u64::MAX {
                "ERR".into()
            } else if bytes == 0 {
                "1".into()
            } else {
                format!("OK {bytes}B")
            }),
        ),
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
        (1..=3, InputKind::Activated) => invoke_custom(event.widget_id as u32),
        (10, InputKind::Activated) => run("terminal", Vec::new()),
        (11, InputKind::Activated) => invoke_custom(11),
        (12, InputKind::Activated) => run_probe(),
        (20, InputKind::LongPressed) => run("lock", Vec::new()),
        (21, InputKind::Activated) => invoke_custom(21),
        (100..=103, InputKind::Activated) => invoke_custom((event.widget_id - 99) as u32),
        _ => {}
    }
    Ok(Update {
        rerender: true,
        presentation: None,
    })
}
fn old_host(event: HostEvent) -> Result<Update, String> {
    use touchbar_component_sdk::touchbar::plugin::broker::OperationResult;
    match event {
        HostEvent::Completion((request_id, OperationResult::Success(payload)))
            if request_id == PROBE_REQUEST.load(Ordering::Relaxed) =>
        {
            PROBE_REQUEST.store(0, Ordering::Relaxed);
            match decode_command_opened(&payload) {
                Ok(resource) => {
                    PROBE_RESOURCE.store(resource.into_raw(), Ordering::Relaxed);
                    PROBE_STATE.store(PROBE_RUNNING, Ordering::Relaxed);
                }
                Err(_) => PROBE_STATE.store(PROBE_FAILED, Ordering::Relaxed),
            }
        }
        HostEvent::Completion((request_id, OperationResult::Error(_)))
            if request_id == PROBE_REQUEST.load(Ordering::Relaxed) =>
        {
            PROBE_REQUEST.store(0, Ordering::Relaxed);
            PROBE_STATE.store(PROBE_FAILED, Ordering::Relaxed);
        }
        HostEvent::ResourceEvent((resource, _, OperationResult::Success(payload)))
            if resource == PROBE_RESOURCE.load(Ordering::Relaxed) =>
        {
            if let Ok(CommandEvent::Exited {
                exit_code, signal, ..
            }) = decode_command_event(&payload)
            {
                PROBE_RESOURCE.store(0, Ordering::Relaxed);
                PROBE_STATE.store(
                    if exit_code == Some(0) && signal.is_none() {
                        PROBE_SUCCEEDED
                    } else {
                        PROBE_FAILED
                    },
                    Ordering::Relaxed,
                );
            }
        }
        HostEvent::ResourceEvent((resource, _, OperationResult::Error(_)))
            if resource == PROBE_RESOURCE.load(Ordering::Relaxed) =>
        {
            PROBE_RESOURCE.store(0, Ordering::Relaxed);
            PROBE_STATE.store(PROBE_FAILED, Ordering::Relaxed);
        }
        HostEvent::Completion((request_id, OperationResult::Success(payload)))
            if request_id == CUSTOM_REQUEST.load(Ordering::Relaxed) =>
        {
            CUSTOM_REQUEST.store(0, Ordering::Relaxed);
            if let Ok((resource, _)) = decode_local_connection_opened(&payload) {
                CUSTOM_RESOURCE.store(resource.into_raw(), Ordering::Relaxed);
                CUSTOM_REQUEST.store(0, Ordering::Relaxed);
                send_custom(resource, PENDING_ACTION.swap(0, Ordering::Relaxed));
            }
        }
        HostEvent::ResourceEvent((resource, _, OperationResult::Success(payload)))
            if resource == CUSTOM_RESOURCE.load(Ordering::Relaxed) =>
        {
            if payload.is_empty() {
                CUSTOM_RESOURCE.store(0, Ordering::Relaxed);
            } else if let Ok(frame) = decode_local_frame(&payload) {
                CUSTOM_RESPONSE_BYTES.store(frame.bytes.len() as u64, Ordering::Relaxed);
            }
        }
        HostEvent::ResourceEvent((resource, _, OperationResult::Error(_)))
            if resource == CUSTOM_RESOURCE.load(Ordering::Relaxed) =>
        {
            CUSTOM_RESOURCE.store(0, Ordering::Relaxed);
            CUSTOM_RESPONSE_BYTES.store(u64::MAX, Ordering::Relaxed);
        }
        HostEvent::Completion((request_id, OperationResult::Error(_)))
            if request_id == CUSTOM_REQUEST.load(Ordering::Relaxed) =>
        {
            CUSTOM_REQUEST.store(0, Ordering::Relaxed);
            PENDING_ACTION.store(0, Ordering::Relaxed);
        }
        _ => {}
    }
    Ok(Update {
        rerender: true,
        presentation: None,
    })
}
fn run(command_id: &str, values: Vec<touchbar_component_sdk::CommandValue>) {
    let _ = ENGINE.commands().run(&CommandRunRequest {
        command_id: command_id.into(),
        values,
    });
}
fn run_probe() {
    if PROBE_REQUEST.load(Ordering::Relaxed) != 0 || PROBE_RESOURCE.load(Ordering::Relaxed) != 0 {
        return;
    }
    PROBE_STATE.store(PROBE_PENDING, Ordering::Relaxed);
    match ENGINE.commands().run(&CommandRunRequest {
        command_id: "probe".into(),
        values: Vec::new(),
    }) {
        Ok(request) => PROBE_REQUEST.store(request.into_raw(), Ordering::Relaxed),
        Err(_) => PROBE_STATE.store(PROBE_FAILED, Ordering::Relaxed),
    }
}
fn invoke_custom(action: u32) {
    let resource = CUSTOM_RESOURCE.load(Ordering::Relaxed);
    if resource != 0 {
        send_custom(ResourceId::from_raw(resource), action);
        return;
    }
    PENDING_ACTION.store(action, Ordering::Relaxed);
    if CUSTOM_REQUEST.load(Ordering::Relaxed) == 0
        && let Ok(request) = ENGINE.local().connect(&LocalConnect {
            endpoint: "command-deck".into(),
            protocol: "io.github.cameroncooper.touchbar.command-deck.v1".into(),
        })
    {
        CUSTOM_REQUEST.store(request.into_raw(), Ordering::Relaxed);
    }
}
fn send_custom(resource: ResourceId, action: u32) {
    if action != 0 {
        let _ = ENGINE
            .local()
            .send(resource, format!("{{\"action\":{action}}}").as_bytes());
    }
}

touchbar_logic_sdk::export!(CommandDeck);
