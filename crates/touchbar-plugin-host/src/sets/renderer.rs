use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use touchbar_set_config::{
    ActionEvent, ColorRole as ConfigRole, LayoutKind, LayoutNode, LogicDescription, ScalarValue,
    SetDocument, StateRecord, StateSnapshot, StateValue,
};
use touchbar_ui::{
    ColorRole, ContinuousValue, Flex, FlexItem, Icon, ImageFit, ImageTint, MeterStyle, Node,
    PressableStyle, Rect, RetainedUi, Size, Symbol, SymbolCatalog, WidgetId,
};

use super::bindings::{BoundWidget, Routing, stable_id};
use crate::{
    AnimationRegistry, AnimationSignature, Appearance, EffectRegistry, HostLimits, InputEvent,
    PackageAssets,
};
use touchbar_set_config::{ScalarBinding, TransformDefinition};
use touchbar_ui::{
    CanvasCommand, CanvasPaint, Easing, Motion, MotionId, MotionPlayback, Point, Representation,
    ResponsiveVariant, ShaderEffect, TextAlign, VisualTransform,
};

pub struct CompiledSet {
    pub(super) document: SetDocument,
    pub(super) description: LogicDescription,
    limits: HostLimits,
    routing: RefCell<Routing>,
    symbols: RefCell<SymbolCatalog>,
    animations: RefCell<AnimationRegistry>,
    effects: RefCell<EffectRegistry>,
}

impl CompiledSet {
    pub fn new(
        set: SetDocument,
        description: &LogicDescription,
        limits: HostLimits,
    ) -> Result<Self> {
        set.validate_bindings(description)?;
        fn count(node: &LayoutNode) -> usize {
            1 + node.children.iter().map(count).sum::<usize>()
                + node.template.as_deref().map_or(0, count)
                + node
                    .variants
                    .iter()
                    .map(|variant| count(&variant.layout))
                    .sum::<usize>()
        }
        if count(&set.layout)
            + set
                .panels
                .iter()
                .map(|panel| count(&panel.layout))
                .sum::<usize>()
            > limits.max_nodes
        {
            bail!("set exceeds host node limit");
        }
        fn check_effects(node: &LayoutNode, sources: &mut BTreeSet<String>) -> Result<()> {
            if let Some(effect) = &node.effect {
                touchbar_ui::EffectProgram::compile(&effect.source)?;
                sources.insert(effect.source.clone());
            }
            for child in &node.children {
                check_effects(child, sources)?;
            }
            for variant in &node.variants {
                check_effects(&variant.layout, sources)?;
            }
            if let Some(template) = &node.template {
                check_effects(template, sources)?;
            }
            Ok(())
        }
        let mut sources = BTreeSet::new();
        check_effects(&set.layout, &mut sources)?;
        for panel in &set.panels {
            check_effects(&panel.layout, &mut sources)?;
        }
        if sources.len() > limits.max_effect_programs {
            bail!("effect program budget exceeded");
        }
        Ok(Self {
            document: set,
            description: description.clone(),
            limits,
            routing: RefCell::new(Routing::default()),
            symbols: RefCell::new(SymbolCatalog::default()),
            animations: RefCell::new(AnimationRegistry::default()),
            effects: RefCell::new(EffectRegistry::default()),
        })
    }
    pub fn render(
        &self,
        state: &StateSnapshot,
        viewport: Rect,
        _appearance: Appearance,
        now: Duration,
        assets: &PackageAssets,
    ) -> Result<RetainedUi> {
        self.render_layout(&self.document.layout, state, viewport, now, assets)
    }
    pub fn render_panel(
        &self,
        panel: &str,
        state: &StateSnapshot,
        viewport: Rect,
        _appearance: Appearance,
        now: Duration,
        assets: &PackageAssets,
    ) -> Result<RetainedUi> {
        let panel = self
            .document
            .panels
            .iter()
            .find(|definition| definition.id == panel)
            .context("unknown panel")?;
        self.render_layout(&panel.layout, state, viewport, now, assets)
    }
    fn render_layout(
        &self,
        layout: &LayoutNode,
        state: &StateSnapshot,
        viewport: Rect,
        now: Duration,
        assets: &PackageAssets,
    ) -> Result<RetainedUi> {
        crate::validate_viewport(viewport)?;
        state.validate(&self.description)?;
        let mut builder = Builder {
            set: self,
            state,
            assets,
            widgets: BTreeMap::new(),
            nodes: 0,
            text: 0,
            viewport,
            now,
            animations: self.animations.borrow().clone(),
            effects: self.effects.borrow().clone(),
            motion_ids: BTreeSet::new(),
            effect_ids: BTreeSet::new(),
            canvas_commands: 0,
        };
        let root = builder.node(layout, "root", None, 1)?;
        builder
            .animations
            .retain_item(&self.document.id, &builder.motion_ids);
        builder
            .effects
            .retain_item(&self.document.id, &builder.effect_ids);
        self.routing.borrow_mut().replace(builder.widgets)?;
        *self.animations.borrow_mut() = builder.animations;
        *self.effects.borrow_mut() = builder.effects;
        Ok(RetainedUi::new(root))
    }
    pub fn action_for(&self, event: &InputEvent) -> Result<Option<ActionEvent>> {
        if event.item_id != self.document.id {
            bail!("input belongs to a different set");
        }
        self.routing.borrow_mut().action_for(event)
    }
    pub fn drain_cancelled(&self) -> Vec<ActionEvent> {
        self.routing.borrow_mut().drain_cancelled()
    }
    pub fn cancel_contacts(&self) -> Result<()> {
        self.routing.borrow_mut().replace(BTreeMap::new())
    }
}

