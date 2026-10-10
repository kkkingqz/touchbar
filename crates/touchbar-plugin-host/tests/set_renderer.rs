use std::{collections::BTreeMap, time::Duration};

use touchbar_plugin_host::{
    Appearance, HostLimits, InputEvent, InputKind, PackageAssets, sets::CompiledSet,
};
use touchbar_set_config::{
    ActionDescription, InputKind as LogicKind, LogicDescription, ScalarType, ScalarValue,
    SetDocument, StateRecord, StateSnapshot, StateType, StateValue,
};
use touchbar_ui::{Rect, SemanticNode, SemanticRole, Theme};

const BUTTONS: &str = r#"
version = 1
id = "buttons"
label = "Buttons"
[layout]
kind = "row"
[[layout.children]]
kind = "button"
id = "first"
widget_id = 10
label = "Alpha"
width = 80
enabled_from = "ready"
on_activate = "select"
[[layout.children]]
kind = "button"
id = "second"
widget_id = 11
label = "Beta"
width = 80
on_activate = "select"
"#;

const REPEAT: &str = r#"
version = 1
id = "buttons"
label = "Players"
[layout]
kind = "row"
[[layout.children]]
kind = "repeat"
repeat_from = "players"
repeat_key = "id"
spacing = 2
[layout.children.template]
kind = "button"
id = "player"
label_from = "record.title"
on_activate = "select"
on_cancelled = "cancel"
"#;

fn description() -> LogicDescription {
    LogicDescription {
        actions: BTreeMap::from([
            (
                "select".into(),
                ActionDescription {
                    events: vec![LogicKind::Activated],
                    value_type: None,
                },
            ),
            (
                "cancel".into(),
                ActionDescription {
                    events: vec![LogicKind::Cancelled],
                    value_type: None,
                },
            ),
        ]),
        state: BTreeMap::from([
            ("ready".into(), StateType::Bool),
            (
                "players".into(),
                StateType::List {
                    key: "id".into(),
                    fields: BTreeMap::from([
                        ("id".into(), ScalarType::Text),
                        ("title".into(), ScalarType::Text),
                    ]),
                },
            ),
        ]),
    }
}

fn state(keys: &[&str]) -> StateSnapshot {
    StateSnapshot(BTreeMap::from([
        ("ready".into(), StateValue::Bool(true)),
        (
            "players".into(),
            StateValue::List(
                keys.iter()
                    .map(|key| {
                        StateRecord(BTreeMap::from([
                            ("id".into(), ScalarValue::Text((*key).into())),
                            ("title".into(), ScalarValue::Text((*key).into())),
                        ]))
                    })
                    .collect(),
            ),
        ),
    ]))
}

fn compile(source: &str) -> CompiledSet {
    CompiledSet::new(
        SetDocument::from_toml(source).unwrap(),
        &description(),
        HostLimits::default(),
    )
    .unwrap()
}

fn buttons(set: &CompiledSet, state: &StateSnapshot) -> Vec<SemanticNode> {
    let bounds = Rect::new(0.0, 0.0, 300.0, 60.0);
    let mut ui = set
        .render(
            state,
            bounds,
            Appearance::default(),
            Duration::ZERO,
            &PackageAssets::default(),
        )
        .unwrap();
    fn visit(node: &SemanticNode, result: &mut Vec<SemanticNode>) {
        if node.role == SemanticRole::Button {
            result.push(node.clone());
        }
        for child in &node.children {
            visit(child, result);
        }
    }
    let mut result = vec![];
    visit(
        &ui.resolve(bounds, Theme::default()).inspector.semantics,
        &mut result,
    );
    result
}

fn event(widget_id: u64, kind: InputKind) -> InputEvent {
    InputEvent {
        item_id: "buttons".into(),
        widget_id,
        kind,
        value: None,
        contact_id: Some(1),
        activation: None,
    }
}

#[test]
fn layout_order_changes_without_logic() {
    let first = compile(BUTTONS);
    let original = buttons(&first, &state(&[]));
    assert_eq!(
        original
            .iter()
            .map(|button| button.label.as_str())
            .collect::<Vec<_>>(),
        vec!["Alpha", "Beta"]
    );
    assert!(original[0].bounds.x < original[1].bounds.x);
    let start = BUTTONS.find("[[layout.children]]").unwrap();
    let next = BUTTONS[start + 1..].find("[[layout.children]]").unwrap() + start + 1;
    let reversed = format!(
        "{}{}{}",
        &BUTTONS[..start],
        &BUTTONS[next..],
        &BUTTONS[start..next]
    );
    let second = compile(&reversed);
    let reordered = buttons(&second, &state(&[]));
    assert_eq!(
        reordered
            .iter()
            .map(|button| button.label.as_str())
            .collect::<Vec<_>>(),
        vec!["Beta", "Alpha"]
    );
    assert_eq!(reordered[1].id.unwrap().0, 10);
}

