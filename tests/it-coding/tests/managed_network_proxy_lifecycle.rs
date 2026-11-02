// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Codex's session cases for starting, refreshing and amending the managed network proxy
//! (`core/src/session/tests.rs`), against the thread's proxy slot.
//!
//! Codex drives them through a whole session; here the slot is exercised directly: `start` is
//! the session initialization, `refresh` the permission-profile change, `started_proxy` the
//! session service and `for_permission_profile` a turn's network. The cases that need Codex's
//! network policy decider, its orchestrator attempts, its user shell or its whole configuration
//! rebuild are recorded with the slices that port those consumers.

use ra_coding::config::network::{NetworkProxyConfig, NetworkProxySpec};
use ra_coding::exec_policy::ExecPolicyManager;
use ra_coding::thread_settings::network_proxy_test_api::{self, ThreadNetworkProxyProbe};
use ra_config::{NetworkConstraints, NetworkDomainPermissionToml, NetworkDomainPermissionsToml};
use ra_execpolicy::{Decision, NetworkRuleProtocol, Policy};
use ra_filesystem::models::PermissionProfile;
use ra_protocol::server::protocol::{
    NetworkApprovalContext, NetworkApprovalProtocol, NetworkPolicyAmendment,
    NetworkPolicyRuleAction,
};

#[tokio::test]
async fn start_managed_network_proxy_applies_execpolicy_network_rules() -> anyhow::Result<()> {
    let permission_profile = PermissionProfile::workspace_write();
    let spec = NetworkProxySpec::from_config_and_constraints(
        NetworkProxyConfig {
            enabled: true,
            ..NetworkProxyConfig::default()
        },
        /*requirements*/ None,
        &permission_profile,
    )?;
    let mut exec_policy = Policy::empty();
    exec_policy.add_network_rule(
        "example.com",
        NetworkRuleProtocol::Https,
        Decision::Allow,
        /*justification*/ None,
    )?;

    let proxy = ThreadNetworkProxyProbe::default();
    proxy
        .start(Some(&spec), &exec_policy, &permission_profile, false)
        .await?;

    let current_cfg = proxy.started_proxy().unwrap().current_cfg().await?;
    assert_eq!(
        current_cfg.allowed_domains(),
        Some(vec!["example.com".to_string()])
    );
    Ok(())
}

#[tokio::test]
async fn start_managed_network_proxy_ignores_invalid_execpolicy_network_rules() -> anyhow::Result<()>
{
    let permission_profile = PermissionProfile::workspace_write();
    let spec = NetworkProxySpec::from_config_and_constraints(
        NetworkProxyConfig {
            enabled: true,
            ..NetworkProxyConfig::default()
        },
        Some(NetworkConstraints {
            domains: Some(NetworkDomainPermissionsToml {
                entries: std::collections::BTreeMap::from([(
                    "managed.example.com".to_string(),
                    NetworkDomainPermissionToml::Allow,
                )]),
            }),
            managed_allowed_domains_only: Some(true),
            ..Default::default()
        }),
        &permission_profile,
    )?;
    let mut exec_policy = Policy::empty();
    exec_policy.add_network_rule(
        "example.com",
        NetworkRuleProtocol::Https,
        Decision::Allow,
        /*justification*/ None,
    )?;

    let proxy = ThreadNetworkProxyProbe::default();
    proxy
        .start(Some(&spec), &exec_policy, &permission_profile, false)
        .await?;

    let current_cfg = proxy.started_proxy().unwrap().current_cfg().await?;
    assert_eq!(
        current_cfg.allowed_domains(),
        Some(vec!["managed.example.com".to_string()])
    );
    Ok(())
}

