//! Shared presentation composition metadata.
use serde::{Deserialize, Serialize};

/// One package-owned ordered bar used as presentation content. References are
/// deliberately local item IDs; the installer/session compositor qualifies
/// them with the immutable package identity before composition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresentationBar {
    pub id: String,
    pub minimum_width: u32,
    pub preferred_width: u32,
    pub maximum_width: u32,
    pub dismiss_on_selection: bool,
    #[serde(default, rename = "element")]
    pub elements: Vec<PresentationBarElement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal_item: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PresentationBarElement {
    Item {
        item: String,
        minimum_width: u32,
        preferred_width: u32,
        maximum_width: u32,
    },
    FixedSpace {
        width: u32,
    },
    FlexibleSpace {
        minimum: u32,
        weight: u32,
    },
    /// An atomic, recursively composited cluster. Groups intentionally cannot
    /// contain spaces: their child geometry is entirely owned by the group.
    Group {
        id: String,
        #[serde(default)]
        layout: PresentationGroupLayout,
        #[serde(default)]
        spacing: u32,
        #[serde(default)]
        visibility_priority: i32,
        #[serde(default)]
        compression_priority: i32,
        #[serde(default, rename = "element")]
        elements: Vec<PresentationGroupElement>,
    },
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PresentationGroupLayout {
    #[default]
    Natural,
    EqualWidth,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum PresentationGroupElement {
    Item {
        item: String,
        minimum_width: u32,
        preferred_width: u32,
        maximum_width: u32,
    },
    Group {
        id: String,
        #[serde(default)]
        layout: PresentationGroupLayout,
        #[serde(default)]
        spacing: u32,
        #[serde(default)]
        visibility_priority: i32,
        #[serde(default)]
        compression_priority: i32,
        #[serde(default, rename = "element")]
        elements: Vec<PresentationGroupElement>,
    },
}