#[derive(Clone, Copy)]
struct Record<'a> {
    record: &'a StateRecord,
    key: &'a str,
}

struct Builder<'a> {
    set: &'a CompiledSet,
    state: &'a StateSnapshot,
    assets: &'a PackageAssets,
    widgets: BTreeMap<u64, BoundWidget>,
    nodes: usize,
    text: usize,
    viewport: Rect,
    now: Duration,
    animations: AnimationRegistry,
    effects: EffectRegistry,
    motion_ids: BTreeSet<u64>,
    effect_ids: BTreeSet<u64>,
    canvas_commands: usize,
}

impl Builder<'_> {
    fn spend(&mut self, count: usize) -> Result<()> {
        self.nodes += count;
        if self.nodes > self.set.limits.max_expanded_nodes {
            bail!("expanded UI node budget exceeded");
        }
        Ok(())
    }

    fn text(&mut self, text: String) -> Result<String> {
        self.text += text.len();
        if self.text > self.set.limits.max_text_bytes {
            bail!("rendered text budget exceeded");
        }
        Ok(text)
    }

    fn value(&self, path: &str, record: Option<Record<'_>>) -> Result<ScalarValue> {
        if let Some(field) = path.strip_prefix("record.") {
            return record
                .and_then(|record| record.record.0.get(field))
                .cloned()
                .with_context(|| format!("missing record value {path}"));
        }
        match self.state.0.get(path) {
            Some(StateValue::Bool(value)) => Ok(ScalarValue::Bool(*value)),
            Some(StateValue::Number(value)) => Ok(ScalarValue::Number(*value)),
            Some(StateValue::Text(value)) => Ok(ScalarValue::Text(value.clone())),
            _ => bail!("missing scalar state {path}"),
        }
    }

    fn boolean(
        &self,
        path: &Option<String>,
        default: bool,
        record: Option<Record<'_>>,
    ) -> Result<bool> {
        match path {
            None => Ok(default),
            Some(path) => match self.value(path, record)? {
                ScalarValue::Bool(value) => Ok(value),
                _ => bail!("binding {path} is not boolean"),
            },
        }
    }

    fn string(
        &self,
        literal: &Option<String>,
        path: &Option<String>,
        record: Option<Record<'_>>,
    ) -> Result<Option<String>> {
        match path {
            None => Ok(literal.clone()),
            Some(path) => match self.value(path, record)? {
                ScalarValue::Text(value) => Ok(Some(value)),
                _ => bail!("binding {path} is not text"),
            },
        }
    }

    fn number(
        &self,
        literal: Option<f64>,
        path: &Option<String>,
        record: Option<Record<'_>>,
    ) -> Result<f32> {
        let value = match path {
            None => literal.unwrap_or(0.0),
            Some(path) => match self.value(path, record)? {
                ScalarValue::Number(value) => value,
                _ => bail!("binding {path} is not numeric"),
            },
        };
        let number = value as f32;
        if !number.is_finite() {
            bail!("rendered numeric value is not finite");
        }
        Ok(number)
    }

    fn node(
        &mut self,
        node: &LayoutNode,
        path: &str,
        record: Option<Record<'_>>,
        depth: usize,
    ) -> Result<Node> {
        self.spend(1)?;
        if depth > self.set.limits.max_tree_depth {
            bail!("rendered tree is too deep");
        }
        if !self.boolean(&node.visible_from, true, record)? {
            return Ok(Node::Empty);
        }
        let label = self
            .string(&node.label, &node.label_from, record)?
            .unwrap_or_default();
        let label = self.text(label)?;
        let color = node.color.map(color_role).unwrap_or(ColorRole::Foreground);
        let id = if record.is_none() {
            node.widget_id
        } else {
            None
        }
        .unwrap_or_else(|| {
            stable_id(
                &self.set.document.id,
                node.id.as_deref().unwrap_or(path),
                record.map_or("", |record| record.key),
            )
        });
        let enabled = self.boolean(&node.enabled_from, true, record)?;
        if enabled
            && matches!(
                node.kind,
                LayoutKind::Button | LayoutKind::Slider | LayoutKind::Scrubber
            )
        {
            let widget = BoundWidget {
                actions: node
                    .actions()
                    .map(|(action, kind)| (kind, action.to_owned()))
                    .collect(),
                record_key: record.map(|record| record.key.to_owned()),
            };
            if self.widgets.insert(id, widget).is_some() {
                bail!("rendered widget identity collision");
            }
        }
        let rendered = match node.kind {
            LayoutKind::Row | LayoutKind::Column | LayoutKind::Stack => {
                let mut children = vec![];
                for (index, child) in node.children.iter().enumerate() {
                    let rendered =
                        self.node(child, &format!("{path}/{index}"), record, depth + 1)?;
                    children.push(FlexItem::new(flex(child), rendered));
                }
                match node.kind {
                    LayoutKind::Row => Node::row(node.spacing, node.padding, children),
                    LayoutKind::Column => Node::column(node.spacing, node.padding, children),
                    _ => Node::Layer(children.into_iter().map(|child| child.node).collect()),
                }
            }
            LayoutKind::Repeat => {
                let source = node
                    .repeat_from
                    .as_deref()
                    .context("repeat source absent")?;
                let Some(StateValue::List(records)) = self.state.0.get(source) else {
                    bail!("missing record list {source}");
                };
                let template = node.template.as_deref().context("repeat template absent")?;
                let key = node.repeat_key.as_deref().context("repeat key absent")?;
                let mut children = vec![];
                for StateRecord(fields) in records {
                    let Some(ScalarValue::Text(key)) = fields.get(key) else {
                        bail!("repeat record lacks key");
                    };
                    let current = StateRecord(fields.clone());
                    let rendered = self.node(
                        template,
                        &format!("{path}/template"),
                        Some(Record {
                            record: &current,
                            key,
                        }),
                        depth + 1,
                    )?;
                    children.push(FlexItem::new(flex(template), rendered));
                }
                Node::row(node.spacing, node.padding, children)
            }
            LayoutKind::Button => {
                self.spend(1)?;
                let child = if let Some(child) = node.children.first() {
                    self.node(child, &format!("{path}/content"), record, depth + 1)?
                } else {
                    self.content(node, &label, color, record)?
                };
                if enabled {
                    Node::Pressable {
                        id: WidgetId(id),
                        label,
                        pressed: self.boolean(&node.pressed_from, false, record)?,
                        hold: node.hold_ms.map(|ms| Duration::from_millis(ms.into())),
                        selected: node
                            .selected_from
                            .as_ref()
                            .map(|_| self.boolean(&node.selected_from, false, record))
                            .transpose()?,
                        style: PressableStyle {
                            background: Some(
                                node.background
                                    .map(color_role)
                                    .unwrap_or(ColorRole::Control),
                            ),
                            pressed_background: Some(
                                node.pressed_background
                                    .map(color_role)
                                    .unwrap_or(ColorRole::ControlPressed),
                            ),
                            corner_radius: node.corner_radius,
                            padding: node.padding,
                            ..PressableStyle::control()
                        },
                        child: Box::new(child),
                    }
                } else {
                    Node::opacity(0.35, child)
                }
            }
            LayoutKind::Text | LayoutKind::Icon | LayoutKind::Image => {
                self.content(node, &label, color, record)?
            }
            LayoutKind::Slider | LayoutKind::Scrubber => {
                let value = self
                    .number(node.value, &node.value_from, record)?
                    .clamp(0.0, 1.0);
                if enabled {
                    Node::slider(WidgetId(id), label, value)
                } else {
                    Node::meter(
                        label,
                        ContinuousValue::unit(value),
                        None,
                        MeterStyle::default(),
                    )
                }
            }
            LayoutKind::Meter => Node::meter(
                label,
                ContinuousValue::unit(self.number(node.value, &node.value_from, record)?),
                None,
                MeterStyle::default(),
            ),
            LayoutKind::Progress => {
                Node::progress(label, self.number(node.value, &node.value_from, record)?)
            }
            LayoutKind::FixedSpace | LayoutKind::FlexibleSpace => Node::Empty,
            LayoutKind::Canvas => self.canvas(node, &label, color)?,
            LayoutKind::Responsive => {
                let mut variants = Vec::new();
                for (index, variant) in node.variants.iter().enumerate() {
                    let representation = match variant.representation.as_str() {
                        "hidden" => Representation::Hidden,
                        "minimal" => Representation::Minimal,
                        "compact" => Representation::Compact,
                        "full" => Representation::Full,
                        _ => bail!("unknown representation"),
                    };
                    variants.push(ResponsiveVariant::new(
                        representation,
                        variant.minimum_width,
                        self.node(
                            &variant.layout,
                            &format!("{path}/variant/{index}"),
                            record,
                            depth + 1,
                        )?,
                    ));
                }
                Node::responsive(WidgetId(id), variants)
            }
            LayoutKind::ShaderEffect => {
                let effect = node.effect.as_ref().context("effect absent")?;
                if !self.effect_ids.insert(effect.id)
                    || self.effect_ids.len() > self.set.limits.max_effect_nodes
                {
                    bail!("effect ID collision or budget exceeded");
                }
                let mut parameters = [0.0; 8];
                for (target, value) in parameters.iter_mut().zip(&effect.parameters) {
                    *target = self.scalar(Some(value), 0.0, -1_000_000.0, 1_000_000.0, record)?;
                }
                let period = effect.period_ms.map(|ms| Duration::from_millis(ms.into()));
                let (program, started) = self.effects.bind(
                    &self.set.document.id,
                    effect.id,
                    &effect.source,
                    period,
                    self.now,
                    self.set.limits.max_effect_programs,
                )?;
                Node::shader_effect(
                    label,
                    Size::new(
                        node.width.unwrap_or(self.viewport.width),
                        node.height.unwrap_or(self.viewport.height),
                    ),
                    ShaderEffect {
                        program,
                        parameters,
                        opacity: effect.opacity.unwrap_or(1.0),
                        started,
                        period,
                    },
                )
            }
        };
        let rendered = if let Some(motion) = &node.motion {
            self.spend(1)?;
            if !self.motion_ids.insert(motion.id)
                || self.motion_ids.len() > self.set.limits.max_animations
            {
                bail!("motion ID collision or budget exceeded");
            }
            let signature = AnimationSignature {
                from: self.transform(&motion.from, record)?,
                to: self.transform(&motion.to, record)?,
                duration: Duration::from_millis(motion.duration_ms.into()),
                easing: if motion.easing == "linear" {
                    Easing::Linear
                } else {
                    Easing::EaseInOut
                },
                playback: match motion.playback.as_str() {
                    "loop" => MotionPlayback::Loop,
                    "alternate" => MotionPlayback::Alternate,
                    _ => MotionPlayback::Once,
                },
            };
            let started =
                self.animations
                    .bind(&self.set.document.id, motion.id, signature, self.now);
            Node::motion(
                Motion {
                    id: MotionId(motion.id),
                    from: signature.from,
                    to: signature.to,
                    duration: signature.duration,
                    easing: signature.easing,
                    playback: signature.playback,
                    started,
                },
                rendered,
            )
        } else {
            rendered
        };
        Ok(if let Some(opacity) = node.opacity {
            self.spend(1)?;
            Node::opacity(opacity, rendered)
        } else {
            rendered
        })
    }

    fn scalar(
        &self,
        scalar: Option<&ScalarBinding>,
        default: f32,
        min: f32,
        max: f32,
        record: Option<Record<'_>>,
    ) -> Result<f32> {
        let value = match scalar {
            None => default,
            Some(ScalarBinding::Number(value)) => *value as f32,
            Some(ScalarBinding::State(path)) => self.number(None, &Some(path.clone()), record)?,
        };
        if !value.is_finite() || !(min..=max).contains(&value) {
            bail!("numeric UI property outside bounds");
        }
        Ok(value)
    }
    fn transform(
        &self,
        value: &TransformDefinition,
        record: Option<Record<'_>>,
    ) -> Result<VisualTransform> {
        Ok(VisualTransform {
            translation: Point::new(
                self.scalar(value.x.as_ref(), 0.0, -8192.0, 8192.0, record)?,
                self.scalar(value.y.as_ref(), 0.0, -8192.0, 8192.0, record)?,
            ),
            scale: self.scalar(value.scale.as_ref(), 1.0, 0.01, 16.0, record)?,
            opacity: self.scalar(value.opacity.as_ref(), 1.0, 0.0, 1.0, record)?,
        })
    }
    fn canvas(&mut self, node: &LayoutNode, label: &str, color: ColorRole) -> Result<Node> {
        let source = node
            .canvas_from
            .as_deref()
            .context("canvas source absent")?;
        let Some(StateValue::List(records)) = self.state.0.get(source) else {
            bail!("missing canvas records");
        };
        self.canvas_commands += records.len();
        if self.canvas_commands > self.set.limits.max_canvas_commands {
            bail!("canvas command budget exceeded");
        }
        let mut commands = Vec::new();
        for record in records {
            let text = |field: &str| -> Result<&str> {
                match record.0.get(field) {
                    Some(ScalarValue::Text(text)) => Ok(text),
                    _ => bail!("missing canvas text {field}"),
                }
            };
            let number = |field: &str, default: f32, min: f32, max: f32| -> Result<f32> {
                let number = match record.0.get(field) {
                    None => default,
                    Some(ScalarValue::Number(value)) => *value as f32,
                    _ => bail!("canvas field {field} must be numeric"),
                };
                if !number.is_finite() || !(min..=max).contains(&number) {
                    bail!("canvas field {field} outside bounds");
                }
                Ok(number)
            };
            let role = if let Some(ScalarValue::Text(role)) = record.0.get("color") {
                color_role(serde_json::from_value(serde_json::Value::String(
                    role.clone(),
                ))?)
            } else {
                color
            };
            let paint = CanvasPaint::role(role).with_opacity(number("opacity", 1.0, 0.0, 1.0)?);
            let x = number("x", 0.0, -8192.0, 8192.0)?;
            let y = number("y", 0.0, -8192.0, 8192.0)?;
            let rect = Rect::new(
                x,
                y,
                number("width", 0.0, 0.0, 8192.0)?,
                number("height", 0.0, 0.0, 8192.0)?,
            );
            commands.push(match text("op")? {
                "rect" => CanvasCommand::FillRect {
                    rect,
                    radius: number("radius", 0.0, 0.0, 4096.0)?,
                    paint,
                },
                "line" => CanvasCommand::Line {
                    start: Point::new(x, y),
                    end: Point::new(
                        number("x2", 0.0, -8192.0, 8192.0)?,
                        number("y2", 0.0, -8192.0, 8192.0)?,
                    ),
                    width: number("stroke", 1.0, 0.1, 512.0)?,
                    paint,
                },
                "circle" => CanvasCommand::FillCircle {
                    center: Point::new(x, y),
                    radius: number("radius", 0.0, 0.0, 4096.0)?,
                    paint,
                },
                "text" => CanvasCommand::Text {
                    rect,
                    text: self.text(text("text")?.to_owned())?,
                    size: number("size", 16.0, 1.0, 64.0)?,
                    paint,
                    align: TextAlign::Center,
                },
                _ => bail!("unsupported canvas command"),
            });
        }
        Ok(Node::canvas(
            label.to_owned(),
            Size::new(
                node.width.unwrap_or(self.viewport.width),
                node.height.unwrap_or(self.viewport.height),
            ),
            commands,
        ))
    }

    fn content(
        &mut self,
        node: &LayoutNode,
        label: &str,
        color: ColorRole,
        record: Option<Record<'_>>,
    ) -> Result<Node> {
        if let Some(symbol) = self.string(&node.symbol, &node.symbol_from, record)? {
            let icon = match symbol.as_str() {
                "volume" => Some(Icon::Volume),
                "muted" => Some(Icon::Muted),
                "play" => Some(Icon::Play),
                "pause" => Some(Icon::Pause),
                "check" => Some(Icon::Check),
                "chevron.left" => Some(Icon::ChevronLeft),
                "chevron.right" => Some(Icon::ChevronRight),
                _ => None,
            };
            if let Some(icon) = icon {
                return Ok(Node::Icon {
                    icon,
                    color,
                    label: label.into(),
                });
            }
            let symbol = match symbol.as_str() {
                "previous" => Symbol::Previous,
                "next" => Symbol::Next,
                "microphone" => Symbol::Microphone,
                "brightness" => Symbol::Brightness,
                "battery" => Symbol::Battery,
                "wifi" => Symbol::Wifi,
                "bluetooth" => Symbol::Bluetooth,
                "workspace" => Symbol::Workspace,
                "capture" => Symbol::Capture,
                "timer" => Symbol::Timer,
                "terminal" => Symbol::Terminal,
                "graph" => Symbol::Graph,
                _ => bail!("unknown symbol {symbol}"),
            };
            return self.set.symbols.borrow_mut().node(symbol, 32, color, label);
        }
        if let Some(asset) = self.string(&node.asset, &node.asset_from, record)? {
            let image = self
                .assets
                .get(&asset)
                .with_context(|| format!("unknown asset {asset}"))?
                .clone();
            return Ok(Node::Image {
                image,
                opacity: 1.0,
                fit: ImageFit::Contain,
                tint: ImageTint::Mask(color),
                label: label.into(),
            });
        }
        let text = self
            .string(&node.text, &node.text_from, record)?
            .unwrap_or_else(|| label.into());
        let text = self.text(text)?;
        let size = node.font_size.unwrap_or(16.0);
        if !(1.0..=64.0).contains(&size) {
            bail!("font size outside host bounds");
        }
        let mut leaf = Node::label(text, size);
        if let Node::Label {
            color: leaf_color, ..
        } = &mut leaf
        {
            *leaf_color = color;
        }
        Ok(leaf)
    }
}

