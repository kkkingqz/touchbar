//! Common native API for legacy UI components and declarative sets.
use crate::{
    Appearance, BrokerClient, ComponentPresentationEvent, ComponentUpdate, HostLimits, HostedItem,
    InputEvent, PackageAssets, PluginHost, sets::ButtonSetHost,
};
use anyhow::Result;
use std::{os::fd::RawFd, path::Path, time::Duration};
use touchbar_protocol::broker_ipc::CapabilityState;
use touchbar_ui::{Rect, RetainedUi};

pub enum PluginRuntime {
    Legacy(Box<PluginHost>),
    ButtonSet(Box<ButtonSetHost>),
}
impl From<PluginHost> for PluginRuntime {
    fn from(host: PluginHost) -> Self {
        Self::Legacy(Box::new(host))
    }
}
impl From<ButtonSetHost> for PluginRuntime {
    fn from(host: ButtonSetHost) -> Self {
        Self::ButtonSet(Box::new(host))
    }
}

macro_rules! delegate {
    ($name:ident ($($arg:ident: $ty:ty),*) -> $result:ty) => {
        pub fn $name(&mut self, $($arg: $ty),*) -> $result {
            match self { Self::Legacy(host) => host.$name($($arg),*), Self::ButtonSet(host) => host.$name($($arg),*) }
        }
    };
    (ref $name:ident () -> $result:ty) => {
        pub fn $name(&self) -> $result {
            match self { Self::Legacy(host) => host.$name(), Self::ButtonSet(host) => host.$name() }
        }
    };
}

impl PluginRuntime {
    pub fn from_bytes_with_broker(
        bytes: &[u8],
        limits: HostLimits,
        broker: Option<BrokerClient>,
    ) -> Result<Self> {
        Ok(PluginHost::from_bytes_with_broker(bytes, limits, broker)?.into())
    }
    pub fn from_file_with_broker(
        path: impl AsRef<Path>,
        limits: HostLimits,
        broker: Option<BrokerClient>,
    ) -> Result<Self> {
        Ok(PluginHost::from_file_with_broker(path, limits, broker)?.into())
    }
    delegate!(items() -> Result<Vec<HostedItem>>);
    delegate!(set_assets(assets: PackageAssets) -> ());
    delegate!(render(item: &str, viewport: Rect, appearance: Appearance) -> Result<RetainedUi>);
    delegate!(render_at(item: &str, viewport: Rect, appearance: Appearance, now: Duration) -> Result<RetainedUi>);
    delegate!(handle_event(event: &InputEvent) -> Result<ComponentUpdate>);
    delegate!(handle_presentation_event(event: ComponentPresentationEvent) -> Result<ComponentUpdate>);
    delegate!(handle_visibility_changed(item: &str, visible: bool) -> Result<bool>);
    delegate!(dispatch_broker_events() -> Result<bool>);
    delegate!(announce_capabilities() -> Result<bool>);
    delegate!(ref broker_generation() -> Option<u64>);
    delegate!(ref broker_states() -> Option<&[CapabilityState]>);
    delegate!(ref broker_event_fd() -> Option<RawFd>);
    delegate!(ref guest_render_calls() -> u64);
}