#[test]
fn missing_action_is_load_error() {
    let set = SetDocument::from_toml(
        &BUTTONS.replace("on_activate = \"select\"", "on_activate = \"unknown\""),
    )
    .unwrap();
    assert!(CompiledSet::new(set, &description(), HostLimits::default()).is_err());
}

#[test]
fn state_type_mismatch_is_load_error() {
    let mut desc = description();
    desc.state.insert("ready".into(), StateType::Text);
    assert!(
        CompiledSet::new(
            SetDocument::from_toml(BUTTONS).unwrap(),
            &desc,
            HostLimits::default()
        )
        .is_err()
    );
}

#[test]
fn repeated_keys_survive_reorder() {
    let set = compile(REPEAT);
    let before = buttons(&set, &state(&["a", "b"]));
    let target = before[0].id.unwrap().0;
    set.action_for(&event(target, InputKind::Pressed)).unwrap();
    let after = buttons(&set, &state(&["b", "a"]));
    assert_eq!(after[1].id.unwrap().0, target);
    let action = set
        .action_for(&event(target, InputKind::Activated))
        .unwrap()
        .unwrap();
    assert_eq!(action.action, "select");
    assert_eq!(action.record_key.as_deref(), Some("a"));
}

#[test]
fn removed_repeat_key_cancels_contact() {
    let set = compile(REPEAT);
    let before = buttons(&set, &state(&["a", "b"]));
    let target = before[0].id.unwrap().0;
    set.action_for(&event(target, InputKind::Pressed)).unwrap();
    buttons(&set, &state(&["b"]));
    let cancelled = set.drain_cancelled();
    assert_eq!(cancelled.len(), 1);
    assert_eq!(cancelled[0].kind, LogicKind::Cancelled);
    assert_eq!(cancelled[0].record_key.as_deref(), Some("a"));
    assert!(
        set.action_for(&event(target, InputKind::Activated))
            .unwrap()
            .is_none()
    );
}

#[test]
fn duplicate_repeat_keys_are_rejected() {
    let set = compile(REPEAT);
    let bounds = Rect::new(0.0, 0.0, 300.0, 60.0);
    assert!(
        set.render(
            &state(&["a", "a"]),
            bounds,
            Appearance::default(),
            Duration::ZERO,
            &PackageAssets::default()
        )
        .is_err()
    );
}

#[test]
fn disabled_or_hidden_widgets_cannot_dispatch_actions() {
    let set = compile(BUTTONS);
    buttons(&set, &state(&[]));
    let mut disabled = state(&[]);
    disabled.0.insert("ready".into(), StateValue::Bool(false));
    buttons(&set, &disabled);
    assert!(
        set.action_for(&event(10, InputKind::Activated))
            .unwrap()
            .is_none()
    );
    let hidden = compile(&BUTTONS.replace("enabled_from", "visible_from"));
    assert_eq!(buttons(&hidden, &disabled).len(), 1);
    assert!(
        hidden
            .action_for(&event(10, InputKind::Activated))
            .unwrap()
            .is_none()
    );
}

#[test]
fn invalid_state_and_viewport_are_rejected_before_ui() {
    let set = compile(BUTTONS);
    let bounds = Rect::new(0.0, 0.0, 300.0, 60.0);
    let mut bad = state(&[]);
    bad.0
        .insert("ready".into(), StateValue::Text("wrong".into()));
    assert!(
        set.render(
            &bad,
            bounds,
            Appearance::default(),
            Duration::ZERO,
            &PackageAssets::default()
        )
        .is_err()
    );
    assert!(
        set.render(
            &state(&[]),
            Rect::new(0.0, 0.0, f32::NAN, 60.0),
            Appearance::default(),
            Duration::ZERO,
            &PackageAssets::default()
        )
        .is_err()
    );
}

#[test]
fn responsive_layout_uses_declared_variants() {
    let source = r#"
version = 1
id = "buttons"
label = "Responsive"
[layout]
kind = "responsive"
id = "choice"
[[layout.variants]]
representation = "compact"
minimum_width = 0
[layout.variants.layout]
kind = "button"
id = "compact"
label = "Compact"
on_activate = "select"
[[layout.variants]]
representation = "full"
minimum_width = 200
[layout.variants.layout]
kind = "button"
id = "full"
label = "Full"
on_activate = "select"
"#;
    let set = compile(source);
    assert_eq!(buttons(&set, &state(&[]))[0].label, "Full");
    let bounds = Rect::new(0.0, 0.0, 100.0, 60.0);
    let mut ui = set
        .render(
            &state(&[]),
            bounds,
            Appearance::default(),
            Duration::ZERO,
            &PackageAssets::default(),
        )
        .unwrap();
    assert_eq!(
        ui.resolve(bounds, Theme::default())
            .inspector
            .semantics
            .label,
        "Compact"
    );
}

