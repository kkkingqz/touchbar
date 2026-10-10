use std::collections::BTreeMap;

use touchbar_set_config::{
    ActionDescription, InputKind, LogicDescription, MAX_EXPANDED_NODES, MAX_STATE_FIELDS,
    MAX_TEXT_BYTES, ScalarType, ScalarValue, StatePatch, StateRecord, StateType, StateValue,
};

use super::logic_bindings::touchbar::plugin::set_logic as wire;

pub fn description_from_wit(
    description: wire::LogicDescription,
) -> Result<LogicDescription, String> {
    let mut state = BTreeMap::new();
    for (name, ty) in unique(description.state)? {
        let ty = match ty {
            wire::StateType::Boolean => StateType::Bool,
            wire::StateType::Number => StateType::Number,
            wire::StateType::Text => StateType::Text,
            wire::StateType::Records(record) => StateType::List {
                key: record.key,
                fields: unique(record.fields)?
                    .into_iter()
                    .map(|(name, ty)| (name, scalar_type_from_wit(ty)))
                    .collect(),
            },
        };
        state.insert(name, ty);
    }
    let actions = unique(description.actions)?
        .into_iter()
        .map(|(name, action)| {
            (
                name,
                ActionDescription {
                    events: action.events.into_iter().map(input_from_wit).collect(),
                    value_type: action.value_type.map(scalar_type_from_wit),
                },
            )
        })
        .collect();
    let description = LogicDescription { actions, state };
    description.validate().map_err(|error| error.to_string())?;
    Ok(description)
}

pub fn patch_from_wit(patch: wire::StatePatch) -> Result<StatePatch, String> {
    let mut state = BTreeMap::new();
    let mut text_bytes = 0usize;
    let mut record_count = 0usize;
    for (name, value) in unique(patch)? {
        text_bytes += name.len();
        let value = match value {
            wire::StateValue::Boolean(value) => StateValue::Bool(value),
            wire::StateValue::Number(value) => {
                if !value.is_finite() {
                    return Err("nonfinite state number".into());
                }
                StateValue::Number(value)
            }
            wire::StateValue::Text(value) => {
                text_bytes += value.len();
                StateValue::Text(value)
            }
            wire::StateValue::Records(records) => {
                record_count += records.len();
                if record_count > MAX_EXPANDED_NODES {
                    return Err("too many state records".into());
                }
                let mut values = Vec::new();
                for record in records {
                    let mut fields = BTreeMap::new();
                    for (name, value) in unique(record.fields)? {
                        text_bytes += name.len();
                        let value = match value {
                            wire::ScalarValue::Boolean(value) => ScalarValue::Bool(value),
                            wire::ScalarValue::Number(value) => {
                                if !value.is_finite() {
                                    return Err("nonfinite record number".into());
                                }
                                ScalarValue::Number(value)
                            }
                            wire::ScalarValue::Text(value) => {
                                text_bytes += value.len();
                                ScalarValue::Text(value)
                            }
                        };
                        if text_bytes > MAX_TEXT_BYTES {
                            return Err("state text budget exceeded".into());
                        }
                        fields.insert(name, value);
                    }
                    values.push(StateRecord(fields));
                }
                StateValue::List(values)
            }
        };
        if text_bytes > MAX_TEXT_BYTES {
            return Err("state text budget exceeded".into());
        }
        state.insert(name, value);
    }
    Ok(StatePatch(state))
}

pub(super) fn unique<T>(values: Vec<(String, T)>) -> Result<BTreeMap<String, T>, String> {
    if values.len() > MAX_STATE_FIELDS {
        return Err("too many named fields".into());
    }
    let mut map = BTreeMap::new();
    for (name, value) in values {
        if map.insert(name.clone(), value).is_some() {
            return Err(format!("duplicate field {name}"));
        }
    }
    Ok(map)
}

pub(super) fn scalar_type_from_wit(ty: wire::ScalarType) -> ScalarType {
    match ty {
        wire::ScalarType::Boolean => ScalarType::Bool,
        wire::ScalarType::Number => ScalarType::Number,
        wire::ScalarType::Text => ScalarType::Text,
    }
}

pub(super) fn input_from_wit(kind: crate::bindings::touchbar::plugin::ui::InputKind) -> InputKind {
    use crate::bindings::touchbar::plugin::ui::InputKind as Source;
    match kind {
        Source::Pressed => InputKind::Pressed,
        Source::Activated => InputKind::Activated,
        Source::LongPressed => InputKind::LongPressed,
        Source::ValueChanged => InputKind::ValueChanged,
        Source::Released => InputKind::Released,
        Source::Cancelled => InputKind::Cancelled,
    }
}

pub(super) fn input_to_wit(kind: InputKind) -> crate::bindings::touchbar::plugin::ui::InputKind {
    use crate::bindings::touchbar::plugin::ui::InputKind as Target;
    match kind {
        InputKind::Pressed => Target::Pressed,
        InputKind::Activated => Target::Activated,
        InputKind::LongPressed => Target::LongPressed,
        InputKind::ValueChanged => Target::ValueChanged,
        InputKind::Released => Target::Released,
        InputKind::Cancelled => Target::Cancelled,
    }
}
