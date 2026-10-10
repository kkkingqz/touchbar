use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::schema::validate_name;
use crate::{MAX_EXPANDED_NODES, MAX_ID_BYTES, MAX_STATE_FIELDS, MAX_TEXT_BYTES};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputKind {
    Pressed,
    Activated,
    LongPressed,
    ValueChanged,
    Released,
    Cancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ScalarType {
    Bool,
    Number,
    Text,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ScalarValue {
    Bool(bool),
    Number(f64),
    Text(String),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StateType {
    Bool,
    Number,
    Text,
    List {
        key: String,
        fields: BTreeMap<String, ScalarType>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StateRecord(pub BTreeMap<String, ScalarValue>);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StateValue {
    Bool(bool),
    Number(f64),
    Text(String),
    List(Vec<StateRecord>),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StateSnapshot(pub BTreeMap<String, StateValue>);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct StatePatch(pub BTreeMap<String, StateValue>);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDescription {
    pub events: Vec<InputKind>,
    pub value_type: Option<ScalarType>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LogicDescription {
    pub actions: BTreeMap<String, ActionDescription>,
    pub state: BTreeMap<String, StateType>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ActionEvent {
    pub action: String,
    pub kind: InputKind,
    pub value: Option<ScalarValue>,
    pub contact_id: u64,
    /// Key of a record selected through a repeated layout, if any.
    pub record_key: Option<String>,
}

impl StateSnapshot {
    pub fn validate(&self, description: &LogicDescription) -> Result<()> {
        description.validate()?;
        if self.0.len() > MAX_STATE_FIELDS {
            bail!("too many state fields");
        }
        let mut budget = StateBudget::default();
        for (name, value) in &self.0 {
            let ty = description
                .state
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("undeclared state field {name}"))?;
            budget.text(name)?;
            validate_value(name, value, ty, &mut budget)?;
        }
        Ok(())
    }

    /// Check the complete candidate snapshot before publishing any update.
    pub fn apply(&mut self, patch: &StatePatch, description: &LogicDescription) -> Result<bool> {
        description.validate()?;
        if self.0.len() > MAX_STATE_FIELDS || patch.0.len() > MAX_STATE_FIELDS {
            bail!("too many state fields");
        }
        let mut budget = StateBudget::default();
        // Validate borrowed values before cloning untrusted guest data.
        for (name, value) in self
            .0
            .iter()
            .filter(|(name, _)| !patch.0.contains_key(*name))
            .chain(patch.0.iter())
        {
            let ty = description
                .state
                .get(name)
                .ok_or_else(|| anyhow::anyhow!("undeclared state field {name}"))?;
            budget.text(name)?;
            validate_value(name, value, ty, &mut budget)?;
        }
        let changed = patch
            .0
            .iter()
            .any(|(name, value)| self.0.get(name) != Some(value));
        if changed {
            self.0.extend(patch.0.clone());
        }
        Ok(changed)
    }
}

impl LogicDescription {
    pub fn validate(&self) -> Result<()> {
        if self.actions.len() > MAX_STATE_FIELDS || self.state.len() > MAX_STATE_FIELDS {
            bail!("logic description exceeds its field/action budget");
        }
        let mut text = 0usize;
        for (name, action) in &self.actions {
            validate_name(name)?;
            text += name.len();
            if action.events.is_empty() || action.events.len() > 6 {
                bail!("action {name} must declare between one and six input kinds");
            }
            for (index, event) in action.events.iter().enumerate() {
                if action.events[..index].contains(event) {
                    bail!("action {name} declares a duplicate input kind");
                }
            }
        }
        for (name, ty) in &self.state {
            validate_name(name)?;
            text += name.len();
            if let StateType::List { key, fields } = ty {
                validate_name(key)?;
                if fields.is_empty() || fields.len() > MAX_STATE_FIELDS {
                    bail!("list {name} has too many fields or none");
                }
                if fields.get(key) != Some(&ScalarType::Text) {
                    bail!("list {name} key must name a declared text field");
                }
                for field in fields.keys() {
                    validate_name(field)?;
                    text += field.len();
                }
            }
        }
        if text > MAX_TEXT_BYTES {
            bail!("logic description exceeds its text budget");
        }
        Ok(())
    }
}

#[derive(Default)]
struct StateBudget {
    text: usize,
    records: usize,
}

impl StateBudget {
    fn text(&mut self, text: &str) -> Result<()> {
        self.text = self
            .text
            .checked_add(text.len())
            .ok_or_else(|| anyhow::anyhow!("state text budget overflow"))?;
        if self.text > MAX_TEXT_BYTES {
            bail!("state exceeds its text budget");
        }
        Ok(())
    }
}

fn validate_value(
    name: &str,
    value: &StateValue,
    ty: &StateType,
    budget: &mut StateBudget,
) -> Result<()> {
    match (value, ty) {
        (StateValue::Bool(_), StateType::Bool) => Ok(()),
        (StateValue::Number(value), StateType::Number) if value.is_finite() => Ok(()),
        (StateValue::Text(text), StateType::Text) => budget.text(text),
        (StateValue::List(records), StateType::List { key, fields }) => {
            budget.records = budget
                .records
                .checked_add(records.len())
                .ok_or_else(|| anyhow::anyhow!("state record budget overflow"))?;
            if budget.records > MAX_EXPANDED_NODES {
                bail!("state exceeds its record budget");
            }
            let mut keys = BTreeSet::new();
            for StateRecord(record) in records {
                if record.len() != fields.len() {
                    bail!("list {name} record has missing or extra fields");
                }
                let Some(ScalarValue::Text(id)) = record.get(key) else {
                    bail!("list {name} record lacks its text key");
                };
                if id.is_empty()
                    || id.len() > MAX_ID_BYTES
                    || id.chars().any(char::is_control)
                    || !keys.insert(id)
                {
                    bail!("list {name} has an invalid or duplicate key");
                }
                for (field, value) in record {
                    budget.text(field)?;
                    match (value, fields.get(field)) {
                        (ScalarValue::Bool(_), Some(ScalarType::Bool)) => {}
                        (ScalarValue::Number(number), Some(ScalarType::Number))
                            if number.is_finite() => {}
                        (ScalarValue::Text(text), Some(ScalarType::Text)) => budget.text(text)?,
                        _ => bail!(
                            "list {name} field {field} has the wrong type or a nonfinite number"
                        ),
                    }
                }
            }
            Ok(())
        }
        _ => bail!("state field {name} has the wrong type or a nonfinite number"),
    }
}
