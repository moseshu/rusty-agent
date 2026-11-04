// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! The thread's managed network proxy: Codex's session `network_proxy` service, started when the
//! session is built and refreshed when its permission profile changes.
//!
//! # Differences from Codex
//!
//! - The thread's proxy is also Codex's network approval session: it reads the running turn and
//!   the settings of its next step through the way to ask the client, keeps the exec policy
//!   amendments are saved to, and builds the policy decider and blocked-request observer the
//!   proxy starts with.
//! - A thread without a rules home still applies an approved rule to the running proxy and
//!   reports only its saving as failed; Codex's session always has a home to save to.
//! - An automatic reviewer is refused rather than routed, as for command approvals: Codex's
//!   reviewer, Guardian, is not ported.
//! - Routing and local binding are always `SharedIngress` and `DefaultFalse`: Codex only changes
//!   them for its Windows MXC sandbox, which is not ported.
//! - The snapshot credential broker state and the session's reported listener addresses are not
//!   kept; the credential broker and the shell snapshot consume them separately.

use crate::config::network::{NetworkProxySpec, StartedNetworkProxy};
use crate::exec_policy::ExecPolicyManager;
use crate::hooks::{PermissionRequestDecision, ProductHookRuntime};
use crate::network_approval::{
    ActiveNetworkApproval, CommandNetworkApproval, NetworkApprovalCallSpec, NetworkApprovalError,
    NetworkApprovalRequest, NetworkApprovalService, NetworkApprovalSession, NetworkApprovalTurn,
    begin_network_approval, build_blocked_request_observer, build_network_policy_decider,
};
use crate::network_policy_decision::execpolicy_network_rule_amendment;
use ra_core::item::Message;
use ra_execpolicy::Policy;
use ra_filesystem::models::PermissionProfile;
use ra_network_proxy::{
    BlockedRequestObserver, LocalBindingPolicy, ManagedProxyRouting, NetworkPolicyDecider,
    NetworkProxy, NetworkProxyAuditMetadata, normalize_host,
};
use ra_protocol::server::protocol::{
    CommandExecutionApprovalDecision, CommandExecutionRequestApprovalParams,
    NetworkApprovalContext, NetworkPolicyAmendment, NetworkPolicyRuleAction,
};
use ra_protocol::server::{ReviewDecision, ThreadServerRequests};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use tokio_util::sync::CancellationToken;
use tracing::warn;

/// The proxy a thread's commands are routed through, shared by every settings snapshot of the
/// thread.
#[derive(Default)]
pub(crate) struct ThreadNetworkProxy {
    started: Mutex<Option<Arc<StartedNetworkProxy>>>,
    refresh_lock: tokio::sync::Mutex<()>,
    managed_network_requirements_configured: std::sync::atomic::AtomicBool,
    /// Codex's session `network_approval` service.
    approval: Arc<NetworkApprovalService>,
    /// How the thread asks its client, once the app server has handed it over.
    requests: OnceLock<ThreadServerRequests>,
    /// The exec policy and rules home amendments are saved to.
    policy: Mutex<Option<(Arc<ExecPolicyManager>, Option<PathBuf>)>>,
    /// The thread's product hooks, which decide a network access before its client is asked.
    hooks: OnceLock<Arc<ProductHookRuntime>>,
}

/// What a command's attempt runs under when the thread's managed proxy routes it: the
/// execution-scoped proxy, the token a reviewed denial cancels, and the approval to finish once
/// the command has ended.
pub(crate) struct CommandNetwork {
    pub(crate) proxy: NetworkProxy,
    pub(crate) cancellation: CancellationToken,
    pub(crate) approval: Arc<CommandNetworkApproval>,
}

impl std::fmt::Debug for ThreadNetworkProxy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadNetworkProxy")
            .field("started", &self.started().map(|started| started.proxy()))
            .finish_non_exhaustive()
    }
}

