use std::{os::fd::AsRawFd, path::PathBuf, time::Duration};
use touchbar_plugin_host::{
    Appearance, BrokerClient, HostLimits, InputEvent, InputKind, sets::ButtonSetHost,
};
use touchbar_protocol::broker_ipc::{
    CapabilityState, HostMessage, Seqpacket, SupervisorMessage, WireCapabilityStatus,
};
use touchbar_set_config::{SetDocument, StateValue};
use touchbar_ui::{Rect, SemanticNode, SemanticRole, Theme};
fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../plugins/app-actions")
        .canonicalize()
        .unwrap()
}
fn document(name: &str) -> SetDocument {
    SetDocument::from_toml(
        &std::fs::read_to_string(root().join(format!("sets/{name}.toml"))).unwrap(),
    )
    .unwrap()
}
fn open(set: SetDocument, granted: bool) -> (ButtonSetHost, Seqpacket) {
    let (client, server) = Seqpacket::pair().unwrap();
    let sender = server.try_clone().unwrap();
    let thread = std::thread::spawn(move || {
        let HostMessage::GetCapabilities { request_id } = sender.recv_host().unwrap() else {
            panic!("expected capabilities");
        };
        sender
            .send_supervisor(&SupervisorMessage::Capabilities {
                request_id,
                generation: 1,
                states: vec![CapabilityState {
                    capability: "keyboard.shortcut.v1".into(),
                    required: false,
                    status: if granted {
                        WireCapabilityStatus::Granted
                    } else {
                        WireCapabilityStatus::Denied
                    },
                }],
            })
            .unwrap();
    });
    let broker = BrokerClient::connect(client).unwrap();
    thread.join().unwrap();
    let bytes = std::fs::read(root().join("component/plugin.wasm")).unwrap();
    let mut host =
        ButtonSetHost::from_bytes_with_broker(&bytes, set, HostLimits::default(), Some(broker))
            .unwrap();
    let manifest = touchbar_package::PluginManifest::from_toml(
        &std::fs::read_to_string(root().join("touchbar-plugin.toml")).unwrap(),
    )
    .unwrap();
    host.set_assets(
        touchbar_plugin_host::PackageAssets::from_directory(&root(), &manifest.assets).unwrap(),
    );
    host.announce_capabilities().unwrap();
    (host, server)
}
fn ids(host: &mut ButtonSetHost, item: &str, width: f32) -> Vec<u64> {
    let bounds = Rect::new(0.0, 0.0, width, 60.0);
    let mut ui = host
        .render_at(item, bounds, Appearance::default(), Duration::ZERO)
        .unwrap();
    fn visit(node: &SemanticNode, out: &mut Vec<u64>) {
        if node.role == SemanticRole::Button {
            out.push(node.id.unwrap().0);
        }
        for child in &node.children {
            visit(child, out);
        }
    }
    let mut out = vec![];
    visit(
        &ui.resolve(bounds, Theme::default()).inspector.semantics,
        &mut out,
    );
    out
}
fn touch(host: &mut ButtonSetHost, item: &str, widget: u64, kind: InputKind) {
    host.handle_event(&InputEvent {
        item_id: item.into(),
        widget_id: widget,
        kind,
        value: None,
        contact_id: Some(7),
        activation: None,
    })
    .unwrap();
}
#[test]
fn original_buttons_and_identical_wasm_have_independent_capabilities() {
    let browser = document("browser");
    let formatting = document("formatting");
    assert_eq!(browser.default_width, 240);
    assert_eq!(formatting.default_width, 240);
    let (mut a, requests) = open(browser.clone(), true);
    let mut compact = browser;
    compact.id = "compact-browser".into();
    compact.default_width = 120;
    compact.layout.children.reverse();
    let (mut b, _) = open(compact, false);
    let (mut formatting, _) = open(formatting, true);
    assert_eq!(ids(&mut a, "browser", 240.0), [10, 11, 12]);
    assert_eq!(ids(&mut formatting, "formatting", 240.0), [20, 21, 22]);
    assert!(ids(&mut b, "compact-browser", 120.0).is_empty());
    assert_eq!(
        a.snapshot().0["shortcuts_available"],
        StateValue::Bool(true)
    );
    assert_eq!(
        b.snapshot().0["shortcuts_available"],
        StateValue::Bool(false)
    );
    touch(&mut a, "browser", 10, InputKind::Pressed);
    touch(&mut a, "browser", 10, InputKind::Activated);
    assert!(
        matches!(requests.recv_host().unwrap(),HostMessage::Request{capability,operation,..} if capability=="keyboard.shortcut.v1" && operation=="press")
    );
    touch(&mut a, "browser", 10, InputKind::Released);
    touch(&mut a, "browser", 10, InputKind::Pressed);
    a.handle_visibility_changed("browser", false).unwrap();
    a.handle_visibility_changed("browser", true).unwrap();
    ids(&mut a, "browser", 240.0);
    touch(&mut a, "browser", 10, InputKind::Activated);
    // A stale activation after hiding emits no second broker request.
    let flags = unsafe { libc::fcntl(requests.as_raw_fd(), libc::F_GETFL) };
    unsafe {
        libc::fcntl(
            requests.as_raw_fd(),
            libc::F_SETFL,
            flags | libc::O_NONBLOCK,
        );
    }
    assert!(requests.recv_host().is_err());
}
#[test]
fn denied_set_cannot_issue_shortcut() {
    let (mut host, server) = open(document("browser"), false);
    assert!(ids(&mut host, "browser", 240.0).is_empty());
    touch(&mut host, "browser", 10, InputKind::Pressed);
    touch(&mut host, "browser", 10, InputKind::Activated);
    touch(&mut host, "browser", 10, InputKind::Released);
    let mut fd = libc::pollfd {
        fd: server.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    assert_eq!(unsafe { libc::poll(&mut fd, 1, 100) }, 0);
    assert_eq!(
        host.snapshot().0["shortcuts_available"],
        StateValue::Bool(false)
    );
}