fn flex(node: &LayoutNode) -> Flex {
    if let Some(width) = node.width {
        return Flex::fixed(width);
    }
    if node.kind == LayoutKind::FixedSpace {
        return Flex::fixed(node.minimum.unwrap_or(0.0));
    }
    let mut flex = if node.kind == LayoutKind::FlexibleSpace {
        Flex::flexible(
            node.minimum.unwrap_or(0.0),
            0.0,
            node.maximum.unwrap_or(8192.0),
        )
    } else {
        Flex::content(node.minimum.unwrap_or(44.0), node.maximum.unwrap_or(8192.0))
    };
    if let Some(grow) = node.grow {
        flex.grow = grow;
    }
    flex
}

fn color_role(role: ConfigRole) -> ColorRole {
    match role {
        ConfigRole::Background => ColorRole::Background,
        ConfigRole::Control => ColorRole::Control,
        ConfigRole::ControlPressed => ColorRole::ControlPressed,
        ConfigRole::Track => ColorRole::Track,
        ConfigRole::Foreground => ColorRole::Foreground,
        ConfigRole::Muted => ColorRole::Muted,
        ConfigRole::Accent => ColorRole::Accent,
        ConfigRole::Destructive => ColorRole::Destructive,
        ConfigRole::OnAccent => ColorRole::OnAccent,
        ConfigRole::OnDestructive => ColorRole::OnDestructive,
    }
}
