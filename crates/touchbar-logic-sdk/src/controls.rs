//! Shared, scoped system control requests and bounded response parsing.

use touchbar_component_sdk::{CommandRunRequest, CommandSubmitError, CommandValue};

pub const MAX_CONTROL_OUTPUT_BYTES: usize = 1024;
const TOUCH_BAR_BACKLIGHTS: &[&str] = &[
    "appletb_backlight",
    "228200000.display-pipe.0",
    "228600000.dsi.0",
];

impl PowerProfile {
    pub const fn name(self) -> &'static str {
        match self {
            Self::PowerSaver => "power-saver",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PowerProfile {
    PowerSaver,
    Balanced,
    Performance,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ControlOperation {
    ReadVolume,
    ReadBrightness,
    ReadMicrophone,
    ReadPowerProfile,
    SetVolume(u32),
    UnmuteVolume,
    ToggleMuteVolume,
    SetBrightness { device: String, percent: u32 },
    ToggleMicrophone,
    SetPowerProfile(PowerProfile),
    WatchAudio,
    WatchBrightness,
}

impl ControlOperation {
    pub fn request(&self) -> Result<CommandRunRequest, CommandSubmitError> {
        let (id, values) = match self {
            Self::ReadVolume => ("get-volume", vec![]),
            Self::ReadBrightness => ("list-brightness", vec![]),
            Self::ReadMicrophone => ("get-microphone", vec![]),
            Self::ReadPowerProfile => ("get-power-profile", vec![]),
            Self::SetVolume(percent) => ("set-volume", vec![percent_value(*percent)?]),
            Self::UnmuteVolume => ("unmute-volume", vec![]),
            // Optional named scope: existing packs do not acquire this grant.
            Self::ToggleMuteVolume => ("toggle-volume", vec![]),
            Self::SetBrightness { device, percent } => {
                if !valid_device(device) {
                    return Err(CommandSubmitError::InvalidPayload);
                }
                (
                    "set-brightness",
                    vec![
                        CommandValue::Text {
                            name: "device".into(),
                            value: device.clone(),
                        },
                        percent_value(*percent)?,
                    ],
                )
            }
            Self::ToggleMicrophone => ("toggle-microphone", vec![]),
            Self::SetPowerProfile(profile) => (
                "set-power-profile",
                vec![CommandValue::FixedEnum {
                    name: "profile".into(),
                    value: profile.name().into(),
                }],
            ),
            Self::WatchAudio => ("watch-audio", vec![]),
            Self::WatchBrightness => ("watch-brightness", vec![]),
        };
        Ok(CommandRunRequest {
            command_id: id.into(),
            values,
        })
    }
}

fn percent_value(value: u32) -> Result<CommandValue, CommandSubmitError> {
    if value > 100 {
        return Err(CommandSubmitError::InvalidPayload);
    }
    Ok(CommandValue::Text {
        name: "percent".into(),
        value: format!("{value}%"),
    })
}

fn valid_device(device: &str) -> bool {
    !device.is_empty()
        && device.len() <= 64
        && !TOUCH_BAR_BACKLIGHTS.contains(&device)
        && device
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-.:".contains(&byte))
}

pub fn parse_volume(output: &str) -> Option<(u32, bool)> {
    if output.len() > MAX_CONTROL_OUTPUT_BYTES {
        return None;
    }
    let rest = output.trim().strip_prefix("Volume:")?.trim();
    let (number, flags) = rest.split_once(' ').unwrap_or((rest, ""));
    let volume = number
        .parse::<f64>()
        .ok()
        .filter(|volume| volume.is_finite())?;
    Some((
        (volume.clamp(0.0, 1.0) * 100.0).round() as u32,
        flags.contains("[MUTED]"),
    ))
}

pub fn parse_brightness(output: &str) -> Option<u32> {
    if output.len() > MAX_CONTROL_OUTPUT_BYTES {
        return None;
    }
    let fields: Vec<_> = output.trim().split(',').collect();
    let [_, _, current, _, maximum] = fields.as_slice() else {
        return None;
    };
    let current = current.trim().parse::<u64>().ok()?;
    let maximum = maximum.trim().parse::<u64>().ok().filter(|max| *max > 0)?;
    // u128 avoids overflow on malformed/extreme values without changing rounding.
    Some(
        ((u128::from(current.min(maximum)) * 100 + u128::from(maximum) / 2) / u128::from(maximum))
            as u32,
    )
}

pub fn display_backlight(output: &str) -> Option<(String, u32)> {
    if output.len() > MAX_CONTROL_OUTPUT_BYTES {
        return None;
    }
    output.lines().find_map(|line| {
        let device = line.split(',').next()?;
        valid_device(device)
            .then(|| parse_brightness(line).map(|level| (device.to_owned(), level)))
            .flatten()
    })
}

pub fn parse_power_profile(output: &str) -> Option<PowerProfile> {
    match output.trim() {
        "power-saver" => Some(PowerProfile::PowerSaver),
        "balanced" => Some(PowerProfile::Balanced),
        "performance" => Some(PowerProfile::Performance),
        _ => None,
    }
}