#[test]
fn canvas_records_render_bounded_commands() {
    use touchbar_ui::Primitive;
    let source = "version = 1\nid = \"buttons\"\nlabel = \"Canvas\"\n[layout]\nkind = \"canvas\"\ncanvas_from = \"commands\"\ncolor = \"accent\"\n";
    let mut desc = description();
    let fields = ["x", "y", "width", "height"]
        .into_iter()
        .map(|name| (name.into(), ScalarType::Number))
        .chain([
            ("id".into(), ScalarType::Text),
            ("op".into(), ScalarType::Text),
        ])
        .collect();
    desc.state.insert(
        "commands".into(),
        StateType::List {
            key: "id".into(),
            fields,
        },
    );
    let set = CompiledSet::new(
        SetDocument::from_toml(source).unwrap(),
        &desc,
        HostLimits::default(),
    )
    .unwrap();
    let record = StateRecord(BTreeMap::from([
        ("id".into(), ScalarValue::Text("r1".into())),
        ("op".into(), ScalarValue::Text("rect".into())),
        ("x".into(), ScalarValue::Number(2.0)),
        ("y".into(), ScalarValue::Number(3.0)),
        ("width".into(), ScalarValue::Number(10.0)),
        ("height".into(), ScalarValue::Number(12.0)),
    ]));
    let mut state = state(&[]);
    state
        .0
        .insert("commands".into(), StateValue::List(vec![record]));
    let bounds = Rect::new(0.0, 0.0, 300.0, 60.0);
    let mut ui = set
        .render(
            &state,
            bounds,
            Appearance::default(),
            Duration::ZERO,
            &PackageAssets::default(),
        )
        .unwrap();
    let resolved = ui.resolve(bounds, Theme::default());
    assert!(resolved.scene.primitives.iter().any(|primitive| matches!(primitive,
        Primitive::RoundedRect { rect, color, .. } if *rect == Rect::new(2.0, 3.0, 10.0, 12.0) && *color == Theme::default().accent)));
    if let StateValue::List(records) = state.0.get_mut("commands").unwrap() {
        records[0]
            .0
            .insert("op".into(), ScalarValue::Text("unsupported".into()));
    }
    assert!(
        set.render(
            &state,
            bounds,
            Appearance::default(),
            Duration::ZERO,
            &PackageAssets::default()
        )
        .is_err()
    );
}

#[test]
fn animation_and_effect_clocks_survive_state_redraw() {
    use touchbar_ui::Primitive;
    let source = r#"
version = 1
id = "buttons"
label = "Effect"
[layout]
kind = "shader-effect"
label = "Effect"
[layout.effect]
id = 42
source = "let color = accent;"
period_ms = 1000
[layout.motion]
id = 12
duration_ms = 1000
easing = "linear"
playback = "once"
[layout.motion.from]
opacity = 0.0
[layout.motion.to]
opacity = 1.0
"#;
    let set = compile(source);
    let bounds = Rect::new(0.0, 0.0, 300.0, 60.0);
    for now in [Duration::from_millis(100), Duration::from_millis(250)] {
        let mut ui = set
            .render(
                &state(&[]),
                bounds,
                Appearance::default(),
                now,
                &PackageAssets::default(),
            )
            .unwrap();
        let resolved = ui.resolve(bounds, Theme::default());
        assert!(
            resolved
                .scene
                .primitives
                .iter()
                .any(|primitive| matches!(primitive,
            Primitive::PushMotion { motion, .. } if motion.started == Duration::from_millis(100)))
        );
        assert!(resolved.scene.primitives.iter().any(|primitive| matches!(primitive,
            Primitive::ShaderEffect { effect, .. } if effect.started == Duration::from_millis(100))));
    }
    assert!(
        CompiledSet::new(
            SetDocument::from_toml(&source.replace("let color = accent;", "discard;")).unwrap(),
            &description(),
            HostLimits::default()
        )
        .is_err()
    );
}

#[test]
fn panel_layout_has_its_own_bindings_and_cancels_primary_contact() {
    let source = format!(
        "{BUTTONS}\n[[panels]]\nid = \"details\"\n[panels.layout]\nkind = \"button\"\nid = \"detail\"\nwidget_id = 90\nlabel = \"Details\"\non_activate = \"select\"\n"
    );
    let set = compile(&source);
    buttons(&set, &state(&[]));
    set.action_for(&event(10, InputKind::Pressed)).unwrap();
    let bounds = Rect::new(0.0, 0.0, 300.0, 60.0);
    let mut ui = set
        .render_panel(
            "details",
            &state(&[]),
            bounds,
            Appearance::default(),
            Duration::ZERO,
            &PackageAssets::default(),
        )
        .unwrap();
    assert_eq!(
        ui.resolve(bounds, Theme::default())
            .inspector
            .semantics
            .id
            .unwrap()
            .0,
        90
    );
    assert!(
        set.action_for(&event(10, InputKind::Activated))
            .unwrap()
            .is_none()
    );
    assert!(
        set.action_for(&event(90, InputKind::Activated))
            .unwrap()
            .is_none()
    ); // cancelled contact remains tombstoned
    let mut fresh = event(90, InputKind::Pressed);
    fresh.contact_id = Some(2);
    set.action_for(&fresh).unwrap();
    fresh.kind = InputKind::Activated;
    assert_eq!(set.action_for(&fresh).unwrap().unwrap().action, "select");
}
