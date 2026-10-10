//! Engine-owned rendering of declarative button sets.

mod bindings;
mod renderer;

pub use renderer::CompiledSet;

mod runtime;
mod wire;
pub use runtime::ButtonSetHost;

pub mod logic_bindings {
    wasmtime::component::bindgen!({
        path: "../../wit",
        world: "button-set-logic",
        with: {
            "touchbar:plugin/ui@1.0.0": crate::bindings::touchbar::plugin::ui,
            "touchbar:plugin/broker@1.0.0": crate::bindings::touchbar::plugin::broker,
        },
    });
}
