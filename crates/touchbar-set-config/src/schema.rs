use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{InputKind, LogicDescription, ScalarType, StateType};

pub const MAX_CONFIG_BYTES: usize = 256 * 1024;
pub const MAX_NODES: usize = 256;
pub const MAX_EXPANDED_NODES: usize = 512;
pub const MAX_TREE_DEPTH: usize = 32;
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_DIMENSION: f32 = 8192.0;
pub const MAX_STATE_FIELDS: usize = 256;
pub const MAX_ID_BYTES: usize = 128;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SetDocument {
    pub version: u32,
    pub id: String,
    pub label: String,
    #[serde(default = "default_width")]
    pub default_width: u32,
    #[serde(default)]
    pub logic_config: String,
    #[serde(default = "default_true")]
    pub show_in_default_profile: bool,
    pub selection: Option<Selection>,
    pub layout: LayoutNode,
    #[serde(default)]
    pub panels: Vec<PanelDefinition>,
    pub expanded_panel: Option<String>,
    pub press_and_hold_panel: Option<String>,
}

fn default_width() -> u32 {
    160
}
fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    #[serde(default)]
    pub applications: Vec<String>,
    #[serde(default)]
    pub activities: Vec<String>,
    #[serde(default = "default_true")]
    pub enabled_by_default: bool,
    #[serde(default)]
    pub priority: i32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PanelDefinition {
    #[serde(default, rename = "element")]
    pub elements: Vec<crate::PresentationBarElement>,
    pub principal_item: Option<String>,
    pub id: String,
    pub layout: Option<LayoutNode>,
    #[serde(default)]
    pub minimum_width: u32,
    #[serde(default = "default_width")]
    pub preferred_width: u32,
    #[serde(default = "maximum_width")]
    pub maximum_width: u32,
    #[serde(default = "default_true")]
    pub dismiss_on_selection: bool,
    #[serde(default)]
    pub items: Vec<String>,
}

