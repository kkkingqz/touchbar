//! `keyboard.shortcut.v1`: press one fixed shortcut in a named application.
//!
//! The grant fixes every shortcut's keys and the applications it may be
//! pressed in; the component names only the shortcut. This backend checks
//! the grant and consumes a fresh touch, then hands the press to the session
//! daemon over the private channel it gave this supervisor. The daemon knows
//! which window has the focus and owns the desktop's input channel; it checks
//! the grant again and presses the keys only while one of the shortcut's
//! applications is focused.

use std::{
    os::fd::AsRawFd,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use touchbar_broker_schema::{
    KeyboardShortcutRequest, SchemaError, ShortcutDispatch, ShortcutOutcome, ShortcutResult,
};
use touchbar_policy::{CapabilityId, CapabilityScope};
use touchbar_protocol::broker_ipc::{BrokerErrorCode, BrokerResult, Seqpacket};

use crate::{ActivationExpectation, ActivationLedger, Backend, BackendRequest, CancellationToken};

pub const KEYBOARD_SHORTCUT_OPERATION: &str = "press";
/// How often a waiting press checks for cancellation.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub struct KeyboardShortcutBackend {
    /// `None` outside a session daemon, where no shortcut can be pressed.
    channel: Option<Mutex<Seqpacket>>,
    next_request: AtomicU64,
}

impl KeyboardShortcutBackend {
    pub fn new(channel: Option<Seqpacket>) -> Self {
        Self {
            channel: channel.map(Mutex::new),
            next_request: AtomicU64::new(1),
        }
    }

    /// Sends one press and waits for its outcome. Presses are serialized:
    /// the channel carries one at a time, and a late outcome of an abandoned
    /// press is skipped by its request id.
    fn press(
        &self,
        shortcut: &str,
        cancellation: &CancellationToken,
    ) -> Result<ShortcutResult, BrokerErrorCode> {
        let channel = self.channel.as_ref().ok_or(BrokerErrorCode::Unavailable)?;
        let channel = channel.lock().unwrap_or_else(PoisonError::into_inner);
        let request_id = self.next_request.fetch_add(1, Ordering::Relaxed);
        let dispatch = ShortcutDispatch {
            request_id,
            shortcut: shortcut.into(),
        }
        .encode()
        .map_err(schema_error)?;
        channel
            .send_payload(&dispatch)
            .map_err(|_| BrokerErrorCode::Unavailable)?;
        loop {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            match channel.try_recv_payload() {
                Ok(Some(payload)) => {
                    let outcome = ShortcutOutcome::decode(&payload)
                        .map_err(|_| BrokerErrorCode::BackendFailed)?;
                    if outcome.request_id == request_id {
                        return Ok(outcome.result);
                    }
                }
                Ok(None) => wait_readable(&channel),
                Err(_) => return Err(BrokerErrorCode::Unavailable),
            }
        }
    }
}

