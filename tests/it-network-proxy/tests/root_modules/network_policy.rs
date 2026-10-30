// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#[cfg(test)]
pub(crate) mod test_support {
    pub(crate) const POLICY_DECISION_EVENT_NAME: &str = super::POLICY_DECISION_EVENT_NAME;

    use std::collections::BTreeMap;
    use std::fmt;
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;
    use tracing::Event;
    use tracing::Id;
    use tracing::Metadata;
    use tracing::Subscriber;
    use tracing::field::Field;
    use tracing::field::Visit;
    use tracing::instrument::WithSubscriber;
    use tracing::span::Attributes;
    use tracing::span::Record;
    use tracing::subscriber::Interest;

    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(crate) struct CapturedEvent {
        pub target: String,
        pub fields: BTreeMap<String, String>,
    }

    impl CapturedEvent {
        pub fn field(&self, name: &str) -> Option<&str> {
            self.fields.get(name).map(String::as_str)
        }
    }

    #[derive(Clone, Default)]
    struct EventCollector {
        events: Arc<Mutex<Vec<CapturedEvent>>>,
        next_span_id: Arc<AtomicU64>,
    }

    impl EventCollector {
        fn events(&self) -> Vec<CapturedEvent> {
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
        }
    }

    impl Subscriber for EventCollector {
        fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
            true
        }

