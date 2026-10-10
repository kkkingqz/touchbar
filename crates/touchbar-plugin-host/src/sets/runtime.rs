//! An isolated component Store for one declarative set.
use super::{
    CompiledSet, logic_bindings::ButtonSetLogic,
    logic_bindings::touchbar::plugin::set_logic as wire,
};
use crate::{
    Appearance, BrokerClient, CallbackPhase, ComponentPresentationEndReason,
    ComponentPresentationEvent, ComponentPresentationLifecycle, ComponentUpdate, HostLimits,
    HostState, HostedItem, InputEvent, PackageAssets, activation_for_input, create_linker,
    create_store, from_wit_update, to_wit_broker_event, to_wit_capability_state, to_wit_theme, wit,
    wit_broker,
};
use anyhow::{Result, anyhow, bail};
use std::{
    os::fd::{AsRawFd, RawFd},
    time::Duration,
};
use touchbar_protocol::broker_ipc::{ActivationContext, CapabilityState};
use touchbar_set_config::{ActionEvent, ScalarValue, SetDocument, StateSnapshot};
use touchbar_ui::{Rect, RetainedUi};
use wasmtime::component::Component;
use wasmtime::error::Context as _;
use wasmtime::{Engine, Store};

pub struct ButtonSetHost {
    store: Store<HostState>,
    bindings: ButtonSetLogic,
    compiled: CompiledSet,
    state: StateSnapshot,
    limits: HostLimits,
    assets: PackageAssets,
    context: Option<(Rect, Appearance)>,
    guest_render_calls: u64,
    visible: bool,
    selected_panel: Option<String>,
}

impl ButtonSetHost {
    pub fn from_bytes_with_broker(
        bytes: &[u8],
        set: SetDocument,
        limits: HostLimits,
        broker: Option<BrokerClient>,
    ) -> Result<Self> {
        set.validate()?;
        let (engine, store) = create_store(limits, broker)?;
        let component = Component::new(&engine, bytes).context("compile set logic")?;
        Self::instantiate(&engine, store, &component, set, limits)
    }
    fn instantiate(
        engine: &Engine,
        mut store: Store<HostState>,
        component: &Component,
        set: SetDocument,
        limits: HostLimits,
    ) -> Result<Self> {
        // Imports are the very same broker ABI as legacy components. The
        // separate world changes exports, never ambient capabilities.
        let linker = create_linker(engine)?;
        let instance = linker.instantiate(&mut store, component)?;
        let bindings = ButtonSetLogic::new(&mut store, &instance)?;
        store.data_mut().phase = Some(CallbackPhase::Items);
        store.set_fuel(limits.fuel_per_call)?;
        let description = bindings.call_describe(&mut store);
        store.data_mut().phase = None;
        let description =
            super::wire::description_from_wit(description?).map_err(|message| anyhow!(message))?;
        let compiled = CompiledSet::new(set, &description, limits)?;
        store.data_mut().phase = Some(CallbackPhase::Items);
        store.set_fuel(limits.fuel_per_call)?;
        let patch = bindings.call_initialize(
            &mut store,
            &compiled.document.id,
            &compiled.document.logic_config,
        );
        store.data_mut().phase = None;
        let patch =
            patch?.map_err(|message| anyhow!("logic rejected initialization: {message}"))?;
        let patch = super::wire::patch_from_wit(patch).map_err(|message| anyhow!(message))?;
        let mut state = StateSnapshot::default();
        state.apply(&patch, &description)?;
        Ok(Self {
            store,
            bindings,
            compiled,
            state,
            limits,
            assets: PackageAssets::default(),
            context: None,
            guest_render_calls: 0,
            visible: true,
            selected_panel: None,
        })
    }
    pub fn snapshot(&self) -> &StateSnapshot {
        &self.state
    }
    pub fn set_assets(&mut self, assets: PackageAssets) {
        self.assets = assets;
    }
    pub fn items(&mut self) -> Result<Vec<HostedItem>> {
        Ok(vec![HostedItem {
            id: self.compiled.document.id.clone(),
            label: self.compiled.document.label.clone(),
        }])
    }
    fn check_item(&self, item: &str) -> Result<()> {
        if item != self.compiled.document.id {
            bail!("item belongs to another set");
        }
        Ok(())
    }
    pub fn render(
        &mut self,
        item: &str,
        viewport: Rect,
        appearance: Appearance,
    ) -> Result<RetainedUi> {
        self.render_at(item, viewport, appearance, Duration::ZERO)
    }
    pub fn render_at(
        &mut self,
        item: &str,
        viewport: Rect,
        appearance: Appearance,
        now: Duration,
    ) -> Result<RetainedUi> {
        self.check_item(item)?;
        crate::validate_viewport(viewport)?;
        if self.context != Some((viewport, appearance)) {
            self.refuel()?;
            self.store.data_mut().phase = Some(CallbackPhase::Render);
            let request = wit::RenderRequest {
                item_id: item.into(),
                viewport: wit::Viewport {
                    width: viewport.width,
                    height: viewport.height,
                    scale: 1.0,
                },
                theme: to_wit_theme(appearance),
            };
            self.guest_render_calls = self.guest_render_calls.saturating_add(1);
            let result = self
                .bindings
                .call_handle_context_changed(&mut self.store, &request);
            self.store.data_mut().phase = None;
            self.apply_update(result?.map_err(|message| anyhow!(message))?, false)?;
            self.context = Some((viewport, appearance));
        }
        // Each pass can cancel captured contacts only once. Rebuild after their
        // patches so the returned scene and routing agree with authoritative state.
        for _ in 0..=super::bindings::MAX_CONTACTS {
            let ui = if let Some(panel) = &self.selected_panel {
                self.compiled.render_panel(
                    panel,
                    &self.state,
                    viewport,
                    appearance,
                    now,
                    &self.assets,
                )?
            } else {
                self.compiled
                    .render(&self.state, viewport, appearance, now, &self.assets)?
            };
            if !self.deliver_cancelled()? {
                return Ok(ui);
            }
        }
        bail!("cancellation patches did not converge within contact budget")
    }

