//! A logic module with no rendering or widget placement code.

use std::{cell::Cell, collections::BTreeMap};

use touchbar_logic_sdk::{
    Guest, HostEvent, PresentationEvent, RenderRequest, description_to_wit, patch_to_wit, wire,
};
use touchbar_set_config::{
    ActionDescription, InputKind, LogicDescription, StatePatch, StateType, StateValue,
};

struct Counter;

thread_local! { static COUNT: Cell<f64> = const { Cell::new(0.0) }; }

fn patch() -> wire::StatePatch {
    patch_to_wit(&StatePatch(BTreeMap::from([(
        "count".into(),
        StateValue::Number(COUNT.get()),
    )])))
}

fn unchanged() -> wire::LogicUpdate {
    wire::LogicUpdate {
        state: vec![],
        presentation: None,
    }
}

impl Guest for Counter {
    fn describe() -> wire::LogicDescription {
        description_to_wit(&LogicDescription {
            actions: BTreeMap::from([(
                "increment".into(),
                ActionDescription {
                    events: vec![InputKind::Activated],
                    value_type: None,
                },
            )]),
            state: BTreeMap::from([("count".into(), StateType::Number)]),
        })
    }

    fn initialize(item_id: String, logic_config: String) -> Result<wire::StatePatch, String> {
        if item_id.is_empty() || !matches!(logic_config.as_str(), "" | "counter") {
            return Err("invalid counter configuration".into());
        }
        COUNT.set(0.0);
        Ok(patch())
    }

    fn handle_action(event: wire::ActionEvent) -> Result<wire::LogicUpdate, String> {
        if event.action != "increment" || event.kind != wire::InputKind::Activated {
            return Err("unexpected counter action".into());
        }
        COUNT.set(COUNT.get() + 1.0);
        Ok(wire::LogicUpdate {
            state: patch(),
            presentation: None,
        })
    }

    fn handle_visibility_changed(_visible: bool) -> Result<wire::LogicUpdate, String> {
        Ok(unchanged())
    }

    fn handle_context_changed(_request: RenderRequest) -> Result<wire::LogicUpdate, String> {
        Ok(unchanged())
    }
    fn handle_presentation_event(_event: PresentationEvent) -> Result<wire::LogicUpdate, String> {
        Ok(unchanged())
    }
    fn handle_host_event(_event: HostEvent) -> Result<wire::LogicUpdate, String> {
        Ok(unchanged())
    }
}

touchbar_logic_sdk::export!(Counter);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_updates_only_on_activation() {
        Counter::initialize("counter".into(), "counter".into()).unwrap();
        let event = |kind| wire::ActionEvent {
            action: "increment".into(),
            kind,
            value: None,
            contact_id: 1,
            record_key: None,
        };
        assert!(Counter::handle_action(event(wire::InputKind::Pressed)).is_err());
        assert_eq!(COUNT.get(), 0.0);
        let update = Counter::handle_action(event(wire::InputKind::Activated)).unwrap();
        match &update.state[0].1 {
            wire::StateValue::Number(count) => assert_eq!(*count, 1.0),
            _ => panic!("counter returned the wrong state type"),
        }
    }
}
