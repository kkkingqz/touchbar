//! Omarchy state and bounded drawing records. Effects and motion live in sets/.
use std::cell::Cell;
use std::collections::BTreeMap;
use touchbar_component_sdk::bindings::touchbar::plugin::ui::{MotionPolicy, Rgba, Theme};
use touchbar_component_sdk::{HostEvent, PresentationEvent, RenderRequest};
use touchbar_logic_sdk::{Guest, description_to_wit, patch_to_wit, wire};
use touchbar_set_config::{
    ActionDescription, InputKind, LogicDescription, ScalarType, StatePatch, StateType, StateValue,
};
struct Omarchy;
thread_local! { static ANIMATED: Cell<bool> = const { Cell::new(true) }; static AWAKE: Cell<bool> = const { Cell::new(false) };  }
const MIN_ACCENT_LUMINANCE: f32 = 0.28;
const MIN_FOREGROUND_LUMINANCE: f32 = 0.55;

const OPAQUE_BLACK: Rgba = Rgba {
    red: 0.0,
    green: 0.0,
    blue: 0.0,
    alpha: 1.0,
};

const WORDMARK_WIDTH: usize = 81;
const WORDMARK_HEIGHT: usize = 19;

/// The bitmap used by the homepage effect. Keeping it as rows rather than an
/// image lets every horizontal run resolve through a live semantic theme paint.
const WORDMARK: [&str; WORDMARK_HEIGHT] = [
    "000000000000000001110000000000000000000000000000000000000000000000000000000000000",
    "001111100000011111111111000000111111100001111111000011111110000100010000001000100",
    "011111110000111111111111100001111111100011111111000111111110001100011000011000110",
    "111000111001110001110001110011100011100111000111001110001110011100011100111000111",
    "111000111001110001110001110011100011100111000111001110001110011100011100111000111",
    "111000111001110001110001110011100011100111000111001110001100011100011100111000111",
    "111000111001110001110001110011100011100111000111001110001000011100011100111000111",
    "111000111001110001110001110011100011100111000111001110000000011100011100111000111",
    "111000111001110001110001110111111111101111111110001110000000111111111110111111111",
    "111000111001110001110001110111111111101111111100001110000001111111111100111111111",
    "111000111001110001110001110011100011100111000000001110000000011100011100000000111",
    "111000111001110001110001110011100011101111111111001110001000011100011100011000111",
    "111000111001110001110001110011100011101111111111001110001100011100011100111000111",
    "111000111001110001110001110011100011100111000111001110001110011100011100111000111",
    "111000111001110001110001110011100011100111000111001110001110011100011100111000111",
    "011111110000110001110001100011100011000111000111001111111100011100011000011111110",
    "001111100000010001110001000011100010000111000111001111111000011100010000001111100",
    "000000000000000000000000000000000000000111000110000000000000000000000000000000000",
    "000000000000000000000000000000000000000111000100000000000000000000000000000000000",
];

struct OledPalette {
    background: Rgba,
    accent: Rgba,
    foreground: Rgba,
    accent_lift: f32,
    foreground_lift: f32,
}

fn oled_palette(accent: &Rgba, foreground: &Rgba) -> OledPalette {
    let (accent, accent_lift) = lift_luminance(accent, MIN_ACCENT_LUMINANCE);
    let (foreground, foreground_lift) = lift_luminance(foreground, MIN_FOREGROUND_LUMINANCE);
    OledPalette {
        background: copy_color(&OPAQUE_BLACK),
        accent,
        foreground,
        accent_lift,
        foreground_lift,
    }
}

/// Find the smallest sRGB interpolation toward white that reaches the desired
/// contrast. The interpolation factor is also sent to the bounded shader so
/// retained canvas nodes and GPU effects use the exact same projected color.
fn lift_luminance(color: &Rgba, minimum: f32) -> (Rgba, f32) {
    if relative_luminance(color) >= minimum {
        return (copy_color(color), 0.0);
    }
    let mut low = 0.0;
    let mut high = 1.0;
    for _ in 0..16 {
        let middle = (low + high) * 0.5;
        if relative_luminance(&mix_with_white(color, middle)) < minimum {
            low = middle;
        } else {
            high = middle;
        }
    }
    (mix_with_white(color, high), high)
}

