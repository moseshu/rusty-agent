//! What the inline network approval handler tells the thread after the client answers: Codex's
//! `record_network_policy_amendment_message` once a rule is saved, and a warning event when it
//! could not be applied.

use std::sync::{Arc, Mutex};

use ra_coding::network_approval::*;
use ra_filesystem::models::PermissionProfile;
use ra_network_proxy::{NetworkDecision, NetworkPolicyRequest, NetworkProtocol};
use ra_paths::{AbsolutePathBuf, PathUri};
use ra_protocol::server::ReviewDecision;
use ra_protocol::server::protocol::{
    AskForApproval, NetworkApprovalContext, NetworkPolicyAmendment, NetworkPolicyRuleAction,
};

#[derive(Default)]
struct Notices {
    recorded: Mutex<Vec<(String, NetworkPolicyAmendment)>>,
    warnings: Mutex<Vec<(String, String)>>,
}

/// A session that answers every review with `decision` and saves amendments when `saves` holds.
struct AnsweringSession {
    decision: ReviewDecision,
    saves: bool,
    notices: Arc<Notices>,
}

#[async_trait::async_trait]
impl NetworkApprovalSession for AnsweringSession {
    fn active_turn(&self) -> Option<NetworkApprovalTurn> {
        let cwd = AbsolutePathBuf::from_absolute_path(std::env::temp_dir()).unwrap();
        Some(NetworkApprovalTurn {
            model: Some("test-model".to_owned()),
            turn_id: "active-turn".to_string(),
            environment_id: "local".to_string(),
            approval_policy: AskForApproval::OnRequest,
            permission_profile: PermissionProfile::workspace_write(),
            cwd: PathUri::from_abs_path(&cwd),
        })
    }

    async fn request_network_approval(
        &self,
        _request: NetworkApprovalRequest,
    ) -> Result<ReviewDecision, NetworkApprovalError> {
        Ok(self.decision.clone())
    }

    async fn persist_network_policy_amendment<'a>(
        &'a self,
        _amendment: &'a NetworkPolicyAmendment,
        _network_approval_context: &'a NetworkApprovalContext,
        on_policy_applied: Box<dyn FnOnce() + Send + 'a>,
    ) -> anyhow::Result<()> {
        if self.saves {
            on_policy_applied();
            Ok(())
        } else {
            anyhow::bail!("the rules file is read-only")
        }
    }

    async fn record_network_policy_amendment_message(
        &self,
        turn_id: &str,
        amendment: &NetworkPolicyAmendment,
    ) {
        self.notices
            .recorded
            .lock()
            .unwrap()
            .push((turn_id.to_owned(), amendment.clone()));
    }

    async fn send_warning(&self, turn_id: &str, message: String) {
        self.notices
            .warnings
            .lock()
            .unwrap()
            .push((turn_id.to_owned(), message));
    }
}

fn policy_request() -> NetworkPolicyRequest {
    NetworkPolicyRequest {
        protocol: NetworkProtocol::Http,
        host: "example.com".to_string(),
        port: 80,
        environment_id: Some("local".to_string()),
        execution_id: None,
        client_addr: None,
        method: None,
        command: None,
        exec_policy_hint: None,
        disconnect: None,
        cancellation: None,
    }
}

async fn decide(action: NetworkPolicyRuleAction, saves: bool) -> (NetworkDecision, Arc<Notices>) {
    let notices = Arc::new(Notices::default());
    let session: Arc<dyn NetworkApprovalSession> = Arc::new(AnsweringSession {
        decision: ReviewDecision::NetworkPolicyAmendment {
            network_policy_amendment: NetworkPolicyAmendment::new(
                "example.com".to_string(),
                action,
            ),
        },
        saves,
        notices: Arc::clone(&notices),
    });
    let decision = NetworkApprovalService::default()
        .handle_inline_policy_request(session, policy_request())
        .await;
    (decision, notices)
}

#[tokio::test]
async fn a_saved_allow_rule_is_recorded_for_the_turn() {
    let (decision, notices) = decide(NetworkPolicyRuleAction::Allow, true).await;

    assert_eq!(decision, NetworkDecision::Allow);
    let recorded = notices.recorded.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].0, "active-turn");
    assert_eq!(recorded[0].1.action(), NetworkPolicyRuleAction::Allow);
    assert_eq!(recorded[0].1.host(), "example.com");
    assert!(notices.warnings.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_saved_deny_rule_is_recorded_for_the_turn() {
    let (decision, notices) = decide(NetworkPolicyRuleAction::Deny, true).await;

    assert_eq!(decision, NetworkDecision::deny("not_allowed"));
    let recorded = notices.recorded.lock().unwrap();
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].1.action(), NetworkPolicyRuleAction::Deny);
    assert!(notices.warnings.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_rule_that_could_not_be_saved_is_a_warning_and_not_recorded() {
    for action in [
        NetworkPolicyRuleAction::Allow,
        NetworkPolicyRuleAction::Deny,
    ] {
        let (_, notices) = decide(action, false).await;

        assert!(notices.recorded.lock().unwrap().is_empty());
        let warnings = notices.warnings.lock().unwrap();
        assert_eq!(
            *warnings,
            vec![(
                "active-turn".to_string(),
                "Failed to apply network policy amendment: the rules file is read-only".to_string()
            )]
        );
    }
}
