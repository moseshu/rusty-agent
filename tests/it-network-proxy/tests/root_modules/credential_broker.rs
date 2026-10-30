// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#[cfg(test)]
impl CredentialBroker {
    pub(crate) fn discover_parent_credentials(
        &self,
        parent_env: &HashMap<String, String>,
        child_env: &HashMap<String, String>,
    ) {
        self.discover_parent_credentials_for_environment(
            parent_env, child_env, /*environment_id*/ None,
        );
    }
    pub(crate) fn virtualize_child_env(&self, env: &mut HashMap<String, String>) {
        self.virtualize_child_env_for_environment(env, /*environment_id*/ None);
    }
    pub(crate) fn host_requires_mitm(&self, host: &str, port: u16) -> bool {
        self.host_protocols_for_environment(host, port, /*environment_id*/ None)
            .tls
    }
    pub(crate) fn inject_request_headers(&self, destination: &str, headers: &mut HeaderMap) {
        self.inject_request_headers_for_environment(
            destination,
            headers,
            /*environment_id*/ None,
        );
    }
}
#[cfg(test)]
#[path = "../upstream/credential_broker_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "../upstream/credential_broker/configured_tests.rs"]
mod configured_tests;