impl ThreadNetworkProxy {
    fn started(&self) -> Option<Arc<StartedNetworkProxy>> {
        self.started
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn store(&self, started: Option<StartedNetworkProxy>) {
        *self.started.lock().unwrap_or_else(PoisonError::into_inner) = started.map(Arc::new);
    }

    /// Codex's session initialization: starts the proxy when the configuration enables one, with
    /// the blocked-request observer, and with the policy decider when network requirements are
    /// configured.
    pub(crate) async fn start(
        self: &Arc<Self>,
        spec: Option<&NetworkProxySpec>,
        exec_policy: &Policy,
        permission_profile: &PermissionProfile,
        managed_network_requirements_configured: bool,
    ) -> std::io::Result<()> {
        self.managed_network_requirements_configured.store(
            managed_network_requirements_configured,
            std::sync::atomic::Ordering::Relaxed,
        );
        let Some(spec) = spec.filter(|spec| spec.enabled()) else {
            return Ok(());
        };
        // The managed proxy can call back into the thread for allowlist-miss decisions.
        let network_policy_decider = managed_network_requirements_configured.then(|| {
            let session: Arc<dyn NetworkApprovalSession> = Arc::clone(self) as _;
            build_network_policy_decider(Arc::clone(&self.approval), Arc::downgrade(&session))
        });
        let started = start_managed_network_proxy(
            spec,
            exec_policy,
            permission_profile,
            network_policy_decider,
            Some(build_blocked_request_observer(Arc::clone(&self.approval))),
            managed_network_requirements_configured,
            NetworkProxyAuditMetadata::default(),
        )
        .await?;
        self.store(Some(started));
        Ok(())
    }

    /// Codex's `refresh_managed_network_proxy_for_current_permission_profile`.
    pub(crate) async fn refresh(
        &self,
        spec: Option<&NetworkProxySpec>,
        exec_policy: &Policy,
        permission_profile: &PermissionProfile,
    ) {
        let _refresh_guard = self.refresh_lock.lock().await;
        let Some(spec) = spec.cloned() else {
            self.store(None);
            return;
        };

        let spec = match spec.recompute_for_permission_profile(permission_profile) {
            Ok(spec) => spec,
            Err(err) => {
                warn!("failed to rebuild managed network proxy policy for sandbox change: {err}");
                return;
            }
        };
        let spec = match spec.with_exec_policy_network_rules(exec_policy) {
            Ok(spec) => spec,
            Err(err) => {
                warn!(
                    "failed to apply execpolicy network rules while refreshing managed network proxy: {err}"
                );
                spec
            }
        };
        // Disabled specs still carry managed requirements and constraints, but they do not have
        // listeners and must not be exposed as active managed proxy runtimes.
        if !spec.enabled() {
            self.store(None);
            return;
        }
        let managed_proxy_routing = ManagedProxyRouting::SharedIngress;
        let started_proxy = self.started();
        let network_policy_decider = started_proxy
            .as_ref()
            .and_then(|started_proxy| started_proxy.network_policy_decider());
        if let Some(started_proxy) = started_proxy
            && started_proxy.proxy().managed_proxy_routing() == managed_proxy_routing
        {
            if let Err(err) = spec.apply_to_started_proxy(started_proxy.as_ref()).await {
                warn!("failed to refresh managed network proxy for sandbox change: {err}");
            }
            return;
        }

        match start_managed_network_proxy(
            &spec,
            exec_policy,
            permission_profile,
            network_policy_decider,
            Some(build_blocked_request_observer(Arc::clone(&self.approval))),
            self.managed_network_requirements_configured
                .load(std::sync::atomic::Ordering::Relaxed),
            NetworkProxyAuditMetadata::default(),
        )
        .await
        {
            Ok(started_proxy) => self.store(Some(started_proxy)),
            Err(err) => {
                warn!("failed to start managed network proxy for sandbox change: {err}");
            }
        }
    }

    /// Codex's `persist_network_policy_amendment`: applies an approved host rule to the running
    /// proxy, then saves it to the exec policy rules under `rules_home`. `on_policy_applied` runs
    /// once the first effective policy change has been made.
    pub(crate) async fn persist_network_policy_amendment(
        &self,
        amendment: &NetworkPolicyAmendment,
        network_approval_context: &NetworkApprovalContext,
        exec_policy: &ExecPolicyManager,
        rules_home: Option<&Path>,
        on_policy_applied: impl FnOnce() + Send,
    ) -> anyhow::Result<()> {
        let _refresh_guard = self.refresh_lock.lock().await;
        let host = validated_network_policy_amendment_host(amendment, network_approval_context)?;
        let execpolicy_amendment =
            execpolicy_network_rule_amendment(amendment, network_approval_context, &host);
        let mut on_policy_applied = Some(on_policy_applied);

        if let Some(started_network_proxy) = self.started() {
            let proxy = started_network_proxy.proxy();
            match amendment.action() {
                NetworkPolicyRuleAction::Allow => proxy
                    .add_allowed_domain(&host)
                    .await
                    .map_err(|err| anyhow::anyhow!("failed to update runtime allowlist: {err}"))?,
                NetworkPolicyRuleAction::Deny => proxy
                    .add_denied_domain(&host)
                    .await
                    .map_err(|err| anyhow::anyhow!("failed to update runtime denylist: {err}"))?,
                action => anyhow::bail!("unsupported network policy rule action {action:?}"),
            }
            // Active enforcement changed successfully. Notify the owner before
            // the next fallible await so cancellation cannot contradict it.
            if let Some(on_policy_applied) = on_policy_applied.take() {
                on_policy_applied();
            }
        }

        // Codex's session always has a home to save to. Without one, the rule above still applies
        // to the running proxy and only saving it fails.
        let codex_home = rules_home
            .ok_or_else(|| anyhow::anyhow!("no rules home is configured for this thread"))?;
        exec_policy
            .append_network_rule_and_update(
                codex_home,
                &host,
                execpolicy_amendment.protocol,
                execpolicy_amendment.decision,
                Some(execpolicy_amendment.justification),
            )
            .await
            .map_err(|err| {
                anyhow::anyhow!("failed to persist network policy amendment to execpolicy: {err}")
            })?;

        // Without a running proxy, persistence is the first effective policy change.
        if let Some(on_policy_applied) = on_policy_applied {
            on_policy_applied();
        }

        Ok(())
    }

    /// Keeps the way the app server gave the thread to ask its client.
    pub(crate) fn attach_requests(&self, requests: ThreadServerRequests) {
        let _ = self.requests.set(requests);
    }

    /// Keeps the thread's product hooks; see `hooks`.
    pub(crate) fn attach_hooks(&self, hooks: Arc<ProductHookRuntime>) {
        let _ = self.hooks.set(hooks);
    }

    /// Keeps the exec policy and rules home network amendments are saved to.
    pub(crate) fn bind_exec_policy(
        &self,
        exec_policy: Arc<ExecPolicyManager>,
        rules_home: Option<PathBuf>,
    ) {
        *self.policy.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((exec_policy, rules_home));
    }

    /// The running turn as a network review sees it, and whether its next step names the
    /// automatic reviewer: Codex's `active_turn_context_and_strict_auto_review`, read when the
    /// review happens so a mid-turn settings change applies.
    fn running_turn(&self) -> Option<(NetworkApprovalTurn, bool)> {
        let (turn_id, config) = self.requests.get()?.running_turn()?;
        let settings = config
            .app_context()?
            .downcast_ref::<super::CodingThreadSettings>()?;
        let model = self.requests.get()?.resolved_model_name(&config);
        super::network_approval_turn(turn_id, model, settings)
    }

    /// Codex's `begin_network_approval` for one command under `permission_profile`: `None` when
    /// the turn exposes no managed proxy.
    pub(crate) fn begin_command(
        &self,
        permission_profile: &PermissionProfile,
        spec: NetworkApprovalCallSpec,
    ) -> Option<anyhow::Result<CommandNetwork>> {
        let network = self.for_permission_profile(permission_profile)?;
        Some(
            begin_network_approval(&self.approval, &network, spec).and_then(
                |active: ActiveNetworkApproval| {
                    let cancellation = active.cancellation_token();
                    let proxy = active.execution_proxy().clone();
                    let deferred = active.into_deferred().ok_or_else(|| {
                        anyhow::anyhow!("the network approval has no registration")
                    })?;
                    Ok(CommandNetwork {
                        proxy,
                        cancellation,
                        approval: Arc::new(CommandNetworkApproval::new(
                            Arc::clone(&self.approval),
                            deferred,
                        )),
                    })
                },
            ),
        )
    }

    /// The proxy a turn under `permission_profile` exposes: Codex's turn context network.
    pub(crate) fn for_permission_profile(
        &self,
        permission_profile: &PermissionProfile,
    ) -> Option<NetworkProxy> {
        self.started()
            .filter(|_| managed_network_proxy_active_for_permission_profile(permission_profile))
            .map(|started_proxy| started_proxy.proxy())
    }
}

#[async_trait::async_trait]
impl NetworkApprovalSession for ThreadNetworkProxy {
    fn active_turn(&self) -> Option<NetworkApprovalTurn> {
        self.running_turn().map(|(turn, _)| turn)
    }

