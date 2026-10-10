use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use touchbar_component_sdk::kit::{InputEvent, InputKind};
use touchbar_component_sdk::touchbar::plugin::broker::CapabilityStatus;
use touchbar_component_sdk::{
    HostEvent, PresentationEvent, RenderRequest, RequestId, ResourceId, Update,
};
use touchbar_component_sdk::{LocalConnect, decode_local_connection_opened};
use touchbar_logic_sdk::{Guest, description_to_wit, engine::ENGINE, wire};
use touchbar_set_config::{ActionDescription, InputKind as LogicKind, LogicDescription, StateType};
struct Capture;
static RECORDING: AtomicBool = AtomicBool::new(false);
static ELAPSED: AtomicU32 = AtomicU32::new(0);
static REQUEST: AtomicU64 = AtomicU64::new(0);
static RESOURCE: AtomicU64 = AtomicU64::new(0);
static PENDING_ACTION: AtomicU32 = AtomicU32::new(0);

static LOCAL_AVAILABLE: AtomicBool = AtomicBool::new(false);

impl Guest for Capture {
    fn describe() -> wire::LogicDescription {
        let mut d = LogicDescription::default();
        d.state.insert("local_available".into(), StateType::Bool);
        d.state.insert("recording_label".into(), StateType::Text);
        d.state.insert("recording_text".into(), StateType::Text);
        d.state.insert("recording_color".into(), StateType::Text);
        d.state
            .insert("recording_background".into(), StateType::Text);
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
            "button-4".into(),
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
            "button-22".into(),
            ActionDescription {
                events: vec![LogicKind::Activated],
                value_type: None,
            },
        );
        d.state.insert("recording_pressed".into(), StateType::Text);
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
            let request = REQUEST.swap(0, Ordering::Relaxed);
            if request != 0 {
                let _ = touchbar_component_sdk::cancel(RequestId::from_raw(request));
            }
            let resource = RESOURCE.swap(0, Ordering::Relaxed);
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
        if let HostEvent::CapabilityChanged((_, cap)) = &event
            && cap.capability == "local.connect.v1"
        {
            LOCAL_AVAILABLE.store(cap.status == CapabilityStatus::Granted, Ordering::Relaxed);
        }
        old_host(event)?;
        Ok(update())
    }
}
fn patch() -> wire::StatePatch {
    #[allow(unused_imports)]
    use wire::StateValue::{Boolean as B, Number as N, Text as T};
    let recording = RECORDING.load(Ordering::Relaxed);
    vec![
        (
            "local_available".into(),
            B(LOCAL_AVAILABLE.load(Ordering::Relaxed)),
        ),
        (
            "recording_label".into(),
            T(if recording {
                "Stop recording"
            } else {
                "Start recording"
            }
            .into()),
        ),
        (
            "recording_text".into(),
            T(if recording { "● REC" } else { "RECORD" }.into()),
        ),
        (
            "recording_color".into(),
            T(if recording {
                "on-destructive"
            } else {
                "foreground"
            }
            .into()),
        ),
        (
            "recording_pressed".into(),
            T(if recording {
                "destructive"
            } else {
                "control-pressed"
            }
            .into()),
        ),
        (
            "recording_background".into(),
            T(if recording { "destructive" } else { "control" }.into()),
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
    if event.kind != InputKind::Activated {
        return Ok(Update {
            rerender: false,
            presentation: None,
        });
    }
    match event.widget_id {
        1..=4 => invoke(event.widget_id as u32),
        10 => {
            let was = RECORDING.fetch_xor(true, Ordering::Relaxed);
            invoke(if was { 11 } else { 10 });
            if was {
                ELAPSED.store(0, Ordering::Relaxed);
            }
        }
        20..=22 => invoke(event.widget_id as u32),
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
            if request_id == REQUEST.load(Ordering::Relaxed) =>
        {
            REQUEST.store(0, Ordering::Relaxed);
            if let Ok((resource, _)) = decode_local_connection_opened(&payload) {
                RESOURCE.store(resource.into_raw(), Ordering::Relaxed);
                REQUEST.store(0, Ordering::Relaxed);
                send_action(resource, PENDING_ACTION.swap(0, Ordering::Relaxed));
            }
        }
        HostEvent::ResourceEvent((resource, _, result))
            if resource == RESOURCE.load(Ordering::Relaxed) =>
        {
            if !matches!(result, OperationResult::Success(ref bytes) if !bytes.is_empty()) {
                RESOURCE.store(0, Ordering::Relaxed);
            }
        }
        HostEvent::Completion((request_id, OperationResult::Error(_)))
            if request_id == REQUEST.load(Ordering::Relaxed) =>
        {
            REQUEST.store(0, Ordering::Relaxed);
            PENDING_ACTION.store(0, Ordering::Relaxed);
        }
        _ => {}
    }
    Ok(Update {
        rerender: false,
        presentation: None,
    })
}
fn invoke(action: u32) {
    let resource = RESOURCE.load(Ordering::Relaxed);
    if resource != 0 {
        send_action(ResourceId::from_raw(resource), action);
        return;
    }
    PENDING_ACTION.store(action, Ordering::Relaxed);
    if REQUEST.load(Ordering::Relaxed) == 0
        && let Ok(request) = ENGINE.local().connect(&LocalConnect {
            endpoint: "capture".into(),
            protocol: "io.github.cameroncooper.touchbar.capture.v1".into(),
        })
    {
        REQUEST.store(request.into_raw(), Ordering::Relaxed);
    }
}

fn send_action(resource: ResourceId, action: u32) {
    if action == 0 {
        return;
    }
    let action = match action {
        1 => "screenshot.smart",
        2 => "screenshot.region",
        3 => "screenshot.window",
        4 => "screenshot.fullscreen",
        10 => "record.start",
        11 => "record.stop",
        20 => "ocr",
        21 => "qr",
        22 => "color",
        _ => return,
    };
    let _ = ENGINE
        .local()
        .send(resource, format!("{{\"action\":\"{action}\"}}").as_bytes());
}

touchbar_logic_sdk::export!(Capture);
