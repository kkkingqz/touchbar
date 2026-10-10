use std::{os::fd::AsRawFd, path::PathBuf, time::Duration};
use touchbar_plugin_host::{
    Appearance, BrokerClient, HostLimits, InputActivation, InputEvent, InputKind,
    sets::ButtonSetHost,
};
use touchbar_protocol::broker_ipc::{
    ActivationOrigin, BrokerResult, CapabilityState, HostMessage, Seqpacket, SupervisorMessage,
    WireCapabilityStatus,
};
use touchbar_set_config::{SetDocument, StateValue};
use touchbar_ui::Rect;
fn ready(server: &Seqpacket) -> bool {
    let mut fd = libc::pollfd {
        fd: server.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    (unsafe { libc::poll(&mut fd, 1, 100) }) == 1
}
fn open() -> (ButtonSetHost, Seqpacket) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/command-deck");
    let set =
        SetDocument::from_toml(&std::fs::read_to_string(root.join("sets/launchers.toml")).unwrap())
            .unwrap();
    let (client, server) = Seqpacket::pair().unwrap();
    let sender = server.try_clone().unwrap();
    let thread = std::thread::spawn(move || {
        let HostMessage::GetCapabilities { request_id } = sender.recv_host().unwrap() else {
            panic!()
        };
        sender
            .send_supervisor(&SupervisorMessage::Capabilities {
                request_id,
                generation: 1,
                states: vec![CapabilityState {
                    capability: "command.run.v1".into(),
                    required: false,
                    status: WireCapabilityStatus::Granted,
                }],
            })
            .unwrap();
    });
    let broker = BrokerClient::connect(client).unwrap();
    thread.join().unwrap();
    let bytes = std::fs::read(root.join("component/plugin.wasm")).unwrap();
    let mut host =
        ButtonSetHost::from_bytes_with_broker(&bytes, set, HostLimits::default(), Some(broker))
            .unwrap();
    host.announce_capabilities().unwrap();
    (host, server)
}
fn render(h: &mut ButtonSetHost) {
    h.render_at(
        "launchers",
        Rect::new(0.0, 0.0, 280.0, 60.0),
        Appearance::default(),
        Duration::ZERO,
    )
    .unwrap();
}
fn touch(h: &mut ButtonSetHost, kind: InputKind) {
    h.handle_event(&InputEvent {
        item_id: "launchers".into(),
        widget_id: 12,
        kind,
        value: None,
        contact_id: Some(9),
        activation: (kind == InputKind::Activated).then_some(InputActivation {
            origin: ActivationOrigin::Physical,
            input_sequence: 77,
        }),
    })
    .unwrap();
}
#[test]
fn command_completion_updates_only_own_set() {
    let (mut a, server) = open();
    let (mut b, other) = open();
    render(&mut a);
    render(&mut b);
    touch(&mut a, InputKind::Pressed);
    touch(&mut a, InputKind::Activated);
    touch(&mut a, InputKind::Released);
    let HostMessage::Request {
        request_id,
        capability,
        operation,
        ..
    } = server.recv_host().unwrap()
    else {
        panic!()
    };
    assert_eq!(capability, "command.run.v1");
    assert_eq!(operation, "run");
    assert_eq!(
        a.snapshot().0["probe_label"],
        StateValue::Text("RUN".into())
    );
    assert_eq!(
        b.snapshot().0["probe_label"],
        StateValue::Text("PROBE".into())
    );
    server
        .send_supervisor(&SupervisorMessage::Response {
            request_id,
            result: BrokerResult::Success {
                payload: touchbar_broker_schema::CommandOpened { resource_id: 50 }
                    .encode()
                    .unwrap(),
            },
        })
        .unwrap();
    a.dispatch_broker_events().unwrap();
    server
        .send_supervisor(&SupervisorMessage::ResourceEvent {
            resource_id: 50,
            sequence: 1,
            result: BrokerResult::Success {
                payload: touchbar_broker_schema::CommandEvent::Exited {
                    exit_code: Some(0),
                    signal: None,
                    stdout_bytes: 0,
                    stderr_bytes: 0,
                }
                .encode()
                .unwrap(),
            },
        })
        .unwrap();
    a.dispatch_broker_events().unwrap();
    assert_eq!(a.snapshot().0["probe_label"], StateValue::Text("OK".into()));
    assert_eq!(
        b.snapshot().0["probe_label"],
        StateValue::Text("PROBE".into())
    );
    assert!(!ready(&other));
}
#[test]
fn panel_close_cancels_pending_contact() {
    let (mut h, server) = open();
    render(&mut h);
    touch(&mut h, InputKind::Pressed);
    h.handle_presentation_event(touchbar_plugin_host::ComponentPresentationEvent::Ended(
        touchbar_plugin_host::ComponentPresentationEndReason::Requested,
    ))
    .unwrap();
    render(&mut h);
    touch(&mut h, InputKind::Activated);
    touch(&mut h, InputKind::Released);
    assert!(!ready(&server));
    assert_eq!(
        h.snapshot().0["probe_label"],
        StateValue::Text("PROBE".into())
    );
}
#[test]
fn original_probe_touch_coordinates_still_hit_probe() {
    let (mut host, _) = open();
    let bounds = Rect::new(0.0, 0.0, 240.0, 60.0);
    let mut ui = host
        .render("launchers", bounds, Appearance::default())
        .unwrap();
    let resolved = ui.resolve(bounds, touchbar_ui::Theme::default());

    let mut interactions = touchbar_ui::InteractionState::default();
    let events = interactions.handle(
        &resolved.interactions,
        touchbar_ui::Contact {
            id: 1,
            phase: touchbar_ui::ContactPhase::Down,
            position: touchbar_ui::Point::new(200.0, 30.0),
            time: Duration::ZERO,
        },
    );
    assert!(
        events
            .iter()
            .any(|e| matches!(e,touchbar_ui::UiEvent::Pressed{id} if id.0==12)),
        "{events:?}"
    );
}
