use std::{os::fd::AsRawFd, path::PathBuf, time::Duration};
use touchbar_broker_schema::{
    MediaAction, MediaCommand, MediaPlaybackStatus, MediaSession, MediaState,
    MediaSubscriptionOpened,
};
use touchbar_plugin_host::{
    Appearance, BrokerClient, HostLimits, InputActivation, InputEvent, InputKind, PackageAssets,
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
fn state(track: &str) -> MediaState {
    MediaState {
        generation: 1,
        session: Some(MediaSession {
            player: "org.mpris.MediaPlayer2.vlc".into(),
            identity: "VLC".into(),
            status: MediaPlaybackStatus::Paused,
            track_id: track.into(),
            title: "Song".into(),
            artist: "Artist".into(),
            album: String::new(),
            length_micros: Some(200_000_000),
            position_micros: Some(10_000_000),
            can_play: true,
            can_pause: true,
            can_go_next: true,
            can_go_previous: true,
            can_seek: true,
        }),
    }
}
fn timeline_host() -> (ButtonSetHost, Seqpacket) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/media")
        .canonicalize()
        .unwrap();
    let set =
        SetDocument::from_toml(&std::fs::read_to_string(root.join("sets/timeline.toml")).unwrap())
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
                states: ["media.read.v1", "media.control.v1"]
                    .into_iter()
                    .map(|capability| CapabilityState {
                        capability: capability.into(),
                        required: false,
                        status: WireCapabilityStatus::Granted,
                    })
                    .collect(),
            })
            .unwrap();
    });
    let broker = BrokerClient::connect(client).unwrap();
    thread.join().unwrap();
    let bytes = std::fs::read(root.join("component/plugin.wasm")).unwrap();
    let mut host =
        ButtonSetHost::from_bytes_with_broker(&bytes, set, HostLimits::default(), Some(broker))
            .unwrap();
    let manifest = touchbar_package::PluginManifest::from_toml(
        &std::fs::read_to_string(root.join("touchbar-plugin.toml")).unwrap(),
    )
    .unwrap();
    host.set_assets(PackageAssets::from_directory(&root, &manifest.assets).unwrap());
    host.announce_capabilities().unwrap();
    let HostMessage::Request {
        request_id,
        capability,
        ..
    } = server.recv_host().unwrap()
    else {
        panic!()
    };
    assert_eq!(capability, "media.read.v1");
    server
        .send_supervisor(&SupervisorMessage::Response {
            request_id,
            result: BrokerResult::Success {
                payload: MediaSubscriptionOpened {
                    resource_id: 50,
                    state: state("/track-a"),
                }
                .encode()
                .unwrap(),
            },
        })
        .unwrap();
    host.dispatch_broker_events().unwrap();
    (host, server)
}
#[test]
fn track_change_cancel_seek_and_visibility_cleanup_use_real_logic() {
    let (mut host, server) = timeline_host();
    let viewport = Rect::new(0.0, 0.0, 1000.0, 60.0);
    host.render_at("timeline", viewport, Appearance::default(), Duration::ZERO)
        .unwrap();
    let input = |kind, value| InputEvent {
        item_id: "timeline".into(),
        widget_id: 30,
        kind,
        value,
        contact_id: Some(9),
        activation: (kind == InputKind::Activated).then_some(InputActivation {
            origin: ActivationOrigin::Physical,
            input_sequence: 10,
        }),
    };
    host.handle_event(&input(InputKind::Pressed, None)).unwrap();
    host.handle_event(&input(InputKind::ValueChanged, Some(0.5)))
        .unwrap();
    assert_eq!(
        host.snapshot().0["position_micros"],
        StateValue::Number(100_000_000.0)
    );
    host.handle_event(&input(InputKind::Cancelled, None))
        .unwrap();
    assert_eq!(
        host.snapshot().0["position_micros"],
        StateValue::Number(10_000_000.0)
    );
    assert!(!ready(&server));
    host.handle_event(&input(InputKind::Pressed, None)).unwrap();
    host.handle_event(&input(InputKind::ValueChanged, Some(0.7)))
        .unwrap();
    server
        .send_supervisor(&SupervisorMessage::ResourceEvent {
            resource_id: 50,
            sequence: 1,
            result: BrokerResult::Success {
                payload: state("/track-b").encode().unwrap(),
            },
        })
        .unwrap();
    host.dispatch_broker_events().unwrap();
    host.handle_event(&input(InputKind::ValueChanged, Some(0.8)))
        .unwrap();
    assert_eq!(
        host.snapshot().0["position_micros"],
        StateValue::Number(10_000_000.0),
        "old contact must stay cancelled after replacement motion"
    );
    host.handle_event(&input(InputKind::Activated, None))
        .unwrap();
    assert!(
        !ready(&server),
        "track replacement must not commit an old seek"
    );
    host.handle_event(&input(InputKind::Released, None))
        .unwrap();
    host.handle_event(&input(InputKind::Pressed, None)).unwrap();
    host.handle_event(&input(InputKind::ValueChanged, Some(0.5)))
        .unwrap();
    host.handle_event(&input(InputKind::Activated, None))
        .unwrap();
    let HostMessage::Request {
        payload,
        activation,
        ..
    } = server.recv_host().unwrap()
    else {
        panic!()
    };
    let command = MediaCommand::decode(&payload).unwrap();
    assert_eq!(
        command.action,
        MediaAction::SetPosition {
            track_id: "/track-b".into(),
            position_micros: 100_000_000
        }
    );
    assert_eq!(activation.unwrap().input_sequence, 10);
    host.handle_event(&input(InputKind::Released, None))
        .unwrap();
    let calls = host.guest_render_calls();
    host.render_at("timeline", viewport, Appearance::default(), Duration::ZERO)
        .unwrap();
    assert_eq!(host.guest_render_calls(), calls);
    host.handle_visibility_changed("timeline", false).unwrap();
    assert!(matches!(
        server.recv_host().unwrap(),
        HostMessage::Close {
            resource_id: 50,
            ..
        }
    ));
    assert!(
        ready(&server),
        "hide must cancel an outstanding seek request"
    );
    assert!(matches!(
        server.recv_host().unwrap(),
        HostMessage::Cancel { .. }
    ));
    assert!(!ready(&server));
}

