// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::reasons::REASON_NOT_ALLOWED;
    use pretty_assertions::assert_eq;

    #[test]
    fn blocked_message_with_policy_returns_human_message() {
        let details = PolicyDecisionDetails {
            decision: NetworkPolicyDecision::Ask,
            reason: REASON_NOT_ALLOWED,
            source: NetworkDecisionSource::Decider,
            protocol: NetworkProtocol::HttpsConnect,
            host: "api.example.com",
            port: 443,
        };

        let message = blocked_message_with_policy(REASON_NOT_ALLOWED, &details);
        assert_eq!(message, "Domain not in allowlist.");
    }
}