    /// Codex's `request_approval` for a network access, routed to the client as its
    /// `request_command_approval` sends it: the host's allow and deny rules proposed, and the
    /// default network decisions offered.
    async fn request_network_approval(
        &self,
        request: NetworkApprovalRequest,
    ) -> Result<ReviewDecision, NetworkApprovalError> {
        // Codex's approval precedence is hooks first, then the reviewer.
        if let Some(hooks) = self.hooks.get() {
            match hooks
                .run_permission_request_hooks_with_settings(
                    &request.turn_id,
                    &request.hook_run_id,
                    &request.hook_command,
                    &format!("network-access {}", request.target),
                    request.model.as_deref(),
                    request.approval_policy,
                )
                .await
            {
                Some(PermissionRequestDecision::Allow) => return Ok(ReviewDecision::Approved),
                Some(PermissionRequestDecision::Deny { message }) => {
                    return Err(NetworkApprovalError::Rejected(message));
                }
                None => {}
            }
        }
        let auto_review = self
            .running_turn()
            .is_some_and(|(_, auto_review)| auto_review);
        if auto_review {
            return Err(NetworkApprovalError::Rejected(
                "automatic approval review is not available".to_string(),
            ));
        }
        let Some(requests) = self.requests.get() else {
            return Err(NetworkApprovalError::TurnAborted);
        };
        let host = request.network_approval_context.host().to_string();
        let proposed = vec![
            NetworkPolicyAmendment::new(host.clone(), NetworkPolicyRuleAction::Allow),
            NetworkPolicyAmendment::new(host, NetworkPolicyRuleAction::Deny),
        ];
        let available = CommandExecutionApprovalDecision::default_available(
            Some(&request.network_approval_context),
            /*proposed_execpolicy_amendment*/ None,
            Some(&proposed),
            /*additional_permissions*/ None,
        );
        let started_at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| {
                i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
            });
        let params = CommandExecutionRequestApprovalParams::new(
            requests.thread_id(),
            request.turn_id,
            request.item_id,
            started_at_ms,
        )
        .with_environment_id(request.environment_id)
        .with_reason(request.reason)
        .with_network_approval_context(request.network_approval_context)
        .with_proposed_network_policy_amendments(proposed)
        .with_available_decisions(available);
        // Codex's session reads a lost answer as `Abort`.
        let decision = requests
            .request_network_approval(params)
            .await
            .unwrap_or(ReviewDecision::Abort);
        // Codex's `ApprovalResolution::into_tool_result`, with a denying amendment returned as
        // the decision for network approvals.
        match decision {
            ReviewDecision::NetworkPolicyAmendment {
                network_policy_amendment,
            } if network_policy_amendment.action() == NetworkPolicyRuleAction::Deny => {
                Ok(ReviewDecision::NetworkPolicyAmendment {
                    network_policy_amendment,
                })
            }
            ReviewDecision::ApprovedMcpPolicyAmendment => Err(NetworkApprovalError::Rejected(
                "Error while requesting approval".to_string(),
            )),
            ReviewDecision::Denied { rejection } => Err(NetworkApprovalError::Rejected(rejection)),
            ReviewDecision::TimedOut => Err(NetworkApprovalError::Rejected(
                "approval review timed out".to_string(),
            )),
            ReviewDecision::Abort => Err(NetworkApprovalError::TurnAborted),
            decision => Ok(decision),
        }
    }

