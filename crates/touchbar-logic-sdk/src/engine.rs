//! Typed entry points to the existing policy broker.

use touchbar_component_sdk::{
    CommandSubmitError, MediaCommand, MediaSubmitError, RequestId, ShortcutSubmitError,
    command_run, keyboard_shortcut, media_command, media_subscribe,
};

use crate::controls::{ControlOperation, PowerProfile};

/// A facade, not a grant: all operations still pass through the broker.
#[derive(Clone, Copy, Debug, Default)]
pub struct EngineApi;

pub const ENGINE: EngineApi = EngineApi;

pub struct KeyboardEngine;
pub struct MediaEngine;
pub struct AudioEngine;
pub struct DisplayEngine;
pub struct PowerEngine;

impl EngineApi {
    pub const fn keyboard(self) -> KeyboardEngine {
        KeyboardEngine
    }
    pub const fn media(self) -> MediaEngine {
        MediaEngine
    }
    pub const fn audio(self) -> AudioEngine {
        AudioEngine
    }
    pub const fn display(self) -> DisplayEngine {
        DisplayEngine
    }
    pub const fn power(self) -> PowerEngine {
        PowerEngine
    }
}

impl KeyboardEngine {
    pub fn shortcut(&self, id: &str) -> Result<RequestId, ShortcutSubmitError> {
        keyboard_shortcut(id)
    }
}

impl MediaEngine {
    pub fn subscribe(&self) -> Result<RequestId, MediaSubmitError> {
        media_subscribe()
    }
    pub fn command(&self, command: &MediaCommand) -> Result<RequestId, MediaSubmitError> {
        media_command(command)
    }
}

impl AudioEngine {
    pub fn read_volume(&self) -> Result<RequestId, CommandSubmitError> {
        submit(ControlOperation::ReadVolume)
    }
    pub fn set_volume(&self, percent: u32) -> Result<RequestId, CommandSubmitError> {
        submit(ControlOperation::SetVolume(percent))
    }
    pub fn unmute(&self) -> Result<RequestId, CommandSubmitError> {
        submit(ControlOperation::UnmuteVolume)
    }
    /// Requires a separately declared `toggle-volume` command scope.
    pub fn toggle_mute(&self) -> Result<RequestId, CommandSubmitError> {
        submit(ControlOperation::ToggleMuteVolume)
    }
    pub fn read_microphone(&self) -> Result<RequestId, CommandSubmitError> {
        submit(ControlOperation::ReadMicrophone)
    }
    pub fn toggle_microphone(&self) -> Result<RequestId, CommandSubmitError> {
        submit(ControlOperation::ToggleMicrophone)
    }
    pub fn watch(&self) -> Result<RequestId, CommandSubmitError> {
        submit(ControlOperation::WatchAudio)
    }
}

impl DisplayEngine {
    pub fn read_brightness(&self) -> Result<RequestId, CommandSubmitError> {
        submit(ControlOperation::ReadBrightness)
    }
    pub fn set_brightness(
        &self,
        device: &str,
        percent: u32,
    ) -> Result<RequestId, CommandSubmitError> {
        submit(ControlOperation::SetBrightness {
            device: device.into(),
            percent,
        })
    }
    pub fn watch(&self) -> Result<RequestId, CommandSubmitError> {
        submit(ControlOperation::WatchBrightness)
    }
}

impl PowerEngine {
    pub fn read_profile(&self) -> Result<RequestId, CommandSubmitError> {
        submit(ControlOperation::ReadPowerProfile)
    }
    pub fn set_profile(&self, profile: PowerProfile) -> Result<RequestId, CommandSubmitError> {
        submit(ControlOperation::SetPowerProfile(profile))
    }
}

pub fn submit(operation: ControlOperation) -> Result<RequestId, CommandSubmitError> {
    command_run(&operation.request()?)
}