#[tokio::test]
async fn new_turn_refreshes_managed_network_proxy_for_sandbox_change() -> anyhow::Result<()> {
    let initial_permission_profile = PermissionProfile::workspace_write();

    let mut network_config = NetworkProxyConfig::default();
    network_config.set_allowed_domains(vec!["evil.com".to_string()]);
    let requirements = NetworkConstraints {
        enabled: Some(true),
        domains: Some(NetworkDomainPermissionsToml {
            entries: std::collections::BTreeMap::from([(
                "*.example.com".to_string(),
                NetworkDomainPermissionToml::Allow,
            )]),
        }),
        ..Default::default()
    };
    let spec = NetworkProxySpec::from_config_and_constraints(
        network_config,
        Some(requirements),
        &initial_permission_profile,
    )?;
    let proxy = ThreadNetworkProxyProbe::default();
    proxy
        .start(
            Some(&spec),
            &Policy::empty(),
            &initial_permission_profile,
            false,
        )
        .await?;
    assert_eq!(
        proxy
            .started_proxy()
            .unwrap()
            .current_cfg()
            .await?
            .allowed_domains(),
        Some(vec!["*.example.com".to_string(), "evil.com".to_string()])
    );

    // Codex's new turn with `sandbox_policy: DangerFullAccess` commits a full-access profile and
    // refreshes the proxy for it.
    proxy
        .refresh(Some(&spec), &Policy::empty(), &PermissionProfile::Disabled)
        .await;

    let started_proxy = proxy
        .started_proxy()
        .expect("managed network proxy should be present");
    assert_eq!(
        started_proxy.current_cfg().await?.allowed_domains(),
        Some(vec!["*.example.com".to_string()])
    );
    Ok(())
}

#[tokio::test]
async fn refresh_clears_disabled_managed_network_proxy() -> anyhow::Result<()> {
    let permission_profile = PermissionProfile::workspace_write();
    let enabled_spec = NetworkProxySpec::from_config_and_constraints(
        NetworkProxyConfig::default(),
        Some(NetworkConstraints {
            enabled: Some(true),
            ..Default::default()
        }),
        &permission_profile,
    )?;
    let disabled_spec = NetworkProxySpec::from_config_and_constraints(
        NetworkProxyConfig::default(),
        Some(NetworkConstraints {
            enabled: Some(false),
            ..Default::default()
        }),
        &permission_profile,
    )?;
    let proxy = ThreadNetworkProxyProbe::default();
    proxy
        .start(
            Some(&enabled_spec),
            &Policy::empty(),
            &permission_profile,
            false,
        )
        .await?;
    assert!(proxy.started_proxy().is_some());

    proxy
        .refresh(Some(&disabled_spec), &Policy::empty(), &permission_profile)
        .await;

    assert!(proxy.started_proxy().is_none());
    assert!(proxy.for_permission_profile(&permission_profile).is_none());
    Ok(())
}