    fn deliver_cancelled(&mut self) -> Result<bool> {
        let mut changed = false;
        for event in self.compiled.drain_cancelled() {
            changed |= self.deliver_action(event, None, false)?.rerender;
        }
        Ok(changed)
    }
    pub fn guest_render_calls(&self) -> u64 {
        self.guest_render_calls
    }
    fn apply_update(
        &mut self,
        update: wire::LogicUpdate,
        presentation_allowed: bool,
    ) -> Result<ComponentUpdate> {
        if update.presentation.is_some() && !presentation_allowed {
            bail!("logic requested presentation outside physical input");
        }
        let mut converted = from_wit_update(wit::Update {
            rerender: false,
            presentation: update.presentation,
        })?;
        let patch =
            super::wire::patch_from_wit(update.state).map_err(|message| anyhow!(message))?;
        converted.rerender = self.state.apply(&patch, &self.compiled.description)?;
        Ok(converted)
    }
    fn deliver_action(
        &mut self,
        event: ActionEvent,
        activation: Option<ActivationContext>,
        presentation_allowed: bool,
    ) -> Result<ComponentUpdate> {
        self.refuel()?;
        self.store.data_mut().phase = Some(CallbackPhase::Input);
        self.store.data_mut().activation = activation;
        let event = wire::ActionEvent {
            action: event.action,
            kind: super::wire::input_to_wit(event.kind),
            value: event.value.map(|value| match value {
                ScalarValue::Bool(value) => wire::ScalarValue::Boolean(value),
                ScalarValue::Number(value) => wire::ScalarValue::Number(value),
                ScalarValue::Text(value) => wire::ScalarValue::Text(value),
            }),
            contact_id: event.contact_id,
            record_key: event.record_key,
        };
        let result = self.bindings.call_handle_action(&mut self.store, &event);
        self.store.data_mut().phase = None;
        self.store.data_mut().activation = None;
        self.apply_update(
            result?.map_err(|message| anyhow!("logic rejected action: {message}"))?,
            presentation_allowed,
        )
    }
    pub fn handle_event(&mut self, event: &InputEvent) -> Result<ComponentUpdate> {
        self.check_item(&event.item_id)?;
        if !self.visible {
            return Ok(ComponentUpdate {
                rerender: false,
                presentation: None,
            });
        }
        let Some(action) = self.compiled.action_for(event)? else {
            return Ok(ComponentUpdate {
                rerender: false,
                presentation: None,
            });
        };
        let activation = activation_for_input(
            self.store.data().surface_instance,
            event,
            self.limits.activation_lifetime,
        )?;
        self.deliver_action(action, activation, true)
    }
    pub fn handle_visibility_changed(&mut self, item: &str, visible: bool) -> Result<bool> {
        self.check_item(item)?;
        let mut changed = false;
        if !visible {
            self.compiled.cancel_contacts()?;
            changed |= self.deliver_cancelled()?;
            changed |= self.selected_panel.take().is_some();
        }
        self.visible = visible;
        self.refuel()?;
        self.store.data_mut().phase = Some(CallbackPhase::Lifecycle);
        let result = self
            .bindings
            .call_handle_visibility_changed(&mut self.store, visible);
        self.store.data_mut().phase = None;
        Ok(changed
            | self
                .apply_update(result?.map_err(|message| anyhow!(message))?, false)?
                .rerender)
    }
    pub fn handle_presentation_event(
        &mut self,
        event: ComponentPresentationEvent,
    ) -> Result<ComponentUpdate> {
        let mut changed = false;
        let panel = match event {
            ComponentPresentationEvent::Started
            | ComponentPresentationEvent::StartedWithLifecycle(
                ComponentPresentationLifecycle::Persistent,
            ) => Some(self.compiled.document.expanded_panel.clone()),
            ComponentPresentationEvent::StartedWithLifecycle(
                ComponentPresentationLifecycle::Transient,
            ) => Some(self.compiled.document.press_and_hold_panel.clone()),
            ComponentPresentationEvent::Ended(_) => Some(None),
            _ => None,
        };
        if let Some(panel) = panel {
            changed |= self.selected_panel != panel;
            self.selected_panel = panel;
        }
        if matches!(event, ComponentPresentationEvent::Ended(_)) {
            self.compiled.cancel_contacts()?;
            changed |= self.deliver_cancelled()?;
        }
        self.refuel()?;
        self.store.data_mut().phase = Some(CallbackPhase::HostEvent);
        let event = match event {
            ComponentPresentationEvent::Anchor { x, width } => {
                wit::PresentationEvent::Anchor(wit::PresentationAnchor { x, width })
            }
            ComponentPresentationEvent::Started
            | ComponentPresentationEvent::StartedWithLifecycle(_) => {
                wit::PresentationEvent::Started
            }
            ComponentPresentationEvent::Ended(reason) => {
                wit::PresentationEvent::Ended(match reason {
                    ComponentPresentationEndReason::Requested => {
                        wit::PresentationEndReason::Requested
                    }
                    ComponentPresentationEndReason::Selection => {
                        wit::PresentationEndReason::Selection
                    }
                    ComponentPresentationEndReason::OutsidePress => {
                        wit::PresentationEndReason::OutsidePress
                    }
                    ComponentPresentationEndReason::Timeout => wit::PresentationEndReason::Timeout,
                    ComponentPresentationEndReason::SourceHidden => {
                        wit::PresentationEndReason::SourceHidden
                    }
                    ComponentPresentationEndReason::Replaced => {
                        wit::PresentationEndReason::Replaced
                    }
                    ComponentPresentationEndReason::Rejected => {
                        wit::PresentationEndReason::Rejected
                    }
                })
            }
        };
        let result = self
            .bindings
            .call_handle_presentation_event(&mut self.store, event);
        self.store.data_mut().phase = None;
        let mut update = self.apply_update(result?.map_err(|message| anyhow!(message))?, false)?;
        update.rerender |= changed;
        Ok(update)
    }
    pub fn broker_generation(&self) -> Option<u64> {
        self.store
            .data()
            .broker
            .as_ref()
            .map(BrokerClient::generation)
    }

