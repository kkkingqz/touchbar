use std::{os::fd::AsRawFd, path::PathBuf, time::Duration};
use touchbar_plugin_host::{
    Appearance, BrokerClient, HostLimits, InputActivation, InputEvent, InputKind,
    sets::ButtonSetHost,
};
use touchbar_protocol::broker_ipc::{
    ActivationOrigin, CapabilityState, HostMessage, Seqpacket, SupervisorMessage,
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
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/controls");
    let set =
        SetDocument::from_toml(&std::fs::read_to_string(root.join("sets/volume.toml")).unwrap())
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
#[test]
fn hiding_stops_own_requests_and_revocation_blocks_following_drag_write() {
    let (mut host, server) = open();
    let mut pending = vec![];
    while ready(&server) {
        let HostMessage::Request { request_id, .. } = server.recv_host().unwrap() else {
            panic!()
        };
        pending.push(request_id);
    }
    assert_eq!(pending.len(), 2, "one job and the audio watcher");
    let viewport = Rect::new(0.0, 0.0, 360.0, 60.0);
    host.render_at("volume", viewport, Appearance::default(), Duration::ZERO)
        .unwrap();
    let input = |kind, value, activation| InputEvent {
        item_id: "volume".into(),
        widget_id: 101,
        kind,
        value,
        contact_id: Some(9),
        activation,
    };
    host.handle_event(&input(InputKind::Pressed, None, None))
        .unwrap();
    host.handle_event(&input(InputKind::ValueChanged, Some(0.7), None))
        .unwrap();
    host.handle_visibility_changed("volume", false).unwrap();
    let mut cancelled = vec![];
    while ready(&server) {
        let HostMessage::Cancel {
            target_request_id, ..
        } = server.recv_host().unwrap()
        else {
            panic!("hide must only cancel outstanding requests");
        };
        cancelled.push(target_request_id);
    }
    pending.sort();
    cancelled.sort();
    assert_eq!(pending, cancelled);
    host.handle_visibility_changed("volume", true).unwrap();
    while ready(&server) {
        server.recv_host().unwrap();
    }
    host.render_at("volume", viewport, Appearance::default(), Duration::ZERO)
        .unwrap();
    host.handle_event(&input(InputKind::Pressed, None, None))
        .unwrap();
    server
        .send_supervisor(&SupervisorMessage::CapabilityChanged {
            generation: 2,
            state: CapabilityState {
                capability: "command.run.v1".into(),
                required: false,
                status: WireCapabilityStatus::Denied,
            },
        })
        .unwrap();
    host.dispatch_broker_events().unwrap();
    host.handle_event(&input(InputKind::ValueChanged, Some(0.9), None))
        .unwrap();
    host.handle_event(&input(
        InputKind::Activated,
        None,
        Some(InputActivation {
            origin: ActivationOrigin::Physical,
            input_sequence: 10,
        }),
    ))
    .unwrap();
    assert_eq!(host.snapshot().0["available"], StateValue::Bool(false));
    assert!(
        !ready(&server),
        "revocation must prevent subsequent commands"
    );
}