fn maximum_width() -> u32 {
    MAX_DIMENSION as u32
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LayoutKind {
    Row,
    Column,
    Stack,
    Button,
    Text,
    Icon,
    Image,
    Slider,
    Meter,
    Progress,
    Scrubber,
    Repeat,
    Canvas,
    Responsive,
    ShaderEffect,
    FixedSpace,
    FlexibleSpace,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColorRole {
    Background,
    Control,
    ControlPressed,
    Track,
    Foreground,
    Muted,
    Accent,
    Destructive,
    OnAccent,
    OnDestructive,
}

/// The allowed property names are fixed; validation also checks which node
/// kinds can use children, repetition and event bindings.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayoutNode {
    pub kind: LayoutKind,
    pub id: Option<String>,
    pub widget_id: Option<u64>,
    #[serde(default)]
    pub children: Vec<LayoutNode>,
    #[serde(default)]
    pub spacing: f32,
    #[serde(default)]
    pub padding: f32,
    pub width: Option<f32>,
    pub height: Option<f32>,
    pub minimum: Option<f32>,
    pub maximum: Option<f32>,
    pub grow: Option<f32>,
    pub label: Option<String>,
    pub label_from: Option<String>,
    pub text: Option<String>,
    pub text_from: Option<String>,
    pub symbol: Option<String>,
    pub symbol_from: Option<String>,
    pub asset: Option<String>,
    pub asset_from: Option<String>,
    pub color: Option<ColorRole>,
    pub background: Option<ColorRole>,
    pub pressed_background: Option<ColorRole>,
    pub corner_radius: Option<f32>,
    pub font_size: Option<f32>,
    pub value: Option<f64>,
    pub value_from: Option<String>,
    pub enabled_from: Option<String>,
    pub visible_from: Option<String>,
    pub opacity: Option<f32>,
    pub on_pressed: Option<String>,
    pub on_activate: Option<String>,
    pub on_long_pressed: Option<String>,
    pub on_value_changed: Option<String>,
    pub on_released: Option<String>,
    pub on_cancelled: Option<String>,
    pub repeat_from: Option<String>,
    pub repeat_key: Option<String>,
    pub template: Option<Box<LayoutNode>>,
    pub canvas_from: Option<String>,
    #[serde(default)]
    pub variants: Vec<ResponsiveDefinition>,
    pub effect: Option<EffectDefinition>,
    pub motion: Option<MotionDefinition>,
    pub selected_from: Option<String>,
    pub pressed_from: Option<String>,
    pub hold_ms: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponsiveDefinition {
    pub representation: String,
    pub minimum_width: f32,
    pub layout: Box<LayoutNode>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ScalarBinding {
    Number(f64),
    State(String),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EffectDefinition {
    pub id: u64,
    pub source: String,
    #[serde(default)]
    pub parameters: Vec<ScalarBinding>,
    pub period_ms: Option<u32>,
    pub opacity: Option<f32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransformDefinition {
    pub x: Option<ScalarBinding>,
    pub y: Option<ScalarBinding>,
    pub scale: Option<ScalarBinding>,
    pub opacity: Option<ScalarBinding>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MotionDefinition {
    pub id: u64,
    pub duration_ms: u32,
    pub easing: String,
    pub playback: String,
    pub from: TransformDefinition,
    pub to: TransformDefinition,
}

impl SetDocument {
    pub fn from_toml(source: &str) -> Result<Self> {
        if source.len() > MAX_CONFIG_BYTES {
            bail!("set file exceeds 256 KiB");
        }
        let set: Self = toml::from_str(source)?;
        set.validate()?;
        Ok(set)
    }

    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            bail!("unsupported set version {}", self.version);
        }
        validate_name(&self.id)?;
        if self.label.is_empty() {
            bail!("set label cannot be empty");
        }
        if self.default_width == 0 || self.default_width > MAX_DIMENSION as u32 {
            bail!("set default_width must be between 1 and 8192");
        }
        if !self.logic_config.is_empty() {
            validate_name(&self.logic_config)?;
        }
        if self.panels.len() > 64 {
            bail!("too many panels");
        }
        if let Some(selection) = &self.selection {
            if selection.applications.is_empty() && selection.activities.is_empty() {
                bail!("selection must declare application or activity context");
            }
            for values in [&selection.applications, &selection.activities] {
                if values.len() > 128 {
                    bail!("too many selection identities");
                }
                let mut unique = BTreeSet::new();
                for value in values {
                    validate_name(value)?;
                    if !unique.insert(value) {
                        bail!("duplicate selection identity {value}");
                    }
                }
            }
        }
        let mut budget = LayoutBudget::default();
        budget.text += self.label.len() + self.logic_config.len();
        validate_node(&self.layout, 1, &mut budget).context("primary layout")?;
        let mut panels = BTreeSet::new();
        for panel in &self.panels {
            validate_name(&panel.id)?;
            if !panels.insert(&panel.id) {
                bail!("duplicate panel {}", panel.id);
            }
            if panel.minimum_width > panel.preferred_width
                || panel.preferred_width > panel.maximum_width
                || panel.preferred_width == 0
                || panel.maximum_width > MAX_DIMENSION as u32
            {
                bail!("panel {} widths are out of order or bounds", panel.id);
            }
            if !panel.items.is_empty() && !panel.elements.is_empty() {
                bail!("panel items and elements are mutually exclusive");
            }
            if let Some(item) = &panel.principal_item {
                validate_name(item)?;
            }
            validate_panel_elements(&panel.elements, 1, &mut budget.nodes)?;
            if panel.items.len() > 64 {
                bail!("panel {} has too many item refs", panel.id);
            }
            for item in &panel.items {
                validate_name(item)?;
            }
            if let Some(layout) = &panel.layout {
                validate_node(layout, 1, &mut budget)
                    .with_context(|| format!("panel {}", panel.id))?;
            }
        }
        for reference in [&self.expanded_panel, &self.press_and_hold_panel]
            .into_iter()
            .flatten()
        {
            if !panels.contains(reference) {
                bail!("unknown panel reference {reference}");
            }
        }
        if budget.text > MAX_TEXT_BYTES {
            bail!("set exceeds its text budget");
        }
        Ok(())
    }

    pub fn validate_bindings(&self, description: &LogicDescription) -> Result<()> {
        self.validate()?;
        description.validate()?;
        validate_node_bindings(&self.layout, description, None)?;
        for panel in &self.panels {
            if let Some(layout) = &panel.layout {
                validate_node_bindings(layout, description, None)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || name.len() > MAX_ID_BYTES
        || !name.as_bytes()[0].is_ascii_alphanumeric()
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        || name.contains("..")
    {
        bail!("invalid identifier {name:?}");
    }
    Ok(())
}

#[derive(Default)]
struct LayoutBudget {
    nodes: usize,
    text: usize,
    ids: BTreeSet<String>,
    widgets: BTreeSet<u64>,
}

impl LayoutNode {
    pub fn actions(&self) -> impl Iterator<Item = (&str, InputKind)> {
        [
            (&self.on_pressed, InputKind::Pressed),
            (&self.on_activate, InputKind::Activated),
            (&self.on_long_pressed, InputKind::LongPressed),
            (&self.on_value_changed, InputKind::ValueChanged),
            (&self.on_released, InputKind::Released),
            (&self.on_cancelled, InputKind::Cancelled),
        ]
        .into_iter()
        .filter_map(|(action, kind)| action.as_deref().map(|action| (action, kind)))
    }
}

fn validate_node(node: &LayoutNode, depth: usize, budget: &mut LayoutBudget) -> Result<()> {
    budget.nodes += 1;
    if budget.nodes > MAX_NODES || depth > MAX_TREE_DEPTH {
        bail!("layout exceeds node/depth budget");
    }
    if let Some(id) = &node.id {
        validate_name(id)?;
        if !budget.ids.insert(id.clone()) {
            bail!("duplicate widget id {id}");
        }
    }
    if let Some(id) = node.widget_id
        && (id == 0 || !budget.widgets.insert(id))
    {
        bail!("invalid or duplicate numeric widget id {id}");
    }
    for value in [
        Some(node.spacing),
        Some(node.padding),
        node.width,
        node.height,
        node.minimum,
        node.maximum,
        node.grow,
        node.corner_radius,
        node.font_size,
    ]
    .into_iter()
    .flatten()
    {
        if !value.is_finite() || !(0.0..=MAX_DIMENSION).contains(&value) {
            bail!("invalid layout dimension");
        }
    }
    if let (Some(min), Some(max)) = (node.minimum, node.maximum)
        && min > max
    {
        bail!("layout minimum exceeds maximum");
    }
    if node
        .opacity
        .is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
        || node.value.is_some_and(|v| !v.is_finite())
    {
        bail!("layout contains an invalid opacity or numeric value");
    }
    if let Some(symbol) = &node.symbol
        && !matches!(
            symbol.as_str(),
            "volume"
                | "muted"
                | "play"
                | "pause"
                | "previous"
                | "next"
                | "microphone"
                | "brightness"
                | "battery"
                | "wifi"
                | "bluetooth"
                | "workspace"
                | "capture"
                | "timer"
                | "terminal"
                | "graph"
                | "check"
                | "chevron.left"
                | "chevron.right"
        )
    {
        bail!("unknown built-in symbol {symbol}");
    }
    if let Some(asset) = &node.asset {
        validate_name(asset)?;
    }
    for text in [&node.label, &node.text].into_iter().flatten() {
        budget.text += text.len();
    }
    if budget.text > MAX_TEXT_BYTES {
        bail!("layout exceeds text budget");
    }
    for (literal, binding) in [
        (&node.label, &node.label_from),
        (&node.text, &node.text_from),
        (&node.symbol, &node.symbol_from),
        (&node.asset, &node.asset_from),
    ] {
        if literal.is_some() && binding.is_some() {
            bail!("layout property has both a literal and a state binding");
        }
    }
    if node.value.is_some() && node.value_from.is_some() {
        bail!("value has both a literal and a state binding");
    }
    let interactive = matches!(
        node.kind,
        LayoutKind::Button | LayoutKind::Slider | LayoutKind::Scrubber
    );
    if node.actions().next().is_some()
        && (!interactive || (node.id.is_none() && node.widget_id.is_none()))
    {
        bail!("input actions require an interactive node with an id");
    }
    if node.on_value_changed.is_some()
        && !matches!(node.kind, LayoutKind::Slider | LayoutKind::Scrubber)
    {
        bail!("ValueChanged requires a slider or scrubber");
    }
    for (action, _) in node.actions() {
        validate_name(action)?;
    }
    let container = matches!(
        node.kind,
        LayoutKind::Row | LayoutKind::Column | LayoutKind::Stack | LayoutKind::Button
    );
    if (!container && !node.children.is_empty())
        || (node.kind == LayoutKind::Button && node.children.len() > 1)
    {
        bail!("node kind does not support these children");
    }
    if node.kind == LayoutKind::Repeat {
        if node.repeat_from.is_none() || node.repeat_key.is_none() || node.template.is_none() {
            bail!("repeat requires source, key and template");
        }
        validate_name(node.repeat_from.as_deref().unwrap())?;
        validate_name(node.repeat_key.as_deref().unwrap())?;
    } else if node.repeat_from.is_some() || node.repeat_key.is_some() || node.template.is_some() {
        bail!("repeat properties require a repeat node");
    }
    if node.canvas_from.is_some() && node.kind != LayoutKind::Canvas {
        bail!("canvas binding requires a canvas node");
    }
    if node.hold_ms.is_some_and(|ms| !(1..=60_000).contains(&ms)) {
        bail!("hold duration outside bounds");
    }
    if node.kind == LayoutKind::Responsive {
        if node.variants.is_empty() || node.variants.len() > 4 {
            bail!("responsive requires 1..4 variants");
        }
        let mut representations = BTreeSet::new();
        for variant in &node.variants {
            if !matches!(
                variant.representation.as_str(),
                "hidden" | "minimal" | "compact" | "full"
            ) || !representations.insert(&variant.representation)
                || !variant.minimum_width.is_finite()
                || !(0.0..=MAX_DIMENSION).contains(&variant.minimum_width)
            {
                bail!("invalid responsive variant");
            }
            validate_node(&variant.layout, depth + 1, budget)?;
        }
    } else if !node.variants.is_empty() {
        bail!("variants require responsive node");
    }
    if node.kind == LayoutKind::ShaderEffect {
        let effect = node
            .effect
            .as_ref()
            .context("shader-effect requires effect")?;
        if effect.id == 0
            || effect.source.len() > 4096
            || effect.parameters.len() > 8
            || effect
                .period_ms
                .is_some_and(|ms| !(100..=60_000).contains(&ms))
            || effect
                .opacity
                .is_some_and(|v| !v.is_finite() || !(0.0..=1.0).contains(&v))
        {
            bail!("invalid shader effect definition");
        }
        budget.text += effect.source.len();
    } else if node.effect.is_some() {
        bail!("effect requires shader-effect node");
    }
    if let Some(motion) = &node.motion
        && (motion.id == 0
            || motion.duration_ms == 0
            || motion.duration_ms > 60_000
            || !matches!(motion.easing.as_str(), "linear" | "ease-in-out")
            || !matches!(motion.playback.as_str(), "once" | "loop" | "alternate"))
    {
        bail!("invalid motion definition");
    }
    for child in &node.children {
        validate_node(child, depth + 1, budget)?;
    }
    if let Some(template) = &node.template {
        validate_node(template, depth + 1, budget)?;
    }
    Ok(())
}

fn binding_type(
    path: &str,
    description: &LogicDescription,
    record: Option<&BTreeMap<String, ScalarType>>,
) -> Result<ScalarType> {
    if let Some(field) = path.strip_prefix("record.") {
        validate_name(field)?;
        return record
            .and_then(|record| record.get(field))
            .copied()
            .ok_or_else(|| anyhow::anyhow!("unknown record binding {path}"));
    }
    validate_name(path)?;
    match description.state.get(path) {
        Some(StateType::Bool) => Ok(ScalarType::Bool),
        Some(StateType::Number) => Ok(ScalarType::Number),
        Some(StateType::Text) => Ok(ScalarType::Text),
        _ => bail!("unknown or non-scalar binding {path}"),
    }
}

fn validate_node_bindings(
    node: &LayoutNode,
    description: &LogicDescription,
    record: Option<&BTreeMap<String, ScalarType>>,
) -> Result<()> {
    for (binding, expected) in [
        (&node.enabled_from, ScalarType::Bool),
        (&node.visible_from, ScalarType::Bool),
        (&node.selected_from, ScalarType::Bool),
        (&node.pressed_from, ScalarType::Bool),
        (&node.label_from, ScalarType::Text),
        (&node.text_from, ScalarType::Text),
        (&node.symbol_from, ScalarType::Text),
        (&node.asset_from, ScalarType::Text),
        (&node.value_from, ScalarType::Number),
    ] {
        if let Some(path) = binding
            && binding_type(path, description, record)? != expected
        {
            bail!("binding {path} has wrong type");
        }
    }
    for (action, kind) in node.actions() {
        let action_description = description
            .actions
            .get(action)
            .ok_or_else(|| anyhow::anyhow!("unknown action binding {action}"))?;
        if !action_description.events.contains(&kind) {
            bail!("action {action} does not accept {kind:?}");
        }
        if kind == InputKind::ValueChanged
            && action_description.value_type != Some(ScalarType::Number)
        {
            bail!("action {action} must accept numeric values for ValueChanged");
        }
        if matches!(
            action_description.value_type,
            Some(ScalarType::Bool | ScalarType::Text)
        ) {
            bail!("action {action} requires a value unavailable from physical input");
        }
    }
    if let Some(source) = &node.canvas_from
        && !matches!(description.state.get(source), Some(StateType::List { .. }))
    {
        bail!("canvas source {source} must be a declared record list");
    }
    for child in &node.children {
        validate_node_bindings(child, description, record)?;
    }
    for variant in &node.variants {
        validate_node_bindings(&variant.layout, description, record)?;
    }
    let mut scalars = Vec::new();
    if let Some(effect) = &node.effect {
        scalars.extend(effect.parameters.iter());
    }
    if let Some(motion) = &node.motion {
        for transform in [&motion.from, &motion.to] {
            scalars.extend(
                [
                    &transform.x,
                    &transform.y,
                    &transform.scale,
                    &transform.opacity,
                ]
                .into_iter()
                .flatten(),
            );
        }
    }
    for scalar in scalars {
        match scalar {
            ScalarBinding::Number(value) if !value.is_finite() => bail!("non-finite scalar"),
            ScalarBinding::State(path)
                if binding_type(path, description, record)? != ScalarType::Number =>
            {
                bail!("numeric binding expected")
            }
            _ => {}
        }
    }
    if let Some(template) = &node.template {
        let source = node.repeat_from.as_deref().unwrap();
        let Some(StateType::List { key, fields }) = description.state.get(source) else {
            bail!("repeat source {source} is not a declared list");
        };
        if Some(key.as_str()) != node.repeat_key.as_deref() {
            bail!("repeat key does not match list {source} key");
        }
        validate_node_bindings(template, description, Some(fields))?;
    }
    Ok(())
}

fn validate_panel_elements(
    elements: &[crate::PresentationBarElement],
    depth: usize,
    count: &mut usize,
) -> Result<()> {
    use crate::PresentationBarElement as E;
    if depth > MAX_TREE_DEPTH {
        bail!("panel composition too deep");
    }
    *count += elements.len();
    if *count > MAX_NODES {
        bail!("panel composition too large");
    }
    for element in elements {
        match element {
            E::Item {
                item,
                minimum_width,
                preferred_width,
                maximum_width,
            } => validate_panel_item(item, *minimum_width, *preferred_width, *maximum_width)?,
            E::FixedSpace { width } if *width > 8192 => bail!("panel space too large"),
            E::FlexibleSpace { minimum, weight } if *minimum > 8192 || *weight == 0 => {
                bail!("invalid panel space")
            }
            E::Group {
                id,
                spacing,
                elements,
                ..
            } => {
                validate_name(id)?;
                if *spacing > 8192 {
                    bail!("panel spacing too large");
                }
                validate_panel_group(elements, depth + 1, count)?;
            }
            _ => {}
        }
    }
    Ok(())
}
fn validate_panel_group(
    elements: &[crate::PresentationGroupElement],
    depth: usize,
    count: &mut usize,
) -> Result<()> {
    use crate::PresentationGroupElement as E;
    if depth > MAX_TREE_DEPTH {
        bail!("panel composition too deep");
    }
    *count += elements.len();
    if *count > MAX_NODES {
        bail!("panel composition too large");
    }
    for element in elements {
        match element {
            E::Item {
                item,
                minimum_width,
                preferred_width,
                maximum_width,
            } => validate_panel_item(item, *minimum_width, *preferred_width, *maximum_width)?,
            E::Group {
                id,
                spacing,
                elements,
                ..
            } => {
                validate_name(id)?;
                if *spacing > 8192 {
                    bail!("panel spacing too large");
                }
                validate_panel_group(elements, depth + 1, count)?;
            }
        }
    }
    Ok(())
}
fn validate_panel_item(item: &str, min: u32, preferred: u32, max: u32) -> Result<()> {
    validate_name(item)?;
    if min > preferred || preferred > max || preferred == 0 || max > 8192 {
        bail!("invalid panel item widths");
    }
    Ok(())
}