fn mix_with_white(color: &Rgba, amount: f32) -> Rgba {
    Rgba {
        red: color.red + (1.0 - color.red) * amount,
        green: color.green + (1.0 - color.green) * amount,
        blue: color.blue + (1.0 - color.blue) * amount,
        alpha: color.alpha,
    }
}

fn copy_color(color: &Rgba) -> Rgba {
    Rgba {
        red: color.red,
        green: color.green,
        blue: color.blue,
        alpha: color.alpha,
    }
}

fn relative_luminance(color: &Rgba) -> f32 {
    linear_luminance([
        srgb_to_linear(color.red),
        srgb_to_linear(color.green),
        srgb_to_linear(color.blue),
    ])
}

fn linear_luminance(color: [f32; 3]) -> f32 {
    color[0] * 0.2126 + color[1] * 0.7152 + color[2] * 0.0722
}

fn srgb_to_linear(channel: f32) -> f32 {
    let channel = channel.clamp(0.0, 1.0);
    if channel <= 0.04045 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

#[derive(Clone, Copy)]
struct WordmarkGeometry {
    cell: f32,
    left: f32,
    top: f32,
}

/// Fit whole grid cells to the strip. A full-height Touch Bar gets a 3px pitch;
/// narrow slots shrink only when the 81-column wordmark would no longer fit.
fn wordmark_geometry(width: f32, height: f32) -> WordmarkGeometry {
    let height_pitch = (height / (WORDMARK_HEIGHT as f32 + 1.0)).floor().max(1.0);
    let width_pitch = width / (WORDMARK_WIDTH as f32 + 4.0);
    let cell = height_pitch.min(width_pitch).max(0.75);
    let mark_width = cell * WORDMARK_WIDTH as f32;
    let mark_height = cell * WORDMARK_HEIGHT as f32;
    WordmarkGeometry {
        cell,
        left: ((width - mark_width) * 0.5).floor(),
        top: ((height - mark_height) * 0.5).floor(),
    }
}

fn drift(width: f32) -> f32 {
    (width * 0.012).clamp(0.0, 24.0)
}
fn rgba_rect(
    id: usize,
    x: f32,
    y: f32,
    width: f32,
    height: f32,
    color: &Rgba,
    opacity: f32,
) -> wire::StateRecord {
    wire::StateRecord {
        fields: vec![
            ("id".into(), wire::ScalarValue::Text(id.to_string())),
            ("op".into(), wire::ScalarValue::Text("rect".into())),
            ("x".into(), wire::ScalarValue::Number(f64::from(x))),
            ("y".into(), wire::ScalarValue::Number(f64::from(y))),
            ("width".into(), wire::ScalarValue::Number(f64::from(width))),
            (
                "height".into(),
                wire::ScalarValue::Number(f64::from(height)),
            ),
            ("r".into(), wire::ScalarValue::Number(f64::from(color.red))),
            (
                "g".into(),
                wire::ScalarValue::Number(f64::from(color.green)),
            ),
            ("b".into(), wire::ScalarValue::Number(f64::from(color.blue))),
            (
                "a".into(),
                wire::ScalarValue::Number(f64::from(color.alpha)),
            ),
            (
                "opacity".into(),
                wire::ScalarValue::Number(f64::from(opacity)),
            ),
        ],
    }
}
fn context(item: &str, width: f32, height: f32, theme: &Theme) -> wire::StatePatch {
    let width = width.max(1.0);
    let height = height.max(1.0);
    let geometry = wordmark_geometry(width, height);
    let colors = oled_palette(&theme.accent, &theme.foreground);
    let screensaver = item == "screensaver";
    let mut mark = Vec::new();
    if screensaver {
        for (row_index, row) in WORDMARK.iter().enumerate() {
            let (accent_opacity, lift_opacity) = match row_index {
                0..=4 => (1.0, 0.62),
                5..=6 => (1.0, 0.34),
                7..=10 => (1.0, 0.0),
                11..=13 => (0.66, 0.0),
                _ => (0.34, 0.0),
            };
            let bytes = row.as_bytes();
            let mut col = 0;
            while col < WORDMARK_WIDTH {
                if bytes[col] != b'1' {
                    col += 1;
                    continue;
                }
                let start = col;
                while col < WORDMARK_WIDTH && bytes[col] == b'1' {
                    col += 1;
                }
                let x = geometry.left + start as f32 * geometry.cell;
                let y = geometry.top + row_index as f32 * geometry.cell;
                let run_width = (col - start) as f32 * geometry.cell;
                mark.push(rgba_rect(
                    mark.len(),
                    x,
                    y,
                    run_width,
                    geometry.cell,
                    &colors.accent,
                    accent_opacity,
                ));
                if lift_opacity > 0.0 {
                    mark.push(rgba_rect(
                        mark.len(),
                        x,
                        y,
                        run_width,
                        geometry.cell,
                        &colors.foreground,
                        lift_opacity,
                    ));
                }
            }
        }
    }
    let rule_height = geometry.cell.clamp(1.0, 3.0);
    let palette = if !screensaver {
        palette_records(width)
    } else {
        vec![]
    };
    let awake = AWAKE.get();
    let animated = theme.motion == MotionPolicy::Full;
    ANIMATED.set(animated);
    let mut fields = vec![
        (
            "ground".into(),
            wire::StateValue::Records(if screensaver {
                vec![rgba_rect(
                    0,
                    0.0,
                    0.0,
                    width,
                    height,
                    &colors.background,
                    1.0,
                )]
            } else {
                vec![]
            }),
        ),
        ("mark".into(), wire::StateValue::Records(mark)),
        (
            "hairline".into(),
            wire::StateValue::Records(if screensaver {
                vec![rgba_rect(
                    0,
                    0.0,
                    ((height - rule_height) * 0.5).floor(),
                    width,
                    rule_height,
                    &colors.accent,
                    1.0,
                )]
            } else {
                vec![]
            }),
        ),
        ("palette".into(), wire::StateValue::Records(palette)),
        ("resting".into(), wire::StateValue::Boolean(!awake)),
        (
            "waking_animated".into(),
            wire::StateValue::Boolean(awake && animated),
        ),
        (
            "waking_reduced".into(),
            wire::StateValue::Boolean(awake && !animated),
        ),
    ];
    for (name, value) in [
        ("cell", geometry.cell),
        ("left", geometry.left),
        ("top", geometry.top),
        ("background_r", colors.background.red),
        ("background_g", colors.background.green),
        ("background_b", colors.background.blue),
        ("accent_lift", colors.accent_lift),
        ("foreground_lift", colors.foreground_lift),
        ("drift", drift(width)),
        ("negative_drift", -drift(width)),
    ] {
        fields.push((name.into(), wire::StateValue::Number(f64::from(value))));
    }
    fields
}
fn palette_records(width: f32) -> Vec<wire::StateRecord> {
    let roles = [
        ("background", "bg"),
        ("control", "control"),
        ("track", "track"),
        ("muted", "muted"),
        ("foreground", "fg"),
        ("accent", "accent"),
        ("destructive", "red"),
    ];
    let labelled = width >= 640.0;
    let height = if labelled { 11.0 } else { 15.0 };
    let step = 100.0 / roles.len() as f64;
    let mut records = Vec::new();
    for (index, (role, label)) in roles.iter().enumerate() {
        let x = index as f64 * step + 1.0;
        let swatch = step - 2.0;
        for (op, rx, y, w, h, radius, color, opacity, text, size) in [
            (
                "rect",
                x - 0.5,
                2.0,
                swatch + 1.0,
                height + 1.0,
                2.4,
                "muted",
                0.45,
                "",
                3.4,
            ),
            ("rect", x, 2.5, swatch, height, 2.0, *role, 1.0, "", 3.4),
            ("text", x, 15.0, swatch, 4.5, 0.0, "muted", 1.0, *label, 3.4),
        ] {
            if op == "text" && !labelled {
                continue;
            }
            records.push(wire::StateRecord {
                fields: vec![
                    (
                        "id".into(),
                        wire::ScalarValue::Text(records.len().to_string()),
                    ),
                    ("op".into(), wire::ScalarValue::Text(op.into())),
                    ("color".into(), wire::ScalarValue::Text(color.into())),
                    ("text".into(), wire::ScalarValue::Text(text.into())),
                    ("x".into(), wire::ScalarValue::Number(rx)),
                    ("y".into(), wire::ScalarValue::Number(y)),
                    ("width".into(), wire::ScalarValue::Number(w)),
                    ("height".into(), wire::ScalarValue::Number(h)),
                    ("radius".into(), wire::ScalarValue::Number(radius)),
                    ("opacity".into(), wire::ScalarValue::Number(opacity)),
                    ("size".into(), wire::ScalarValue::Number(size)),
                ],
            });
        }
    }
    records
}
impl Guest for Omarchy {
    fn describe() -> wire::LogicDescription {
        let mut d = LogicDescription::default();
        for name in ["ground", "mark", "hairline", "palette"] {
            let mut fields = BTreeMap::from([
                ("id".into(), ScalarType::Text),
                ("op".into(), ScalarType::Text),
            ]);
            for name in ["x", "y", "width", "height", "opacity"] {
                fields.insert(name.into(), ScalarType::Number);
            }
            if name == "palette" {
                for name in ["color", "text"] {
                    fields.insert(name.into(), ScalarType::Text);
                }
                for name in ["radius", "size"] {
                    fields.insert(name.into(), ScalarType::Number);
                }
            } else {
                for name in ["r", "g", "b", "a"] {
                    fields.insert(name.into(), ScalarType::Number);
                }
            }
            d.state.insert(
                name.into(),
                StateType::List {
                    key: "id".into(),
                    fields,
                },
            );
        }
        for name in ["resting", "waking_animated", "waking_reduced"] {
            d.state.insert(name.into(), StateType::Bool);
        }
        for name in [
            "cell",
            "left",
            "top",
            "background_r",
            "background_g",
            "background_b",
            "accent_lift",
            "foreground_lift",
            "drift",
            "negative_drift",
        ] {
            d.state.insert(name.into(), StateType::Number);
        }
        d.actions.insert(
            "wake".into(),
            ActionDescription {
                events: vec![InputKind::Activated],
                value_type: None,
            },
        );
        description_to_wit(&d)
    }
    fn initialize(_item: String, _config: String) -> Result<wire::StatePatch, String> {
        let mut p = StatePatch::default();
        for name in ["ground", "mark", "hairline", "palette"] {
            p.0.insert(name.into(), StateValue::List(vec![]));
        }
        for name in ["resting", "waking_animated", "waking_reduced"] {
            p.0.insert(name.into(), StateValue::Bool(name == "resting"));
        }
        for name in [
            "cell",
            "left",
            "top",
            "background_r",
            "background_g",
            "background_b",
            "accent_lift",
            "foreground_lift",
            "drift",
            "negative_drift",
        ] {
            p.0.insert(name.into(), StateValue::Number(0.0));
        }

        Ok(patch_to_wit(&p))
    }
    fn handle_context_changed(request: RenderRequest) -> Result<wire::LogicUpdate, String> {
        let p = context(
            &request.item_id,
            request.viewport.width,
            request.viewport.height,
            &request.theme,
        );

        Ok(wire::LogicUpdate {
            state: p,
            presentation: None,
        })
    }
    fn handle_action(event: wire::ActionEvent) -> Result<wire::LogicUpdate, String> {
        if event.action != "wake" || event.kind != wire::InputKind::Activated {
            return Ok(idle());
        }
        AWAKE.set(!AWAKE.get());
        let awake = AWAKE.get();
        let animated = ANIMATED.get();
        Ok(wire::LogicUpdate {
            state: vec![
                ("resting".into(), wire::StateValue::Boolean(!awake)),
                (
                    "waking_animated".into(),
                    wire::StateValue::Boolean(awake && animated),
                ),
                (
                    "waking_reduced".into(),
                    wire::StateValue::Boolean(awake && !animated),
                ),
            ],
            presentation: None,
        })
    }
    fn handle_visibility_changed(_visible: bool) -> Result<wire::LogicUpdate, String> {
        Ok(idle())
    }
    fn handle_presentation_event(_event: PresentationEvent) -> Result<wire::LogicUpdate, String> {
        Ok(idle())
    }
    fn handle_host_event(_event: HostEvent) -> Result<wire::LogicUpdate, String> {
        Ok(idle())
    }
}
fn idle() -> wire::LogicUpdate {
    wire::LogicUpdate {
        state: vec![],
        presentation: None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(red: u8, green: u8, blue: u8) -> Rgba {
        Rgba {
            red: f32::from(red) / 255.0,
            green: f32::from(green) / 255.0,
            blue: f32::from(blue) / 255.0,
            alpha: 1.0,
        }
    }

    #[test]
    fn homepage_wordmark_is_an_81_by_19_bitmap() {
        assert_eq!(WORDMARK.len(), WORDMARK_HEIGHT);
        assert!(WORDMARK.iter().all(|row| row.len() == WORDMARK_WIDTH
            && row.bytes().all(|cell| matches!(cell, b'0' | b'1'))));
    }

    #[test]
    fn full_touchbar_uses_crisp_three_pixel_cells() {
        let geometry = wordmark_geometry(2008.0, 60.0);
        assert_eq!(geometry.cell, 3.0);
        assert_eq!(geometry.left, 882.0);
        assert_eq!(geometry.top, 1.0);

        let narrow = wordmark_geometry(80.0, 60.0);
        assert!(drift(80.0) <= narrow.left);
    }

    #[test]
    fn wordmark_run_count_stays_inside_canvas_budget() {
        let runs: usize = WORDMARK
            .iter()
            .map(|row| {
                row.as_bytes()
                    .windows(2)
                    .filter(|pair| pair[0] == b'0' && pair[1] == b'1')
                    .count()
                    + usize::from(row.starts_with('1'))
            })
            .sum();
        let lifted_runs: usize = WORDMARK[..7]
            .iter()
            .map(|row| {
                row.as_bytes()
                    .windows(2)
                    .filter(|pair| pair[0] == b'0' && pair[1] == b'1')
                    .count()
                    + usize::from(row.starts_with('1'))
            })
            .sum();
        assert_eq!(runs, 211);
        assert_eq!(lifted_runs, 79);
        assert!(runs + lifted_runs < 512);
    }

    #[test]
    fn light_theme_roles_use_black_with_contrast_safe_lit_colors() {
        let colors = oled_palette(&rgb(30, 102, 245), &rgb(76, 79, 105));

        assert_eq!(colors.background.red, 0.0);
        assert_eq!(colors.background.green, 0.0);
        assert_eq!(colors.background.blue, 0.0);
        assert_eq!(colors.background.alpha, 1.0);
        assert!(relative_luminance(&colors.accent) >= MIN_ACCENT_LUMINANCE);
        assert!(relative_luminance(&colors.foreground) >= MIN_FOREGROUND_LUMINANCE);
        assert!(colors.accent_lift > 0.0);
        assert!(colors.foreground_lift > 0.0);
    }

    #[test]
    fn already_safe_lit_colors_pass_through_over_black() {
        let accent = rgb(122, 162, 247);
        let foreground = rgb(202, 204, 204);
        let colors = oled_palette(&accent, &foreground);

        assert_eq!(colors.background.red, 0.0);
        assert_eq!(colors.background.green, 0.0);
        assert_eq!(colors.background.blue, 0.0);
        assert_eq!(colors.accent.red, accent.red);
        assert_eq!(colors.foreground.red, foreground.red);
        assert_eq!(colors.accent_lift, 0.0);
        assert_eq!(colors.foreground_lift, 0.0);
    }
}

touchbar_logic_sdk::export!(Omarchy);