    async fn persist_network_policy_amendment<'a>(
        &'a self,
        amendment: &'a NetworkPolicyAmendment,
        network_approval_context: &'a NetworkApprovalContext,
        on_policy_applied: Box<dyn FnOnce() + Send + 'a>,
    ) -> anyhow::Result<()> {
        let Some((exec_policy, rules_home)) = self
            .policy
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
        else {
            anyhow::bail!("no exec policy is bound to this thread");
        };
        ThreadNetworkProxy::persist_network_policy_amendment(
            self,
            amendment,
            network_approval_context,
            &exec_policy,
            rules_home.as_deref(),
            on_policy_applied,
        )
        .await
    }

    /// Codex's `record_network_policy_amendment_message`: the notice goes to the running turn,
    /// or into the thread's history when none can take it, as `inject_no_new_turn` does.
    async fn record_network_policy_amendment_message(
        &self,
        _turn_id: &str,
        amendment: &NetworkPolicyAmendment,
    ) {
        if let Some(requests) = self.requests.get() {
            requests
                .inject_context(vec![network_rule_saved_message(amendment)])
                .await;
        }
    }

    async fn send_warning(&self, _turn_id: &str, message: String) {
        if let Some(requests) = self.requests.get() {
            requests.send_warning(message).await;
        }
    }
}

