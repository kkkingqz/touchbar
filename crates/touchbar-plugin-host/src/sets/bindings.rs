//! Stable widget identity and contact routing.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, bail};
use touchbar_set_config::{ActionEvent, InputKind, ScalarValue};

use crate::{InputEvent, InputKind as HostKind};

const MAX_CONTACTS: usize = 32;

#[derive(Clone, Debug, PartialEq)]
pub(super) struct BoundWidget {
    pub actions: Vec<(InputKind, String)>,
    pub record_key: Option<String>,
}

impl BoundWidget {
    fn event(
        &self,
        kind: InputKind,
        value: Option<f32>,
        contact: Option<u32>,
    ) -> Option<ActionEvent> {
        let action = self
            .actions
            .iter()
            .find(|(input, _)| *input == kind)?
            .1
            .clone();
        Some(ActionEvent {
            action,
            kind,
            value: value.map(|value| ScalarValue::Number(f64::from(value))),
            contact_id: contact.map_or(0, u64::from),
            record_key: self.record_key.clone(),
        })
    }
}

#[derive(Default)]
pub(super) struct Routing {
    widgets: BTreeMap<u64, BoundWidget>,
    contacts: BTreeMap<u32, (u64, BoundWidget)>,
    cancelled_contacts: BTreeSet<u32>,
    pending_cancelled: Vec<ActionEvent>,
}

impl Routing {
    pub fn replace(&mut self, widgets: BTreeMap<u64, BoundWidget>) -> Result<()> {
        let removed: Vec<_> = self
            .contacts
            .iter()
            .filter(|(_, (id, target))| widgets.get(id) != Some(target))
            .map(|(contact, (_, target))| (*contact, target.clone()))
            .collect();
        if self.pending_cancelled.len() + removed.len() > MAX_CONTACTS {
            bail!("undelivered cancellation budget exceeded");
        }
        for (contact, target) in removed {
            self.contacts.remove(&contact);
            self.cancelled_contacts.insert(contact);
            if let Some(event) = target.event(InputKind::Cancelled, None, Some(contact)) {
                self.pending_cancelled.push(event);
            }
        }
        // Tombstones only suppress late events; they hold no resources.
        while self.cancelled_contacts.len() > MAX_CONTACTS * 8 {
            self.cancelled_contacts.pop_first();
        }
        self.widgets = widgets;
        Ok(())
    }

    pub fn action_for(&mut self, event: &InputEvent) -> Result<Option<ActionEvent>> {
        let kind = match event.kind {
            HostKind::Pressed => InputKind::Pressed,
            HostKind::Activated => InputKind::Activated,
            HostKind::LongPressed => InputKind::LongPressed,
            HostKind::ValueChanged => InputKind::ValueChanged,
            HostKind::Released => InputKind::Released,
            HostKind::Cancelled => InputKind::Cancelled,
        };
        if event.value.is_some_and(|value| !value.is_finite()) {
            bail!("nonfinite input value");
        }
        if kind == InputKind::ValueChanged && event.value.is_none() {
            bail!("ValueChanged lacks its value");
        }
        if let Some(contact) = event.contact_id {
            if kind == InputKind::Pressed {
                self.cancelled_contacts.remove(&contact);
                if let Some(widget) = self.widgets.get(&event.widget_id) {
                    if !self.contacts.contains_key(&contact) && self.contacts.len() >= MAX_CONTACTS
                    {
                        bail!("too many active contacts");
                    }
                    self.contacts
                        .insert(contact, (event.widget_id, widget.clone()));
                }
            } else if self.cancelled_contacts.contains(&contact) {
                if matches!(kind, InputKind::Released | InputKind::Cancelled) {
                    self.cancelled_contacts.remove(&contact);
                }
                return Ok(None);
            }
            if let Some((id, _)) = self.contacts.get(&contact)
                && *id != event.widget_id
            {
                bail!("contact changed its target widget");
            }
        }
        let target = event
            .contact_id
            .and_then(|contact| self.contacts.get(&contact).map(|(_, target)| target))
            .or_else(|| self.widgets.get(&event.widget_id));
        let result = target.and_then(|target| target.event(kind, event.value, event.contact_id));
        if matches!(
            kind,
            InputKind::Activated | InputKind::Released | InputKind::Cancelled
        ) && let Some(contact) = event.contact_id
        {
            self.contacts.remove(&contact);
        }
        Ok(result)
    }

    pub fn drain_cancelled(&mut self) -> Vec<ActionEvent> {
        std::mem::take(&mut self.pending_cancelled)
    }
}

/// Length delimiters prevent a record key containing separators from aliasing
/// another path. Stable IDs do not depend on the record's display position.
pub(super) fn stable_id(set: &str, node: &str, record: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for part in [set, node, record] {
        for byte in part.len().to_le_bytes().into_iter().chain(part.bytes()) {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
        }
    }
    hash.max(1)
}
