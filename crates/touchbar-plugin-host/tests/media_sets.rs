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
#[test]
fn track_change_cancel_seek_and_visibility_cleanup_use_real_logic() {
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