#[tokio::test]
async fn danger_full_access_turns_do_not_expose_managed_network_proxy() -> anyhow::Result<()> {
    let network_spec = NetworkProxySpec::from_config_and_constraints(
        NetworkProxyConfig::default(),
        Some(NetworkConstraints {
            enabled: Some(true),
            ..Default::default()
        }),
        &PermissionProfile::Disabled,
    )?;

    let proxy = ThreadNetworkProxyProbe::default();
    proxy
        .start(
            Some(&network_spec),
            &Policy::empty(),
            &PermissionProfile::Disabled,
            false,
        )
        .await?;

    assert!(
        proxy
            .for_permission_profile(&PermissionProfile::Disabled)
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn workspace_write_turns_continue_to_expose_managed_network_proxy() -> anyhow::Result<()> {
    let permission_profile = PermissionProfile::workspace_write();
    let network_spec = NetworkProxySpec::from_config_and_constraints(
        NetworkProxyConfig::default(),
        Some(NetworkConstraints {
            enabled: Some(true),
            ..Default::default()
        }),
        &permission_profile,
    )?;

    let proxy = ThreadNetworkProxyProbe::default();
    proxy
        .start(
            Some(&network_spec),
            &Policy::empty(),
            &permission_profile,
            false,
        )
        .await?;

    assert!(proxy.for_permission_profile(&permission_profile).is_some());
    Ok(())
}

#[tokio::test]
async fn disabled_managed_network_does_not_start_or_expose_proxy() -> anyhow::Result<()> {
    let permission_profile = PermissionProfile::workspace_write();
    let network_spec = NetworkProxySpec::from_config_and_constraints(
        NetworkProxyConfig::default(),
        Some(NetworkConstraints {
            enabled: Some(false),
            ..Default::default()
        }),
        &permission_profile,
    )?;

    let proxy = ThreadNetworkProxyProbe::default();
    proxy
        .start(
            Some(&network_spec),
            &Policy::empty(),
            &permission_profile,
            false,
        )
        .await?;

    assert!(proxy.started_proxy().is_none());
    assert!(proxy.for_permission_profile(&permission_profile).is_none());
    Ok(())
}

#[test]
fn validated_network_policy_amendment_host_allows_normalized_match() {
    let amendment = NetworkPolicyAmendment::new("ExAmPlE.Com.:443", NetworkPolicyRuleAction::Allow);
    let context = NetworkApprovalContext::new("example.com", NetworkApprovalProtocol::Https);

    let host =
        network_proxy_test_api::validated_network_policy_amendment_host(&amendment, &context)
            .expect("normalized hosts should match");

    assert_eq!(host, "example.com");
}

#[test]
fn validated_network_policy_amendment_host_rejects_mismatch() {
    let amendment = NetworkPolicyAmendment::new("evil.example.com", NetworkPolicyRuleAction::Deny);
    let context = NetworkApprovalContext::new("api.example.com", NetworkApprovalProtocol::Https);

    let err = network_proxy_test_api::validated_network_policy_amendment_host(&amendment, &context)
        .expect_err("mismatched hosts should be rejected");

    let message = err.to_string();
    assert!(message.contains("does not match approved host"));
}

/// Not a Codex case: an approved amendment reaches the running proxy before it is saved to the
/// rules file and the policy in force, as Codex's `persist_network_policy_amendment` orders them.
#[tokio::test]
async fn persisted_network_policy_amendment_updates_proxy_and_rules() -> anyhow::Result<()> {
    let permission_profile = PermissionProfile::workspace_write();
    let spec = NetworkProxySpec::from_config_and_constraints(
        NetworkProxyConfig {
            enabled: true,
            ..NetworkProxyConfig::default()
        },
        /*requirements*/ None,
        &permission_profile,
    )?;
    let proxy = ThreadNetworkProxyProbe::default();
    proxy
        .start(Some(&spec), &Policy::empty(), &permission_profile, false)
        .await?;
    let rules_home = tempfile::tempdir()?;
    let exec_policy = ExecPolicyManager::default();
    let applied = std::sync::atomic::AtomicUsize::new(0);

    proxy
        .persist_network_policy_amendment(
            &NetworkPolicyAmendment::new("Example.COM", NetworkPolicyRuleAction::Allow),
            &NetworkApprovalContext::new("example.com", NetworkApprovalProtocol::Https),
            &exec_policy,
            Some(rules_home.path()),
            || {
                applied.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            },
        )
        .await?;

    assert_eq!(applied.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(
        proxy
            .started_proxy()
            .unwrap()
            .current_cfg()
            .await?
            .allowed_domains(),
        Some(vec!["example.com".to_string()])
    );
    assert_eq!(
        exec_policy.current().compiled_network_domains(),
        (vec!["example.com".to_string()], Vec::new())
    );
    let rules = std::fs::read_to_string(ra_coding::exec_policy::default_policy_path(
        rules_home.path(),
    ))?;
    assert!(
        rules.contains("Allow https_connect access to example.com"),
        "{rules}"
    );
    Ok(())
}
