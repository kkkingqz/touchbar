use std::collections::BTreeMap;

use touchbar_set_config::{
    ActionDescription, InputKind, LogicDescription, ScalarType, SetDocument, StatePatch,
    StateSnapshot, StateType, StateValue,
};

const BROWSER: &str = r#"
version = 1
id = "browser"
label = "Browser Navigation"
default_width = 240
logic_config = "browser"
[selection]
applications = ["firefox", "org.mozilla.firefox"]
enabled_by_default = true
priority = 0
[layout]
kind = "row"
spacing = 8
[[layout.children]]
kind = "button"
id = "back"
widget_id = 10
label = "Back"
symbol = "chevron.left"
enabled_from = "shortcuts_available"
on_activate = "back"
[[layout.children]]
kind = "button"
id = "forward"
widget_id = 11
label = "Forward"
symbol = "chevron.right"
enabled_from = "shortcuts_available"
on_activate = "forward"
[[layout.children]]
kind = "button"
id = "reload"
widget_id = 12
label = "Reload"
asset = "reload"
enabled_from = "shortcuts_available"
on_activate = "reload"
"#;

fn description() -> LogicDescription {
    LogicDescription {
        actions: ["back", "forward", "reload"]
            .into_iter()
            .map(|name| {
                (
                    name.into(),
                    ActionDescription {
                        events: vec![InputKind::Activated],
                        value_type: None,
                    },
                )
            })
            .collect(),
        state: BTreeMap::from([
            ("shortcuts_available".into(), StateType::Bool),
            ("count".into(), StateType::Number),
        ]),
    }
}

#[test]
fn browser_file_is_one_set() {
    let set = SetDocument::from_toml(BROWSER).unwrap();
    assert_eq!(set.id, "browser");
    assert_eq!(set.default_width, 240);
    assert_eq!(set.layout.children.len(), 3);
    assert_eq!(set.layout.children[0].widget_id, Some(10));
    set.validate_bindings(&description()).unwrap();
}

#[test]
fn unknown_fields_are_rejected() {
    assert!(SetDocument::from_toml(&BROWSER.replace("spacing = 8", "spcing = 8")).is_err());
    assert!(SetDocument::from_toml(&format!("secret = true\n{BROWSER}")).is_err());
}

#[test]
fn duplicate_widget_ids_are_rejected() {
    assert!(SetDocument::from_toml(&BROWSER.replace("id = \"forward\"", "id = \"back\"")).is_err());
    assert!(SetDocument::from_toml(&BROWSER.replace("widget_id = 11", "widget_id = 10")).is_err());
}

#[test]
fn unsafe_ids_and_layout_values_are_rejected() {
    for (from, to) in [
        ("id = \"browser\"", "id = \"../browser\""),
        ("default_width = 240", "default_width = 0"),
        ("spacing = 8", "spacing = nan"),
        ("spacing = 8", "spacing = -1"),
        ("symbol = \"chevron.left\"", "symbol = \"nonexistent\""),
    ] {
        assert!(
            SetDocument::from_toml(&BROWSER.replace(from, to)).is_err(),
            "accepted {to}"
        );
    }
}

#[test]
fn missing_and_wrong_binding_types_are_load_errors() {
    let set = SetDocument::from_toml(BROWSER).unwrap();
    let mut description = description();
    description.actions.remove("back");
    assert!(set.validate_bindings(&description).is_err());
    description = self::description();
    description
        .state
        .insert("shortcuts_available".into(), StateType::Text);
    assert!(set.validate_bindings(&description).is_err());
    description = self::description();
    description.actions.get_mut("back").unwrap().events = vec![InputKind::Released];
    assert!(set.validate_bindings(&description).is_err());
}

