// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
use std::collections::HashMap;

pub(crate) mod environment;
pub use environment::CredentialBrokerContext;

fn env_key_matches(candidate: &str, expected: &str) -> bool {
    if cfg!(windows) {
        candidate.eq_ignore_ascii_case(expected)
    } else {
        candidate == expected
    }
}

fn env_entry<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<(&'a str, &'a str)> {
    env.iter()
        .find(|(candidate, _)| env_key_matches(candidate, key))
        .map(|(key, value)| (key.as_str(), value.as_str()))
}

pub(super) fn env_value<'a>(env: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    env_entry(env, key).map(|(_, value)| value)
}