    pub fn broker_states(&self) -> Option<&[CapabilityState]> {
        self.store.data().broker.as_ref().map(BrokerClient::states)
    }

    /// Descriptor used only by the trusted native event loop. It is never
    /// inserted into WASI or exposed as a guest resource.
    pub fn broker_event_fd(&self) -> Option<RawFd> {
        self.store
            .data()
            .broker
            .as_ref()
            .map(|broker| broker.channel.as_raw_fd())
    }

    /// Delivers every broker event currently queued to an event-aware guest.
    /// A bounded batch preserves fairness with Wayland input and rendering.
    pub fn dispatch_broker_events(&mut self) -> Result<bool> {
        let mut rerender = false;
        for _ in 0..64 {
            let event = {
                let state = self.store.data_mut();
                let Some(broker) = state.broker.as_mut() else {
                    bail!("component broker is unavailable");
                };
                broker.try_receive_event()?
            };
            let Some(event) = event else {
                break;
            };
            rerender |= self.deliver_host_event(&to_wit_broker_event(event))?;
        }
        Ok(rerender)
    }

    /// Reports the initial state of every capability as a capability-change
    /// event, once, before the first render. Broker work may start only in
    /// input and host-event callbacks, so without this a component could not
    /// open a subscription until the first touch.
    pub fn announce_capabilities(&mut self) -> Result<bool> {
        let Some(broker) = self.store.data().broker.as_ref() else {
            return Ok(false);
        };
        let generation = broker.generation();
        let events = broker
            .states()
            .iter()
            .map(|state| {
                wit_broker::HostEvent::CapabilityChanged((
                    generation,
                    to_wit_capability_state(state),
                ))
            })
            .collect::<Vec<_>>();
        let mut rerender = false;
        for event in &events {
            rerender |= self.deliver_host_event(event)?;
        }
        Ok(rerender)
    }

    fn deliver_host_event(&mut self, event: &wit_broker::HostEvent) -> Result<bool> {
        self.refuel()?;
        self.store.data_mut().phase = Some(CallbackPhase::HostEvent);
        let result = self
            .bindings
            .call_handle_host_event(&mut self.store, event)
            .context("component host-event call failed");
        self.store.data_mut().phase = None;
        let update = result?.map_err(|message| anyhow!("logic rejected host event: {message}"))?;
        Ok(self.apply_update(update, false)?.rerender)
    }

    fn refuel(&mut self) -> Result<()> {
        Ok(self
            .store
            .set_fuel(self.limits.fuel_per_call)
            .context("reset component instruction budget")?)
    }
}
