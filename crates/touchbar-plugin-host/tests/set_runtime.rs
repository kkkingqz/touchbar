use std::{collections::BTreeMap, time::Duration};
use touchbar_plugin_host::{Appearance, HostLimits, InputEvent, InputKind, sets::ButtonSetHost};
use touchbar_set_config::{SetDocument, StateValue};
use touchbar_ui::Rect;

fn host(id: &str) -> ButtonSetHost {
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/wasm32-wasip2/release/touchbar_button_set_logic_demo.wasm"
    ))
    .expect("build the logic demo before runtime tests");
    let set = SetDocument::from_toml(&format!(
        r#"
version = 1
id = "{id}"
label = "Counter"
[layout]
kind = "button"
widget_id = 10
label = "Increment"
on_activate = "increment"
"#
    ))
    .unwrap();
    ButtonSetHost::from_bytes_with_broker(&bytes, set, HostLimits::default(), None).unwrap()
}

fn event(id: &str, kind: InputKind) -> InputEvent {
    InputEvent {
        item_id: id.into(),
        widget_id: 10,
        kind,
        value: None,
        contact_id: Some(1),
        activation: None,
    }
}
fn render(host: &mut ButtonSetHost, id: &str) {
    host.render_at(
        id,
        Rect::new(0.0, 0.0, 160.0, 60.0),
        Appearance::default(),
        Duration::ZERO,
    )
    .unwrap();
}

#[test]
fn same_component_separate_stores() {
    let mut a = host("counter-a");
    let mut b = host("counter-b");
    render(&mut a, "counter-a");
    render(&mut b, "counter-b");
    a.handle_event(&event("counter-a", InputKind::Pressed))
        .unwrap();
    a.handle_event(&event("counter-a", InputKind::Activated))
        .unwrap();
    assert_eq!(
        a.snapshot().0,
        BTreeMap::from([
            ("count".into(), StateValue::Number(1.0)),
            ("count_text".into(), StateValue::Text("1".into())),
        ])
    );
    assert_eq!(
        b.snapshot().0,
        BTreeMap::from([
            ("count".into(), StateValue::Number(0.0)),
            ("count_text".into(), StateValue::Text("0".into())),
        ])
    );
    assert!(
        a.handle_event(&event("counter-b", InputKind::Activated))
            .is_err()
    );
}

#[test]
fn hidden_contact_cannot_activate_when_shown_again() {
    let mut a = host("counter-a");
    render(&mut a, "counter-a");
    a.handle_event(&event("counter-a", InputKind::Pressed))
        .unwrap();
    a.handle_visibility_changed("counter-a", false).unwrap();
    a.handle_visibility_changed("counter-a", true).unwrap();
    render(&mut a, "counter-a");
    a.handle_event(&event("counter-a", InputKind::Activated))
        .unwrap();
    assert_eq!(a.snapshot().0.get("count"), Some(&StateValue::Number(0.0)));
}

#[test]
fn repeated_context_does_not_call_guest_render_again() {
    let mut a = host("counter-a");
    render(&mut a, "counter-a");
    render(&mut a, "counter-a");
    assert_eq!(a.guest_render_calls(), 1);
}

#[test]
fn late_input_after_restart_is_rejected() {
    let mut old = host("counter-a");
    render(&mut old, "counter-a");
    old.handle_event(&event("counter-a", InputKind::Pressed))
        .unwrap();
    drop(old);
    let mut new = host("counter-a");
    render(&mut new, "counter-a");
    new.handle_event(&event("counter-a", InputKind::Activated))
        .unwrap();
    assert_eq!(
        new.snapshot().0.get("count"),
        Some(&StateValue::Number(0.0))
    );
}

#[test]
fn init_and_describe_cannot_request_effects_and_action_uses_real_activation() {
    use touchbar_plugin_host::{BrokerClient, InputActivation};
    use touchbar_protocol::broker_ipc::{
        ActivationOrigin, CallbackPhase, HostMessage, Seqpacket, SupervisorMessage,
    };
    let (client, server) = Seqpacket::pair().unwrap();
    let receiver = std::thread::spawn(move || {
        assert!(matches!(
            server.recv_host().unwrap(),
            HostMessage::GetCapabilities { request_id: 1 }
        ));
        server
            .send_supervisor(&SupervisorMessage::Capabilities {
                request_id: 1,
                generation: 1,
                states: vec![],
            })
            .unwrap();
        let mut activations = vec![];
        for _ in 0..2 {
            let HostMessage::Request {
                phase,
                activation,
                capability,
                ..
            } = server.recv_host().unwrap()
            else {
                panic!("expected effect request");
            };
            assert_eq!(phase, CallbackPhase::Input);
            assert_eq!(capability, "keyboard.shortcut.v1");
            activations.push(activation);
        }
        activations
    });
    let broker = BrokerClient::connect(client).unwrap();
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../target/wasm32-wasip2/release/touchbar_button_set_logic_demo.wasm"
    ))
    .unwrap();
    let set = SetDocument::from_toml("version=1\nid=\"counter-a\"\nlabel=\"Counter\"\nlogic_config=\"probe-action\"\n[layout]\nkind=\"button\"\nwidget_id=10\nlabel=\"Increment\"\non_activate=\"increment\"").unwrap();
    let mut a =
        ButtonSetHost::from_bytes_with_broker(&bytes, set, HostLimits::default(), Some(broker))
            .unwrap();
    render(&mut a, "counter-a");
    for activation in [
        None,
        Some(InputActivation {
            origin: ActivationOrigin::Physical,
            input_sequence: 42,
        }),
    ] {
        a.handle_event(&event("counter-a", InputKind::Pressed))
            .unwrap();
        let mut input = event("counter-a", InputKind::Activated);
        input.activation = activation;
        a.handle_event(&input).unwrap();
    }
    let activations = receiver.join().unwrap();
    assert!(activations[0].is_none());
    let physical = activations[1].as_ref().unwrap();
    assert_eq!(physical.item_id, "counter-a");
    assert_eq!(physical.widget_id, 10);
    assert_eq!(physical.input_sequence, 42);
    assert!(physical.deadline_monotonic_micros > 0);
    let mut probe_set = SetDocument::from_toml("version=1\nid=\"probe\"\nlabel=\"Probe\"\nlogic_config=\"probe-effects\"\n[layout]\nkind=\"text\"\ntext=\"probe\"").unwrap();
    assert!(
        ButtonSetHost::from_bytes_with_broker(
            &bytes,
            probe_set.clone(),
            HostLimits::default(),
            None
        )
        .is_ok()
    );
    probe_set.logic_config = "invalid".into();
    assert!(
        ButtonSetHost::from_bytes_with_broker(&bytes, probe_set, HostLimits::default(), None)
            .is_err()
    );
}