#[test]
fn limits_match_host_budget() {
    let root = "version = 1\nid = \"many\"\nlabel = \"Many\"\n[layout]\nkind = \"row\"\n";
    let mut text = root.to_string();
    for index in 0..255 {
        text.push_str(&format!(
            "[[layout.children]]\nkind = \"text\"\nid = \"n{index}\"\ntext = \"x\"\n"
        ));
    }
    assert!(SetDocument::from_toml(&text).is_ok());
    text.push_str("[[layout.children]]\nkind = \"text\"\ntext = \"overflow\"\n");
    assert!(SetDocument::from_toml(&text).is_err());
    assert!(
        SetDocument::from_toml(&BROWSER.replace(
            "label = \"Back\"",
            &format!("label = \"{}\"", "x".repeat(16 * 1024))
        ))
        .is_err()
    );
    assert!(SetDocument::from_toml(&format!("{BROWSER}{}", " ".repeat(256 * 1024))).is_err());
}

#[test]
fn state_patch_is_atomic() {
    let mut state = StateSnapshot(BTreeMap::from([("count".into(), StateValue::Number(1.0))]));
    let before = state.clone();
    let invalid = StatePatch(BTreeMap::from([
        ("count".into(), StateValue::Number(2.0)),
        (
            "shortcuts_available".into(),
            StateValue::Text("wrong".into()),
        ),
    ]));
    assert!(state.apply(&invalid, &description()).is_err());
    assert_eq!(state, before);
    let patch = StatePatch(BTreeMap::from([("count".into(), StateValue::Number(2.0))]));
    assert!(state.apply(&patch, &description()).unwrap());
    assert_eq!(state.0["count"], StateValue::Number(2.0));
    assert!(!state.apply(&patch, &description()).unwrap());
}

#[test]
fn state_rejects_nonfinite_numbers_and_undeclared_fields() {
    let mut state = StateSnapshot::default();
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(
            state
                .apply(
                    &StatePatch(BTreeMap::from([(
                        "count".into(),
                        StateValue::Number(value)
                    )])),
                    &description()
                )
                .is_err()
        );
    }
    assert!(
        state
            .apply(
                &StatePatch(BTreeMap::from([("unknown".into(), StateValue::Bool(true))])),
                &description()
            )
            .is_err()
    );
    assert!(state.0.is_empty());
}

#[test]
fn keyed_lists_are_bounded_and_typed() {
    use touchbar_set_config::{ScalarValue, StateRecord};
    let description = LogicDescription {
        actions: BTreeMap::new(),
        state: BTreeMap::from([(
            "players".into(),
            StateType::List {
                key: "id".into(),
                fields: BTreeMap::from([
                    ("id".into(), ScalarType::Text),
                    ("playing".into(), ScalarType::Bool),
                ]),
            },
        )]),
    };
    let record = |id: &str| {
        StateRecord(BTreeMap::from([
            ("id".into(), ScalarValue::Text(id.into())),
            ("playing".into(), ScalarValue::Bool(true)),
        ]))
    };
    let mut state = StateSnapshot::default();
    let patch = |records| {
        StatePatch(BTreeMap::from([(
            "players".into(),
            StateValue::List(records),
        )]))
    };
    state
        .apply(&patch(vec![record("a"), record("b")]), &description)
        .unwrap();
    assert!(
        state
            .apply(&patch(vec![record("a"), record("a")]), &description)
            .is_err()
    );
    assert!(
        state
            .apply(
                &patch((0..513).map(|n| record(&n.to_string())).collect()),
                &description
            )
            .is_err()
    );
    assert_eq!(
        state.0["players"],
        StateValue::List(vec![record("a"), record("b")])
    );
}

