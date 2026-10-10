use std::{path::PathBuf, time::Duration};
use touchbar_plugin_host::{Appearance, HostLimits, InputEvent, InputKind, sets::ButtonSetHost};
use touchbar_set_config::{SetDocument, StateValue};
use touchbar_ui::Rect;
#[test]
fn layout_only_edit_needs_no_build() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/button-set-logic");
    let wasm = std::fs::read(
        root.join("../../target/wasm32-wasip2/release/touchbar_button_set_logic_demo.wasm"),
    )
    .unwrap();
    let first =
        SetDocument::from_toml(&std::fs::read_to_string(root.join("sets/counter.toml")).unwrap())
            .unwrap();
    let compact = SetDocument::from_toml(
        &std::fs::read_to_string(root.join("sets/compact-counter.toml")).unwrap(),
    )
    .unwrap();
    let mut a =
        ButtonSetHost::from_bytes_with_broker(&wasm, first, HostLimits::default(), None).unwrap();
    let mut b =
        ButtonSetHost::from_bytes_with_broker(&wasm, compact, HostLimits::default(), None).unwrap();
    for (host, id, width) in [
        (&mut a, "counter", 240.0),
        (&mut b, "compact-counter", 120.0),
    ] {
        host.render_at(
            id,
            Rect::new(0.0, 0.0, width, 60.0),
            Appearance::default(),
            Duration::ZERO,
        )
        .unwrap();
    }
    a.handle_event(&InputEvent {
        item_id: "counter".into(),
        widget_id: 10,
        kind: InputKind::Pressed,
        value: None,
        contact_id: Some(1),
        activation: None,
    })
    .unwrap();
    a.handle_event(&InputEvent {
        item_id: "counter".into(),
        widget_id: 10,
        kind: InputKind::Activated,
        value: None,
        contact_id: Some(1),
        activation: None,
    })
    .unwrap();
    assert_eq!(a.snapshot().0["count"], StateValue::Number(1.0));
    assert_eq!(b.snapshot().0["count"], StateValue::Number(0.0));
}

#[test]
fn deleting_set_releases_resources() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use touchbar_plugin_supervisor::{
        ActivationLedger, BackendRequest, ConnectionIdentity, ConnectionLimits, OpenedResource,
        ResourceBackend, ResourceEventSink, ResourceHandle, ResourceLimits, SupervisorConnection,
    };
    use touchbar_policy::{
        CapabilityId, CapabilityRegistry, Decision, GrantBindings, GrantRecord, GrantStore,
        PackageInstance, Provenance, ReusePolicy, RuntimeKind, SessionGrants,
        calculate_effective_policy,
    };
    use touchbar_protocol::broker_ipc::{BrokerErrorCode, Seqpacket};
    struct Handle(Arc<AtomicUsize>);
    impl ResourceHandle for Handle {
        fn close(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Backend(Arc<AtomicUsize>);
    impl ResourceBackend for Backend {
        fn limits(&self, _request: &BackendRequest) -> Option<ResourceLimits> {
            Some(ResourceLimits {
                reserved_buffered_bytes: 16 * 1024,
                maximum_events_per_second: 2,
            })
        }
        fn authorize(
            &self,
            request: &BackendRequest,
            _activations: &mut ActivationLedger,
            _now: u64,
        ) -> Result<(), BrokerErrorCode> {
            assert_eq!(request.capability, CapabilityId::MediaReadV1);
            Ok(())
        }
        fn open(
            &self,
            resource_id: u64,
            _request: &BackendRequest,
            _events: ResourceEventSink,
        ) -> Result<OpenedResource, BrokerErrorCode> {
            Ok(OpenedResource {
                handle: Box::new(Handle(self.0.clone())),
                response_payload: touchbar_broker_schema::MediaSubscriptionOpened {
                    resource_id,
                    state: touchbar_broker_schema::MediaState {
                        generation: 1,
                        session: None,
                    },
                }
                .encode()
                .unwrap(),
            })
        }
    }
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/media");
    let manifest = touchbar_package::PluginManifest::from_toml(
        &std::fs::read_to_string(root.join("touchbar-plugin.toml")).unwrap(),
    )
    .unwrap();
    let package = PackageInstance {
        source: manifest.plugin.source.clone(),
        version: manifest.plugin.version.clone(),
        digest: format!("sha256:{}", "a".repeat(64)),
        provenance: Provenance::LocalDevelopment,
        runtime: RuntimeKind::Component,
    };
    let registry = CapabilityRegistry::default();
    let requests = registry.normalize(&manifest).unwrap();
    let request = requests
        .iter()
        .find(|r| r.capability == CapabilityId::MediaReadV1)
        .unwrap();
    let mut grants = GrantStore::default();
    grants
        .insert(GrantRecord {
            source: package.source.clone(),
            capability: request.capability.clone(),
            approved_scope: request.scope.clone(),
            bindings: GrantBindings::default(),
            decision: Decision::Allow,
            reuse: ReusePolicy::ExactDigest,
            approved_version: package.version.clone(),
            approved_digest: package.digest.clone(),
        })
        .unwrap();
    let policy = calculate_effective_policy(
        &package,
        &requests,
        &grants,
        &SessionGrants::default(),
        &registry,
    );
    let wasm = std::fs::read(root.join("component/plugin.wasm")).unwrap();
    let set =
        SetDocument::from_toml(&std::fs::read_to_string(root.join("sets/timeline.toml")).unwrap())
            .unwrap();
    let open = |instance_id| {
        let (client, server) = Seqpacket::pair().unwrap();
        let closes = Arc::new(AtomicUsize::new(0));
        let mut connection = SupervisorConnection::new(
            server,
            ConnectionIdentity {
                instance_id,
                package: package.clone(),
            },
            policy.clone(),
            ConnectionLimits::default(),
        )
        .unwrap();
        connection.register_resource_backend(
            CapabilityId::MediaReadV1,
            Arc::new(Backend(closes.clone())),
        );
        let worker = std::thread::spawn(move || {
            connection.serve_until_disconnect().unwrap();
        });
        let broker = touchbar_plugin_host::BrokerClient::connect(client).unwrap();
        let mut host = ButtonSetHost::from_bytes_with_broker(
            &wasm,
            set.clone(),
            HostLimits::default(),
            Some(broker),
        )
        .unwrap();
        host.announce_capabilities().unwrap();
        let mut fd = libc::pollfd {
            fd: host.broker_event_fd().unwrap(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut fd, 1, 1000) }, 1);
        host.dispatch_broker_events().unwrap();
        assert_eq!(closes.load(Ordering::SeqCst), 0);
        (host, worker, closes)
    };
    let (a, wa, ca) = open(41);
    let (b, wb, cb) = open(42);
    // Dropping the removed set disconnects its broker and releases its handle.
    drop(a);
    wa.join().unwrap();
    assert_eq!(ca.load(Ordering::SeqCst), 1);
    assert_eq!(cb.load(Ordering::SeqCst), 0);
    drop(b);
    wb.join().unwrap();
    assert_eq!(cb.load(Ordering::SeqCst), 1);
}