fn wait_readable(channel: &Seqpacket) {
    let mut descriptor = libc::pollfd {
        fd: channel.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: descriptor is one valid pollfd for a live socket.
    unsafe { libc::poll(&mut descriptor, 1, POLL_INTERVAL.as_millis() as i32) };
}

fn schema_error(error: SchemaError) -> BrokerErrorCode {
    match error {
        SchemaError::Malformed => BrokerErrorCode::InvalidRequest,
        SchemaError::LimitExceeded => BrokerErrorCode::QuotaExceeded,
    }
}

/// The requested shortcut, which must be in the authorized scope.
fn decode(request: &BackendRequest) -> Result<String, BrokerErrorCode> {
    let CapabilityScope::KeyboardShortcut(scope) = &request.authorized_scope else {
        return Err(BrokerErrorCode::InvalidRequest);
    };
    if request.capability != CapabilityId::KeyboardShortcutV1
        || request.operation != KEYBOARD_SHORTCUT_OPERATION
    {
        return Err(BrokerErrorCode::InvalidRequest);
    }
    let shortcut = KeyboardShortcutRequest::decode(&request.payload)
        .map_err(schema_error)?
        .shortcut;
    if scope.shortcut(&shortcut).is_none() {
        return Err(BrokerErrorCode::OutOfScope);
    }
    Ok(shortcut)
}

impl Backend for KeyboardShortcutBackend {
    /// Every press needs a fresh touch on the component's own control.
    fn authorize(
        &self,
        request: &BackendRequest,
        activations: &mut ActivationLedger,
        now_monotonic_micros: u64,
    ) -> Result<(), BrokerErrorCode> {
        decode(request)?;
        let activation = request
            .activation
            .as_ref()
            .ok_or(BrokerErrorCode::ActivationRequired)?;
        activations.consume(
            Some(activation),
            ActivationExpectation {
                surface_instance: activation.surface_instance,
                item_id: &activation.item_id,
                widget_id: activation.widget_id,
                now_monotonic_micros,
            },
        )?;
        Ok(())
    }

    fn execute(&self, request: &BackendRequest, cancellation: &CancellationToken) -> BrokerResult {
        let result = (|| {
            if let Some(reason) = cancellation.reason() {
                return Err(reason);
            }
            let shortcut = decode(request)?;
            match self.press(&shortcut, cancellation)? {
                ShortcutResult::Pressed => Ok(()),
                // The shortcut's application is not in front: nothing to press.
                ShortcutResult::NotFocused | ShortcutResult::Unavailable => {
                    Err(BrokerErrorCode::Unavailable)
                }
                ShortcutResult::NotGranted => Err(BrokerErrorCode::OutOfScope),
                ShortcutResult::Failed => Err(BrokerErrorCode::BackendFailed),
            }
        })();
        match result {
            Ok(()) => BrokerResult::Success {
                payload: Vec::new(),
            },
            Err(error) => BrokerResult::Error(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::{Arc, atomic::AtomicBool},
        thread,
    };

    use semver::Version;
    use touchbar_package::GithubSource;
    use touchbar_policy::{
        GrantBindings, KeyboardShortcut, KeyboardShortcutScope, PackageInstance, Provenance,
        RuntimeKind, ShortcutKey,
    };
    use touchbar_protocol::broker_ipc::{ActivationContext, ActivationOrigin};

    use super::*;
    use crate::ConnectionIdentity;

    fn request(shortcut: &str, activation: Option<ActivationContext>) -> BackendRequest {
        BackendRequest {
            identity: ConnectionIdentity {
                instance_id: 5,
                package: PackageInstance {
                    source: GithubSource::new("alice", "touchbar-actions").unwrap(),
                    version: Version::new(1, 0, 0),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    provenance: Provenance::VerifiedRelease,
                    runtime: RuntimeKind::Component,
                },
            },
            request_id: 1,
            capability: CapabilityId::KeyboardShortcutV1,
            authorized_scope: CapabilityScope::KeyboardShortcut(KeyboardShortcutScope {
                shortcuts: BTreeSet::from([KeyboardShortcut {
                    id: "back".into(),
                    keys: vec![ShortcutKey::Alt, ShortcutKey::Left],
                    applications: BTreeSet::from(["firefox_firefox".into()]),
                }]),
            }),
            bindings: GrantBindings::default(),
            activation,
            operation: KEYBOARD_SHORTCUT_OPERATION.into(),
            payload: KeyboardShortcutRequest {
                shortcut: shortcut.into(),
            }
            .encode()
            .unwrap(),
        }
    }

    fn activation(sequence: u64) -> ActivationContext {
        ActivationContext {
            origin: ActivationOrigin::Physical,
            surface_instance: 4,
            item_id: "browser".into(),
            widget_id: 2,
            input_sequence: sequence,
            deadline_monotonic_micros: u64::MAX,
        }
    }

    fn token() -> CancellationToken {
        CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_secs(2)).unwrap()
    }

    #[test]
    fn a_press_needs_a_granted_shortcut_and_one_fresh_touch() {
        let backend = KeyboardShortcutBackend::new(None);
        let mut ledger = ActivationLedger::new(8);
        assert_eq!(
            backend.authorize(&request("close", Some(activation(1))), &mut ledger, 0),
            Err(BrokerErrorCode::OutOfScope)
        );
        assert_eq!(
            backend.authorize(&request("back", None), &mut ledger, 0),
            Err(BrokerErrorCode::ActivationRequired)
        );
        assert_eq!(
            backend.authorize(&request("back", Some(activation(2))), &mut ledger, 0),
            Ok(())
        );
        assert_eq!(
            backend.authorize(&request("back", Some(activation(2))), &mut ledger, 0),
            Err(BrokerErrorCode::ActivationRequired),
            "a touch presses once"
        );
        // Outside a session daemon nothing can be pressed.
        assert_eq!(
            backend.execute(&request("back", None), &token()),
            BrokerResult::Error(BrokerErrorCode::Unavailable)
        );
    }

    #[test]
    fn the_session_daemons_outcome_is_the_result() {
        let (daemon, supervisor) = Seqpacket::pair().unwrap();
        let backend = KeyboardShortcutBackend::new(Some(supervisor));
        let daemon = thread::spawn(move || {
            let mut seen = Vec::new();
            for result in [ShortcutResult::Pressed, ShortcutResult::NotFocused] {
                let dispatch = loop {
                    if let Some(payload) = daemon.try_recv_payload().unwrap() {
                        break ShortcutDispatch::decode(&payload).unwrap();
                    }
                    thread::sleep(Duration::from_millis(5));
                };
                seen.push(dispatch.shortcut.clone());
                // A stale outcome first: it must be skipped.
                let stale = ShortcutOutcome {
                    request_id: dispatch.request_id + 100,
                    result: ShortcutResult::Failed,
                };
                daemon.send_payload(&stale.encode().unwrap()).unwrap();
                let outcome = ShortcutOutcome {
                    request_id: dispatch.request_id,
                    result,
                };
                daemon.send_payload(&outcome.encode().unwrap()).unwrap();
            }
            seen
        });
        assert_eq!(
            backend.execute(&request("back", None), &token()),
            BrokerResult::Success {
                payload: Vec::new()
            }
        );
        assert_eq!(
            backend.execute(&request("back", None), &token()),
            BrokerResult::Error(BrokerErrorCode::Unavailable),
            "the browser was not in front"
        );
        assert_eq!(daemon.join().unwrap(), ["back", "back"]);
    }

    #[test]
    fn a_press_the_daemon_never_answers_times_out() {
        let (_daemon, supervisor) = Seqpacket::pair().unwrap();
        let backend = KeyboardShortcutBackend::new(Some(supervisor));
        let token =
            CancellationToken::new(Arc::new(AtomicBool::new(false)), Duration::from_millis(120))
                .unwrap();
        assert_eq!(
            backend.execute(&request("back", None), &token),
            BrokerResult::Error(BrokerErrorCode::Timeout)
        );
    }
}
