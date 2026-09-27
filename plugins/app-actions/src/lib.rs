//! App Actions: application buttons, as macOS gives the focused application
//! its own Touch Bar controls.
//!
//! Each button presses one of the application's own shortcuts through
//! `keyboard.shortcut.v1`. The grant fixes the keys and the applications, so
//! a button can act only in the application it was made for; profiles show
//! these items only while that application is in front.

use std::cell::Cell;

use touchbar_component_sdk::kit::{
    ColorRole, Icon, ImageFit, ImageTint, InputEvent, InputKind, NodeId, Pressable, ViewBuilder,
    flexible,
};
use touchbar_component_sdk::touchbar::plugin::broker::CapabilityStatus;
use touchbar_component_sdk::{
    Guest, HostEvent, Item, PresentationEvent, RenderRequest, Update, View, keyboard_shortcut,
};

const SHORTCUTS: &str = "keyboard.shortcut.v1";

/// Buttons and the granted shortcut each presses.
const BUTTONS: &[(u64, &str)] = &[
    (10, "back"),
    (11, "forward"),
    (12, "reload"),
    (20, "bold"),
    (21, "italic"),
    (22, "underline"),
];

thread_local! {
    static GRANTED: Cell<bool> = const { Cell::new(false) };
}

struct Plugin;

impl Guest for Plugin {
    fn items() -> Vec<Item> {
        vec![
            item("browser", "Browser Navigation"),
            item("formatting", "Text Formatting"),
        ]
    }

    fn render(request: RenderRequest) -> Result<View, String> {
        let enabled = GRANTED.get();
        Ok(match request.item_id.as_str() {
            "browser" => browser(enabled),
            "formatting" => formatting(enabled),
            item => return Err(format!("unknown item {item}")),
        })
    }

    fn handle_event(event: InputEvent) -> Result<Update, String> {
        if event.kind == InputKind::Activated
            && GRANTED.get()
            && let Some(shortcut) = shortcut_for(event.widget_id)
        {
            // The application may not be in front any more; then nothing
            // is pressed and the broker says so.
            let _ = keyboard_shortcut(shortcut);
        }
        Ok(Update {
            rerender: false,
            presentation: None,
        })
    }

    fn handle_presentation_event(_event: PresentationEvent) -> Result<Update, String> {
        Ok(Update {
            rerender: false,
            presentation: None,
        })
    }

    fn handle_host_event(event: HostEvent) -> Result<Update, String> {
        let rerender = match event {
            HostEvent::CapabilityChanged((_, state)) if state.capability == SHORTCUTS => {
                let granted = state.status == CapabilityStatus::Granted;
                GRANTED.replace(granted) != granted
            }
            _ => false,
        };
        Ok(Update {
            rerender,
            presentation: None,
        })
    }
}

fn item(id: &str, label: &str) -> Item {
    Item {
        id: id.into(),
        label: label.into(),
    }
}

fn shortcut_for(widget: u64) -> Option<&'static str> {
    BUTTONS
        .iter()
        .find(|(id, _)| *id == widget)
        .map(|(_, shortcut)| *shortcut)
}

fn browser(enabled: bool) -> View {
    let mut b = ViewBuilder::new();
    let back = b.icon(Icon::ChevronLeft, tint(enabled), "Back");
    let forward = b.icon(Icon::ChevronRight, tint(enabled), "Forward");
    let reload = b.image(
        "reload",
        "Reload",
        ImageFit::Contain,
        ImageTint::Mask(tint(enabled)),
    );
    let buttons = [
        button(&mut b, 10, "Back", back, enabled),
        button(&mut b, 11, "Forward", forward, enabled),
        button(&mut b, 12, "Reload", reload, enabled),
    ];
    let root = b.row(
        buttons
            .into_iter()
            .map(|node| flexible(node, 44.0, 72.0, 120.0))
            .collect(),
        6.0,
        2.0,
    );
    b.finish(root)
}

fn formatting(enabled: bool) -> View {
    let mut b = ViewBuilder::new();
    let buttons = [
        (20, "B", "Bold"),
        (21, "I", "Italic"),
        (22, "U", "Underline"),
    ]
    .map(|(id, letter, label)| {
        let text = b.label(letter, 15.0, tint(enabled));
        button(&mut b, id, label, text, enabled)
    });
    let root = b.row(
        buttons
            .into_iter()
            .map(|node| flexible(node, 44.0, 72.0, 120.0))
            .collect(),
        6.0,
        2.0,
    );
    b.finish(root)
}

fn tint(enabled: bool) -> ColorRole {
    if enabled {
        ColorRole::Foreground
    } else {
        ColorRole::Muted
    }
}

/// Without the grant a button keeps its place, dimmed, and takes no touch.
fn button(b: &mut ViewBuilder, id: u64, label: &str, content: NodeId, enabled: bool) -> NodeId {
    if enabled {
        b.pressable(id, label, content, Pressable::control())
    } else {
        b.row(vec![flexible(content, 16.0, 24.0, 1000.0)], 0.0, 8.0)
    }
}

touchbar_component_sdk::export!(Plugin);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_button_presses_a_shortcut_the_manifest_grants() {
        let manifest = include_str!("../touchbar-plugin.toml");
        for (_, shortcut) in BUTTONS {
            assert!(
                manifest.contains(&format!("id = \"{shortcut}\"")),
                "{shortcut} is not in the manifest"
            );
        }
        assert_eq!(shortcut_for(12), Some("reload"));
        assert_eq!(shortcut_for(99), None);
    }

    #[test]
    fn both_items_render_with_and_without_the_grant() {
        for enabled in [false, true] {
            browser(enabled);
            formatting(enabled);
        }
    }
}
