//! Application shortcut logic. Button placement lives in sets/*.toml.
mod actions;
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
};
use touchbar_component_sdk::touchbar::plugin::broker::CapabilityStatus;
use touchbar_logic_sdk::{
    Guest, HostEvent, PresentationEvent, RenderRequest, description_to_wit, wire,
};
use touchbar_set_config::{ActionDescription, InputKind, LogicDescription, StateType};
thread_local! {
    static GRANTED: Cell<bool> = const { Cell::new(false) };
    static CONFIG: RefCell<String> = const { RefCell::new(String::new()) };
}
struct Plugin;
fn patch() -> wire::StatePatch {
    vec![(
        "shortcuts_available".into(),
        wire::StateValue::Boolean(GRANTED.get()),
    )]
}
fn unchanged() -> wire::LogicUpdate {
    wire::LogicUpdate {
        state: vec![],
        presentation: None,
    }
}
impl Guest for Plugin {
    fn describe() -> wire::LogicDescription {
        description_to_wit(&LogicDescription {
            actions: actions::BROWSER
                .iter()
                .chain(actions::FORMATTING)
                .map(|action| {
                    (
                        (*action).into(),
                        ActionDescription {
                            events: vec![InputKind::Activated],
                            value_type: None,
                        },
                    )
                })
                .collect(),
            state: BTreeMap::from([("shortcuts_available".into(), StateType::Bool)]),
        })
    }
    fn initialize(_item: String, config: String) -> Result<wire::StatePatch, String> {
        if !matches!(config.as_str(), "browser" | "formatting") {
            return Err("unknown shortcut configuration".into());
        }
        CONFIG.set(config);
        GRANTED.set(false);
        Ok(patch())
    }
    fn handle_action(event: wire::ActionEvent) -> Result<wire::LogicUpdate, String> {
        if event.kind == wire::InputKind::Activated && GRANTED.get() {
            CONFIG.with_borrow(|config| actions::press(config, &event.action));
        }
        Ok(unchanged())
    }
    fn handle_host_event(event: HostEvent) -> Result<wire::LogicUpdate, String> {
        if let HostEvent::CapabilityChanged((_, state)) = event
            && state.capability == "keyboard.shortcut.v1"
        {
            GRANTED.set(state.status == CapabilityStatus::Granted);
            return Ok(wire::LogicUpdate {
                state: patch(),
                presentation: None,
            });
        }
        Ok(unchanged())
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
}
touchbar_logic_sdk::export!(Plugin);
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shortcuts_keep_their_scoped_ids() {
        let manifest = include_str!("../touchbar-plugin.toml");
        for id in actions::BROWSER.iter().chain(actions::FORMATTING) {
            assert!(manifest.contains(&format!("id = \"{id}\"")));
        }
        assert!(actions::allowed("browser", "reload"));
        assert!(!actions::allowed("formatting", "reload"));
    }
}
