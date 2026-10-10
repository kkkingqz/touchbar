use std::collections::BTreeMap;

use touchbar_component_sdk::{CommandRunRequest, CommandValue};
use touchbar_logic_sdk::{
    controls::{ControlOperation, PowerProfile, display_backlight, parse_brightness, parse_volume},
    description_from_wit, description_to_wit, patch_from_wit, patch_to_wit,
};
use touchbar_set_config::{
    ActionDescription, InputKind, LogicDescription, ScalarType, ScalarValue, StatePatch,
    StateRecord, StateType, StateValue,
};

#[test]
fn logic_description_roundtrip() {
    let description = LogicDescription {
        actions: BTreeMap::from([(
            "change".into(),
            ActionDescription {
                events: vec![InputKind::ValueChanged, InputKind::Released],
                value_type: Some(ScalarType::Number),
            },
        )]),
        state: BTreeMap::from([(
            "players".into(),
            StateType::List {
                key: "id".into(),
                fields: BTreeMap::from([("id".into(), ScalarType::Text)]),
            },
        )]),
    };
    assert_eq!(
        description_from_wit(description_to_wit(&description)).unwrap(),
        description
    );
    let patch = StatePatch(BTreeMap::from([(
        "players".into(),
        StateValue::List(vec![StateRecord(BTreeMap::from([(
            "id".into(),
            ScalarValue::Text("vlc".into()),
        )]))]),
    )]));
    assert_eq!(patch_from_wit(patch_to_wit(&patch)).unwrap(), patch);
}

#[test]
fn duplicate_wire_names_are_rejected() {
    let patch = StatePatch(BTreeMap::from([("count".into(), StateValue::Number(1.0))]));
    let mut wire = patch_to_wit(&patch);
    wire.push(wire[0].clone());
    assert!(patch_from_wit(wire).is_err());
    let description = LogicDescription {
        actions: BTreeMap::new(),
        state: BTreeMap::from([("count".into(), StateType::Number)]),
    };
    let mut wire = description_to_wit(&description);
    wire.state.push(wire.state[0].clone());
    assert!(description_from_wit(wire).is_err());
}

#[test]
fn engine_control_requests_are_scoped() {
    let request = ControlOperation::SetVolume(42).request().unwrap();
    assert_eq!(request.command_id, "set-volume");
    assert_eq!(
        request.values,
        vec![CommandValue::Text {
            name: "percent".into(),
            value: "42%".into()
        }]
    );
    assert_eq!(
        CommandRunRequest::decode(&request.encode().unwrap()).unwrap(),
        request
    );
    let request = ControlOperation::SetPowerProfile(PowerProfile::Performance)
        .request()
        .unwrap();
    assert_eq!(request.command_id, "set-power-profile");
    assert_eq!(
        request.values,
        vec![CommandValue::FixedEnum {
            name: "profile".into(),
            value: "performance".into()
        }]
    );
    assert!(ControlOperation::SetVolume(101).request().is_err());
    assert!(
        ControlOperation::SetBrightness {
            device: "../x".into(),
            percent: 42
        }
        .request()
        .is_err()
    );
    assert!(
        ControlOperation::SetBrightness {
            device: "appletb_backlight".into(),
            percent: 42
        }
        .request()
        .is_err()
    );
}

#[test]
fn bounded_control_parsing_preserves_values() {
    assert_eq!(parse_volume("Volume: 0.89 [MUTED]"), Some((89, true)));
    assert_eq!(parse_volume("Volume: 1.50"), Some((100, false)));
    assert_eq!(parse_volume("Volume: NaN"), None);
    assert_eq!(parse_volume(&"x".repeat(1025)), None);
    assert_eq!(
        parse_brightness("intel_backlight,backlight,600,60%,1000"),
        Some(60)
    );
    assert_eq!(
        parse_brightness(
            "intel_backlight,backlight,18446744073709551615,100%,18446744073709551615"
        ),
        Some(100)
    );
    assert_eq!(parse_brightness("intel_backlight,backlight,1,100%,0"), None);
    assert_eq!(
        display_backlight(
            "appletb_backlight,backlight,500,50%,1000\nintel_backlight,backlight,600,60%,1000"
        ),
        Some(("intel_backlight".into(), 60))
    );
}