#[test]
fn slider_and_repeat_bindings_are_typed() {
    let source = r#"
version = 1
id = "volume"
label = "Volume"
[layout]
kind = "row"
[[layout.children]]
kind = "slider"
id = "level"
value_from = "level"
on_value_changed = "set-level"
[[layout.children]]
kind = "repeat"
repeat_from = "players"
repeat_key = "id"
[layout.children.template]
kind = "button"
id = "player"
label_from = "record.id"
enabled_from = "record.playing"
on_activate = "back"
"#;
    let mut desc = description();
    desc.state.insert("level".into(), StateType::Number);
    desc.state.insert(
        "players".into(),
        StateType::List {
            key: "id".into(),
            fields: BTreeMap::from([
                ("id".into(), ScalarType::Text),
                ("playing".into(), ScalarType::Bool),
            ]),
        },
    );
    desc.actions.insert(
        "set-level".into(),
        ActionDescription {
            events: vec![InputKind::ValueChanged],
            value_type: Some(ScalarType::Number),
        },
    );
    let set = SetDocument::from_toml(source).unwrap();
    set.validate_bindings(&desc).unwrap();
    assert!(
        SetDocument::from_toml(&source.replace("record.id", "record.unknown"))
            .unwrap()
            .validate_bindings(&desc)
            .is_err()
    );
    assert!(
        SetDocument::from_toml(&source.replace("repeat_key = \"id\"", "repeat_key = \"playing\""))
            .unwrap()
            .validate_bindings(&desc)
            .is_err()
    );
    desc.actions.get_mut("set-level").unwrap().value_type = None;
    assert!(set.validate_bindings(&desc).is_err());
}

#[test]
fn panels_require_local_references_and_ordered_widths() {
    let panel = r#"
[[panels]]
id = "expanded"
minimum_width = 100
preferred_width = 240
maximum_width = 400
[panels.layout]
kind = "text"
text = "Panel"
"#;
    let source = format!("expanded_panel = \"expanded\"\n{BROWSER}{panel}");
    let set = SetDocument::from_toml(&source).unwrap();
    assert_eq!(set.panels.len(), 1);
    assert!(
        SetDocument::from_toml(&source.replace(
            "expanded_panel = \"expanded\"",
            "expanded_panel = \"missing\""
        ))
        .is_err()
    );
    assert!(
        SetDocument::from_toml(&source.replace("minimum_width = 100", "minimum_width = 300"))
            .is_err()
    );
}

#[test]
fn layout_depth_is_bounded_before_rendering() {
    let nested = |depth| {
        let mut source =
            "version = 1\nid = \"nested\"\nlabel = \"Nested\"\n[layout]\nkind = \"row\"\n"
                .to_owned();
        let mut path = "layout".to_owned();
        for _ in 1..depth {
            path.push_str(".children");
            source.push_str(&format!("[[{path}]]\nkind = \"row\"\n"));
        }
        source
    };
    assert!(SetDocument::from_toml(&nested(32)).is_ok());
    assert!(SetDocument::from_toml(&nested(33)).is_err());
}

#[test]
fn total_state_text_budget_applies_across_updates() {
    let desc = LogicDescription {
        actions: BTreeMap::new(),
        state: BTreeMap::from([
            ("first".into(), StateType::Text),
            ("second".into(), StateType::Text),
        ]),
    };
    let mut state = StateSnapshot::default();
    state
        .apply(
            &StatePatch(BTreeMap::from([(
                "first".into(),
                StateValue::Text("a".repeat(4096)),
            )])),
            &desc,
        )
        .unwrap();
    let before = state.clone();
    assert!(
        state
            .apply(
                &StatePatch(BTreeMap::from([(
                    "second".into(),
                    StateValue::Text("b".repeat(13 * 1024))
                )])),
                &desc
            )
            .is_err()
    );
    assert_eq!(state, before);
}
#[test]
fn nonfinite_toml_values_are_rejected_before_normalization() {
    for value in ["nan", "inf", "-inf"] {
        let source = format!(
            "version = 1\nid = \"paint\"\nlabel = \"Paint\"\n[layout]\nkind = \"text\"\nwidth = {value}\n"
        );
        assert!(SetDocument::from_toml(&source).is_err(), "{value}");
    }
}