/// Codex's `NetworkRuleSaved` fragment: the developer message that tells the model a rule was
/// saved.
pub(crate) fn network_rule_saved_message(amendment: &NetworkPolicyAmendment) -> Message {
    let (action, list_name) = match amendment.action() {
        NetworkPolicyRuleAction::Deny => ("Denied", "denylist"),
        _ => ("Allowed", "allowlist"),
    };
    Message::developer(format!(
        "{action} network rule saved in execpolicy ({list_name}): {}",
        amendment.host()
    ))
}

/// Codex's `Session::validated_network_policy_amendment_host`.
pub(crate) fn validated_network_policy_amendment_host(
    amendment: &NetworkPolicyAmendment,
    network_approval_context: &NetworkApprovalContext,
) -> anyhow::Result<String> {
    let approved_host = normalize_host(network_approval_context.host());
    let amendment_host = normalize_host(amendment.host());
    if amendment_host != approved_host {
        return Err(anyhow::anyhow!(
            "network policy amendment host '{}' does not match approved host '{}'",
            amendment.host(),
            network_approval_context.host()
        ));
    }
    Ok(approved_host)
}

fn managed_network_proxy_active_for_permission_profile(
    permission_profile: &PermissionProfile,
) -> bool {
    !matches!(permission_profile, PermissionProfile::Disabled)
}

async fn start_managed_network_proxy(
    spec: &NetworkProxySpec,
    exec_policy: &Policy,
    permission_profile: &PermissionProfile,
    network_policy_decider: Option<Arc<dyn NetworkPolicyDecider>>,
    blocked_request_observer: Option<Arc<dyn BlockedRequestObserver>>,
    managed_network_requirements_enabled: bool,
    audit_metadata: NetworkProxyAuditMetadata,
) -> std::io::Result<StartedNetworkProxy> {
    let spec = spec
        .with_exec_policy_network_rules(exec_policy)
        .map_err(|err| {
            warn!(
                "failed to apply execpolicy network rules to managed proxy; continuing with configured network policy: {err}"
            );
            err
        })
        .unwrap_or_else(|_| spec.clone());
    spec.start_proxy(
        permission_profile,
        ManagedProxyRouting::SharedIngress,
        LocalBindingPolicy::DefaultFalse,
        network_policy_decider,
        blocked_request_observer,
        managed_network_requirements_enabled,
        audit_metadata,
    )
    .await
    .map_err(|err| std::io::Error::other(format!("failed to start managed network proxy: {err}")))
}