        fn register_callsite(&self, _metadata: &'static Metadata<'static>) -> Interest {
            Interest::always()
        }

        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::TRACE)
        }

        fn new_span(&self, _span: &Attributes<'_>) -> Id {
            Id::from_u64(self.next_span_id.fetch_add(1, Ordering::Relaxed) + 1)
        }

        fn record(&self, _span: &Id, _values: &Record<'_>) {}

        fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(CapturedEvent {
                    target: event.metadata().target().to_string(),
                    fields: visitor.fields,
                });
        }

        fn enter(&self, _span: &Id) {}

        fn exit(&self, _span: &Id) {}
    }

    #[derive(Default)]
    struct FieldVisitor {
        fields: BTreeMap<String, String>,
    }

    impl FieldVisitor {
        fn insert(&mut self, field: &Field, value: impl Into<String>) {
            self.fields.insert(field.name().to_string(), value.into());
        }
    }

    impl Visit for FieldVisitor {
        fn record_str(&mut self, field: &Field, value: &str) {
            self.insert(field, value);
        }

        fn record_bool(&mut self, field: &Field, value: bool) {
            self.insert(field, value.to_string());
        }

        fn record_i64(&mut self, field: &Field, value: i64) {
            self.insert(field, value.to_string());
        }

        fn record_u64(&mut self, field: &Field, value: u64) {
            self.insert(field, value.to_string());
        }

        fn record_i128(&mut self, field: &Field, value: i128) {
            self.insert(field, value.to_string());
        }

        fn record_u128(&mut self, field: &Field, value: u128) {
            self.insert(field, value.to_string());
        }

        fn record_f64(&mut self, field: &Field, value: f64) {
            self.insert(field, value.to_string());
        }

        fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
            self.insert(field, value.to_string());
        }

        fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
            self.insert(field, format!("{value:?}"));
        }
    }

    pub(crate) async fn capture_events<F, Fut, T>(f: F) -> (T, Vec<CapturedEvent>)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = T>,
    {
        let collector = EventCollector::default();
        // Keep tracing out of its single-subscriber fast path: concurrent tests
        // without a subscriber can otherwise cache this callsite as disabled.
        let _interest_dispatch = tracing::Dispatch::new(collector.clone());
        let output = async {
            tracing::callsite::rebuild_interest_cache();
            f().await
        }
        .with_subscriber(collector.clone())
        .await;
        let events = collector.events();
        (output, events)
    }

    pub(crate) fn find_event_by_name<'a>(
        events: &'a [CapturedEvent],
        event_name: &str,
    ) -> Option<&'a CapturedEvent> {
        events
            .iter()
            .find(|event| event.field("event.name") == Some(event_name))
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::capture_events;
    use super::test_support::find_event_by_name;
    use super::*;
    use crate::ExecutorLogIdentity;
    use crate::NetworkProxyProcessLogMetadata;
    use crate::config::NetworkMode;
    use crate::config::NetworkProxyConfig;
    use crate::reasons::REASON_DENIED;
    use crate::reasons::REASON_METHOD_NOT_ALLOWED;
    use crate::reasons::REASON_NOT_ALLOWED;
    use crate::reasons::REASON_NOT_ALLOWED_LOCAL;
    use crate::runtime::ConfigReloader;
    use crate::runtime::ConfigReloaderFuture;
    use crate::runtime::ConfigState;
    use crate::runtime::NetworkProxyAuditMetadata;
    use crate::state::NetworkProxyConstraints;
    use crate::state::build_config_state;
    use crate::state::network_proxy_state_for_policy;
    use pretty_assertions::assert_eq;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    const LEGACY_DOMAIN_POLICY_DECISION_EVENT_NAME: &str =
        "codex.network_proxy.domain_policy_decision";
    const LEGACY_BLOCK_DECISION_EVENT_NAME: &str = "codex.network_proxy.block_decision";

    #[derive(Clone)]
    struct StaticReloader {
        state: ConfigState,
    }

    impl ConfigReloader for StaticReloader {
        fn maybe_reload(&self) -> ConfigReloaderFuture<'_, Option<ConfigState>> {
            Box::pin(async { Ok(None) })
        }

        fn reload_now(&self) -> ConfigReloaderFuture<'_, ConfigState> {
            Box::pin(async { Ok(self.state.clone()) })
        }

        fn source_label(&self) -> String {
            "static test reloader".to_string()
        }
    }

    fn state_with_metadata(metadata: NetworkProxyAuditMetadata) -> NetworkProxyState {
        let network = NetworkProxyConfig {
            enabled: true,
            mode: NetworkMode::Full,
            ..NetworkProxyConfig::default()
        };
        let config = network;
        let state = build_config_state(
            config,
            NetworkProxyConstraints::default(),
            crate::Platform::native(),
        )
        .unwrap();
        let reloader = Arc::new(StaticReloader {
            state: state.clone(),
        });
        NetworkProxyState::with_reloader_and_audit_metadata(
            state,
            reloader,
            metadata,
            crate::LocalBindingPolicy::DefaultFalse,
        )
    }

    fn is_rfc3339_utc_millis(timestamp: &str) -> bool {
        let bytes = timestamp.as_bytes();
        if bytes.len() != 24 {
            return false;
        }
        bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[10] == b'T'
            && bytes[13] == b':'
            && bytes[16] == b':'
            && bytes[19] == b'.'
            && bytes[23] == b'Z'
            && bytes.iter().enumerate().all(|(idx, value)| match idx {
                4 | 7 | 10 | 13 | 16 | 19 | 23 => true,
                _ => value.is_ascii_digit(),
            })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn policy_audit_observer_receives_domain_and_non_domain_decisions() {
        let mut state = network_proxy_state_for_policy(NetworkProxyConfig::default());
        let (captured_tx, captured_rx) = std::sync::mpsc::channel();
        state.set_policy_audit_observer(Arc::new(move |event| {
            captured_tx
                .send(event)
                .expect("observer should capture the policy decision");
        }));
        let decider: Arc<dyn NetworkPolicyDecider> =
            Arc::new(|_request| async { NetworkDecision::Allow });
        let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
            protocol: NetworkProtocol::Http,
            host: "example.com".to_string(),
            port: 80,
            environment_id: None,
            client_addr: None,
            method: None,
            command: None,
            exec_policy_hint: None,
        });
        evaluate_host_policy(&state, Some(&decider), &request)
            .await
            .expect("evaluate domain policy");
        emit_block_decision_audit_event(
            &state,
            BlockDecisionAuditEventArgs {
                source: NetworkDecisionSource::ModeGuard,
                reason: REASON_METHOD_NOT_ALLOWED,
                protocol: NetworkProtocol::Http,
                server_address: "unix-socket",
                server_port: 0,
                method: Some("POST"),
                client_addr: None,
            },
        );

        let events: Vec<_> = captured_rx.try_iter().collect();
        assert_eq!(
            events
                .iter()
                .map(|event| (event.scope.as_str(), event.decision.as_str()))
                .collect::<Vec<_>>(),
            vec![("domain", "allow"), ("non_domain", "deny")]
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_host_policy_emits_domain_event_for_decider_allow_override() {
        let state = network_proxy_state_for_policy(NetworkProxyConfig::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let decider: Arc<dyn NetworkPolicyDecider> = Arc::new({
            let calls = calls.clone();
            move |_req| {
                calls.fetch_add(1, Ordering::SeqCst);
                // The default policy denies all; the decider is consulted for not_allowed
                // requests and can override that decision.
                async { NetworkDecision::Allow }
            }
        });

        let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
            protocol: NetworkProtocol::Http,
            host: "example.com".to_string(),
            port: 80,
            environment_id: None,
            client_addr: None,
            method: None,
            command: None,
            exec_policy_hint: None,
        });

        let (decision, events) = capture_events(|| async {
            evaluate_host_policy(&state, Some(&decider), &request)
                .await
                .unwrap()
        })
        .await;
        assert_eq!(decision, NetworkDecision::Allow);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision audit event");
        assert_eq!(event.target, AUDIT_TARGET);
        assert!(event.target.starts_with("codex_otel."));
        assert_eq!(
            event.field("network.policy.scope"),
            Some(POLICY_SCOPE_DOMAIN)
        );
        assert_eq!(event.field("network.policy.decision"), Some("allow"));
        assert_eq!(event.field("network.policy.source"), Some("decider"));
        assert_eq!(
            event.field("network.policy.reason"),
            Some(REASON_NOT_ALLOWED)
        );
        assert_eq!(event.field("network.transport.protocol"), Some("http"));
        assert_eq!(event.field("server.address"), Some("example.com"));
        assert_eq!(event.field("server.port"), Some("80"));
        assert_eq!(event.field("http.request.method"), Some(DEFAULT_METHOD));
        assert_eq!(event.field("client.address"), Some(DEFAULT_CLIENT_ADDRESS));
        assert_eq!(event.field("network.policy.override"), Some("true"));
        let timestamp = event
            .field("event.timestamp")
            .expect("event timestamp should be present");
        assert!(is_rfc3339_utc_millis(timestamp));
        assert_eq!(
            find_event_by_name(&events, LEGACY_DOMAIN_POLICY_DECISION_EVENT_NAME),
            None
        );
        assert_eq!(
            find_event_by_name(&events, LEGACY_BLOCK_DECISION_EVENT_NAME),
            None
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_host_policy_emits_execution_id_for_baseline_allow() {
        let state = network_proxy_state_for_policy({
            let mut network = NetworkProxyConfig::default();
            network.set_allowed_domains(vec!["example.com".to_string()]);
            network
        });
        state.register_execution("token-baseline-allow", "local", "execution-baseline-allow");
        let state = state
            .for_execution_token("token-baseline-allow")
            .expect("expected registered execution");
        let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
            protocol: NetworkProtocol::Http,
            host: "example.com".to_string(),
            port: 80,
            environment_id: None,
            client_addr: None,
            method: None,
            command: None,
            exec_policy_hint: None,
        });

        let (decision, events) = capture_events(|| async {
            evaluate_host_policy(&state, /*decider*/ None, &request)
                .await
                .unwrap()
        })
        .await;
        assert_eq!(decision, NetworkDecision::Allow);

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision audit event");
        assert_eq!(event.field("network.policy.decision"), Some("allow"));
        assert_eq!(
            event.field("execution.id"),
            Some("execution-baseline-allow")
        );
        assert_ne!(event.field("execution.id"), Some("token-baseline-allow"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_host_policy_emits_domain_event_for_baseline_deny() {
        let state = network_proxy_state_for_policy({
            let mut network = NetworkProxyConfig::default();
            network.set_allowed_domains(vec!["example.com".to_string()]);
            network.set_denied_domains(vec!["blocked.com".to_string()]);
            network
        });
        state.register_execution("token-baseline-deny", "local", "execution-baseline-deny");
        let state = state
            .for_execution_token("token-baseline-deny")
            .expect("expected registered execution");
        let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
            protocol: NetworkProtocol::Http,
            host: "blocked.com".to_string(),
            port: 80,
            environment_id: None,
            client_addr: Some("127.0.0.1:1234".to_string()),
            method: Some("GET".to_string()),
            command: None,
            exec_policy_hint: None,
        });

        let (decision, events) = capture_events(|| async {
            evaluate_host_policy(&state, /*decider*/ None, &request)
                .await
                .unwrap()
        })
        .await;
        assert_eq!(
            decision,
            NetworkDecision::Deny {
                reason: REASON_DENIED.to_string(),
                source: NetworkDecisionSource::BaselinePolicy,
                decision: NetworkPolicyDecision::Deny,
            }
        );

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision audit event");
        assert_eq!(event.field("network.policy.decision"), Some("deny"));
        assert_eq!(
            event.field("network.policy.source"),
            Some("baseline_policy")
        );
        assert_eq!(event.field("network.policy.reason"), Some(REASON_DENIED));
        assert_eq!(event.field("network.policy.override"), Some("false"));
        assert_eq!(event.field("http.request.method"), Some("GET"));
        assert_eq!(event.field("client.address"), Some("127.0.0.1:1234"));
        assert_eq!(event.field("execution.id"), Some("execution-baseline-deny"));
        assert_ne!(event.field("execution.id"), Some("token-baseline-deny"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_host_policy_emits_domain_event_for_decider_ask() {
        let state = network_proxy_state_for_policy(NetworkProxyConfig::default());
        let decider: Arc<dyn NetworkPolicyDecider> =
            Arc::new(|_req| async { NetworkDecision::ask(REASON_NOT_ALLOWED) });
        let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
            protocol: NetworkProtocol::Http,
            host: "example.com".to_string(),
            port: 80,
            environment_id: None,
            client_addr: None,
            method: Some("GET".to_string()),
            command: None,
            exec_policy_hint: None,
        });

        let (decision, events) = capture_events(|| async {
            evaluate_host_policy(&state, Some(&decider), &request)
                .await
                .unwrap()
        })
        .await;
        assert_eq!(
            decision,
            NetworkDecision::Deny {
                reason: REASON_NOT_ALLOWED.to_string(),
                source: NetworkDecisionSource::Decider,
                decision: NetworkPolicyDecision::Ask,
            }
        );

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision audit event");
        assert_eq!(event.field("network.policy.decision"), Some("ask"));
        assert_eq!(event.field("network.policy.source"), Some("decider"));
        assert_eq!(
            event.field("network.policy.reason"),
            Some(REASON_NOT_ALLOWED)
        );
        assert_eq!(event.field("network.policy.override"), Some("false"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_host_policy_emits_metadata_fields() {
        let metadata = NetworkProxyAuditMetadata {
            conversation_id: Some("conversation-1".to_string()),
            app_version: Some("1.2.3".to_string()),
            user_account_id: Some("acct-1".to_string()),
            auth_mode: Some("Chatgpt".to_string()),
            originator: Some("codex_cli_rs".to_string()),
            user_email: Some("test@example.com".to_string()),
            terminal_type: Some("iTerm.app/3.6.5".to_string()),
            model: Some("gpt-5.3-codex".to_string()),
            slug: Some("gpt-5.3-codex".to_string()),
        };
        let mut state = state_with_metadata(metadata);
        let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
            protocol: NetworkProtocol::Http,
            host: "example.com".to_string(),
            port: 80,
            environment_id: None,
            client_addr: None,
            method: Some("GET".to_string()),
            command: None,
            exec_policy_hint: None,
        });

        for (thread_id, expected_conversation_id) in [
            (None, "conversation-1"),
            (Some("process-thread-1"), "process-thread-1"),
            (None, "conversation-1"),
        ] {
            state.set_process_log_metadata(NetworkProxyProcessLogMetadata {
                thread_id: thread_id.map(str::to_string),
                tool_call_id: Some("call-1".to_string()),
                executor_identity: Some(ExecutorLogIdentity {
                    environment_id: "environment-1".to_string(),
                    registration_id: "registration-1".to_string(),
                }),
            });
            let (_decision, events) = capture_events(|| async {
                evaluate_host_policy(&state, /*decider*/ None, &request)
                    .await
                    .unwrap()
            })
            .await;

            let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
                .expect("expected policy decision audit event");
            assert_eq!(
                event.field("conversation.id"),
                Some(expected_conversation_id)
            );
            assert_eq!(event.field("tool.call_id"), Some("call-1"));
            assert_eq!(
                event.field("executor.environment_id"),
                Some("environment-1")
            );
            assert_eq!(
                event.field("executor.registration_id"),
                Some("registration-1")
            );
            assert_eq!(event.field("app.version"), Some("1.2.3"));
            assert_eq!(event.field("auth_mode"), Some("Chatgpt"));
            assert_eq!(event.field("originator"), Some("codex_cli_rs"));
            assert_eq!(event.field("user.account_id"), Some("acct-1"));
            assert_eq!(event.field("user.email"), Some("test@example.com"));
            assert_eq!(event.field("terminal.type"), Some("iTerm.app/3.6.5"));
            assert_eq!(event.field("model"), Some("gpt-5.3-codex"));
            assert_eq!(event.field("slug"), Some("gpt-5.3-codex"));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emit_block_decision_audit_event_emits_non_domain_event() {
        let state = network_proxy_state_for_policy(NetworkProxyConfig::default());

        let (_, events) = capture_events(|| async {
            emit_block_decision_audit_event(
                &state,
                BlockDecisionAuditEventArgs {
                    source: NetworkDecisionSource::ModeGuard,
                    reason: REASON_METHOD_NOT_ALLOWED,
                    protocol: NetworkProtocol::Http,
                    server_address: "unix-socket",
                    server_port: 0,
                    method: Some("POST"),
                    client_addr: None,
                },
            );
        })
        .await;

        let event = find_event_by_name(&events, POLICY_DECISION_EVENT_NAME)
            .expect("expected policy decision audit event");
        assert_eq!(event.target, AUDIT_TARGET);
        assert_eq!(
            event.field("network.policy.scope"),
            Some(POLICY_SCOPE_NON_DOMAIN)
        );
        assert_eq!(
            event.field("network.policy.decision"),
            Some(POLICY_DECISION_DENY)
        );
        assert_eq!(event.field("network.policy.source"), Some("mode_guard"));
        assert_eq!(
            event.field("network.policy.reason"),
            Some(REASON_METHOD_NOT_ALLOWED)
        );
        assert_eq!(event.field("network.transport.protocol"), Some("http"));
        assert_eq!(event.field("server.address"), Some("unix-socket"));
        assert_eq!(event.field("server.port"), Some("0"));
        assert_eq!(event.field("http.request.method"), Some("POST"));
        assert_eq!(event.field("client.address"), Some(DEFAULT_CLIENT_ADDRESS));
        assert_eq!(event.field("network.policy.override"), Some("false"));
        assert_eq!(
            find_event_by_name(&events, LEGACY_BLOCK_DECISION_EVENT_NAME),
            None
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn evaluate_host_policy_still_denies_not_allowed_local_without_decider_override() {
        let state = network_proxy_state_for_policy({
            let mut network = NetworkProxyConfig::default();
            network.set_allowed_domains(vec!["example.com".to_string()]);
            network.allow_local_binding = Some(false);
            network
        });
        let request = NetworkPolicyRequest::new(NetworkPolicyRequestArgs {
            protocol: NetworkProtocol::Http,
            host: "127.0.0.1".to_string(),
            port: 80,
            environment_id: None,
            client_addr: None,
            method: Some("GET".to_string()),
            command: None,
            exec_policy_hint: None,
        });

        let decision = evaluate_host_policy(&state, /*decider*/ None, &request)
            .await
            .unwrap();
        assert_eq!(
            decision,
            NetworkDecision::Deny {
                reason: REASON_NOT_ALLOWED_LOCAL.to_string(),
                source: NetworkDecisionSource::BaselinePolicy,
                decision: NetworkPolicyDecision::Deny,
            }
        );
    }

    #[test]
    fn ask_uses_decider_source_and_ask_decision() {
        assert_eq!(
            NetworkDecision::ask(REASON_NOT_ALLOWED),
            NetworkDecision::Deny {
                reason: REASON_NOT_ALLOWED.to_string(),
                source: NetworkDecisionSource::Decider,
                decision: NetworkPolicyDecision::Ask,
            }
        );
    }
}