#[test]
fn timeline_seek_is_reachable_at_native_bar_height() {
    let (mut host, _server) = timeline_host();
    let bounds = Rect::new(0.0, 0.0, 1000.0, 60.0);
    let mut ui = host
        .render_at("timeline", bounds, Appearance::default(), Duration::ZERO)
        .unwrap();
    let resolved = ui.resolve(bounds, touchbar_ui::Theme::default());
    let target = resolved
        .interactions
        .target(touchbar_ui::WidgetId(30))
        .expect("timeline seek target must fit in the physical bar");
    let mut interactions = touchbar_ui::InteractionState::default();
    let events = interactions.handle(
        &resolved.interactions,
        touchbar_ui::Contact {
            id: 8,
            phase: touchbar_ui::ContactPhase::Down,
            position: touchbar_ui::Point::new(
                target.bounds.x + target.bounds.width / 2.0,
                target.bounds.y + target.bounds.height / 2.0,
            ),
            time: Duration::ZERO,
        },
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event,touchbar_ui::UiEvent::Pressed{id} if id.0==30))
    );
}
#[test]
fn cancellation_patch_is_reflected_in_the_returned_ui() {
    let (mut host, server) = timeline_host();
    let bounds = Rect::new(0.0, 0.0, 1000.0, 60.0);
    host.render_at("timeline", bounds, Appearance::default(), Duration::ZERO)
        .unwrap();
    let mut input = InputEvent {
        item_id: "timeline".into(),
        widget_id: 30,
        kind: InputKind::Pressed,
        value: None,
        contact_id: Some(1),
        activation: None,
    };
    host.handle_event(&input).unwrap();
    input.kind = InputKind::ValueChanged;
    input.value = Some(0.5);
    host.handle_event(&input).unwrap();
    assert_eq!(
        host.snapshot().0["position_micros"],
        StateValue::Number(100_000_000.0)
    );
    server
        .send_supervisor(&SupervisorMessage::CapabilityChanged {
            generation: 2,
            state: CapabilityState {
                capability: "media.control.v1".into(),
                required: false,
                status: WireCapabilityStatus::Denied,
            },
        })
        .unwrap();
    host.dispatch_broker_events().unwrap();
    let mut ui = host
        .render_at("timeline", bounds, Appearance::default(), Duration::ZERO)
        .unwrap();
    assert_eq!(
        host.snapshot().0["position_micros"],
        StateValue::Number(10_000_000.0)
    );
    fn labels(node: &touchbar_ui::SemanticNode, result: &mut Vec<String>) {
        if node.role == touchbar_ui::SemanticRole::Label {
            result.push(node.label.clone());
        }
        for child in &node.children {
            labels(child, result);
        }
    }
    let mut rendered = vec![];
    labels(
        &ui.resolve(bounds, touchbar_ui::Theme::default())
            .inspector
            .semantics,
        &mut rendered,
    );
    assert!(
        rendered.contains(&"0:10".into()),
        "cancellation must render the authoritative position: {rendered:?}"
    );
    assert!(!rendered.contains(&"1:40".into()));
}