/// Codex's session network-proxy entry points for the external integration-test workspace, which
/// ports the session cases that start and refresh the proxy.
#[cfg(feature = "test-api")]
pub mod test_api {
    #![allow(missing_docs, reason = "Hidden bridges for source tests.")]
    use super::{
        ExecPolicyManager, NetworkApprovalContext, NetworkPolicyAmendment, NetworkProxy,
        NetworkProxySpec, Path, PermissionProfile, Policy, ThreadNetworkProxy,
    };

    pub fn validated_network_policy_amendment_host(
        amendment: &NetworkPolicyAmendment,
        network_approval_context: &NetworkApprovalContext,
    ) -> anyhow::Result<String> {
        super::validated_network_policy_amendment_host(amendment, network_approval_context)
    }

    /// A thread's proxy slot, as a session holds it.
    #[derive(Default)]
    pub struct ThreadNetworkProxyProbe(std::sync::Arc<ThreadNetworkProxy>);

    impl ThreadNetworkProxyProbe {
        pub async fn start(
            &self,
            spec: Option<&NetworkProxySpec>,
            exec_policy: &Policy,
            permission_profile: &PermissionProfile,
            managed_network_requirements_configured: bool,
        ) -> std::io::Result<()> {
            self.0
                .start(
                    spec,
                    exec_policy,
                    permission_profile,
                    managed_network_requirements_configured,
                )
                .await
        }

        pub async fn refresh(
            &self,
            spec: Option<&NetworkProxySpec>,
            exec_policy: &Policy,
            permission_profile: &PermissionProfile,
        ) {
            self.0.refresh(spec, exec_policy, permission_profile).await;
        }

        pub async fn persist_network_policy_amendment(
            &self,
            amendment: &NetworkPolicyAmendment,
            network_approval_context: &NetworkApprovalContext,
            exec_policy: &ExecPolicyManager,
            rules_home: Option<&Path>,
            on_policy_applied: impl FnOnce() + Send,
        ) -> anyhow::Result<()> {
            self.0
                .persist_network_policy_amendment(
                    amendment,
                    network_approval_context,
                    exec_policy,
                    rules_home,
                    on_policy_applied,
                )
                .await
        }

        pub fn attach_hooks(&self, hooks: std::sync::Arc<crate::hooks::ProductHookRuntime>) {
            self.0.attach_hooks(hooks);
        }

        pub async fn request_network_approval(
            &self,
            request: crate::network_approval::NetworkApprovalRequest,
        ) -> Result<
            ra_protocol::server::ReviewDecision,
            crate::network_approval::NetworkApprovalError,
        > {
            use crate::network_approval::NetworkApprovalSession;
            self.0.request_network_approval(request).await
        }

        pub async fn record_network_policy_amendment_message(
            &self,
            turn_id: &str,
            amendment: &NetworkPolicyAmendment,
        ) {
            use crate::network_approval::NetworkApprovalSession;
            self.0
                .record_network_policy_amendment_message(turn_id, amendment)
                .await;
        }

        /// The started proxy, whatever the turn's profile: Codex's `services.network_proxy`.
        #[must_use]
        pub fn started_proxy(&self) -> Option<NetworkProxy> {
            self.0.started().map(|started| started.proxy())
        }

        /// The proxy a turn under `permission_profile` exposes: Codex's `turn_context.network`.
        #[must_use]
        pub fn for_permission_profile(
            &self,
            permission_profile: &PermissionProfile,
        ) -> Option<NetworkProxy> {
            self.0.for_permission_profile(permission_profile)
        }
    }
}
