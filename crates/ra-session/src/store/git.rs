//! Git metadata and credential-safe remote URLs, ported from Codex's protocol and thread store.

use std::fmt;
use std::ops::Deref;

use gix_url::Scheme;
use gix_url::Url;
use serde::Deserialize;
use serde::Serialize;

/// A git remote URL with authentication credentials removed.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SanitizedGitUrl(String);

impl SanitizedGitUrl {
    /// The remote address with credentials removed.
    #[must_use]
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl TryFrom<&str> for SanitizedGitUrl {
    type Error = &'static str;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        // Remote helpers wrap another remote, so peel off their prefixes
        // without recursion before passing the address to the Git URL parser.
        let mut address = value;
        while let Some((transport, nested_address)) = address.split_once("::")
            && !transport.is_empty()
            && transport.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '+' | '-' | '.')
            })
        {
            // Helpers can also receive executable command lines containing
            // arbitrary secrets; scan the whole payload once and reject those.
            if address.len() == value.len() && nested_address.chars().any(char::is_whitespace) {
                return Err("invalid git remote URL");
            }
            address = nested_address;
        }
        let helper_prefix = &value[..value.len() - address.len()];
        let original = value;
        let value = address;

        // Parse first to recognize both URL and SCP-style remotes and reject
        // malformed inputs before reconstructing the original text.
        let url = if let Ok(url) = Url::try_from(value) {
            url
        } else {
            // gix mistakes an IPv6 address's first colon for the SCP path
            // separator when a username comes before the bracketed host.
            let (user, host_and_path) = value.split_once('@').ok_or("invalid git remote URL")?;
            let (host, path) = host_and_path
                .strip_prefix('[')
                .and_then(|host_and_path| host_and_path.split_once("]:"))
                .ok_or("invalid git remote URL")?;
            if user.is_empty() || path.is_empty() {
                return Err("invalid git remote URL");
            }
            // SCP paths are opaque and can contain characters that URL parsing rejects.
            let ssh_url = format!("ssh://{user}@[{host}]/");
            Url::try_from(ssh_url.as_str()).map_err(|_| "invalid git remote URL")?
        };

        // SSH's conventional `git` user is a transport identity, not a secret;
        // every other username and all passwords must be removed.
        let preserve_ssh_git_user = url.scheme == Scheme::Ssh && url.user() == Some("git");

        // Inspect the authority directly because gix treats the complete
        // `file://` authority as its host and does not expose its userinfo.
        // Rebuilding only this part also preserves escaped repository paths.
        if let Some((scheme, authority_and_path)) = value.split_once("://") {
            let authority_end = authority_and_path
                .find('/')
                .unwrap_or(authority_and_path.len());
            let (authority, path) = authority_and_path.split_at(authority_end);
            let Some((_, host)) = authority.rsplit_once('@') else {
                return Ok(Self(original.to_owned()));
            };
            if preserve_ssh_git_user && url.password().is_none() {
                return Ok(Self(original.to_owned()));
            }
            let user = if preserve_ssh_git_user { "git@" } else { "" };
            return Ok(Self(format!(
                "{helper_prefix}{scheme}://{user}{host}{path}"
            )));
        }

        // Parsed serialization can alter Git paths, so preserve the exact
        // original representation whenever no credential needs removal.
        if url.password().is_none() && (url.user().is_none() || preserve_ssh_git_user) {
            return Ok(Self(original.to_owned()));
        }

        // SCP-style remotes have no URL authority; remove only `user@` and
        // leave the original `host:path` untouched.
        let authority_end = value.find(':').unwrap_or(value.len());
        let user_end = value[..authority_end]
            .rfind('@')
            .ok_or("invalid git remote URL")?;
        Ok(Self(format!("{helper_prefix}{}", &value[user_end + 1..])))
    }
}

impl TryFrom<String> for SanitizedGitUrl {
    type Error = &'static str;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::try_from(value.as_str())
    }
}

impl From<SanitizedGitUrl> for String {
    fn from(value: SanitizedGitUrl) -> Self {
        value.0
    }
}

impl AsRef<str> for SanitizedGitUrl {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl Deref for SanitizedGitUrl {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.as_str()
    }
}

impl fmt::Display for SanitizedGitUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

pub(crate) fn deserialize_optional_sanitized_git_url<'de, D>(
    deserializer: D,
) -> Result<Option<SanitizedGitUrl>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Malformed legacy origins must not prevent the rest of a rollout from loading.
    Ok(Option::<String>::deserialize(deserializer)?
        .and_then(|value| SanitizedGitUrl::try_from(value).ok()))
}

/// The persisted Git facts of a thread: Codex's `GitInfo`.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitInfo {
    /// Commit hash.
    pub commit_hash: Option<String>,
    /// Branch name.
    pub branch: Option<String>,
    /// Remote address with credentials removed.
    #[serde(default, deserialize_with = "deserialize_optional_sanitized_git_url")]
    pub repository_url: Option<SanitizedGitUrl>,
}

/// A partial Git metadata update: Codex's `GitInfoPatch` and `ClearableField` semantics.
#[non_exhaustive]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(
    clippy::option_option,
    reason = "Codex's unset, clear and replace patch fields"
)]
pub struct GitInfoPatch {
    /// Commit hash to set or clear.
    pub sha: Option<Option<String>>,
    /// Branch to set or clear.
    pub branch: Option<Option<String>>,
    /// Sanitized remote to set or clear.
    pub origin_url: Option<Option<SanitizedGitUrl>>,
}

impl GitInfoPatch {
    /// Merges the fields supplied by `next`, preserving unspecified fields.
    pub fn merge(&mut self, next: Self) {
        if next.sha.is_some() {
            self.sha = next.sha;
        }
        if next.branch.is_some() {
            self.branch = next.branch;
        }
        if next.origin_url.is_some() {
            self.origin_url = next.origin_url;
        }
    }

    /// Applies this patch; clearing all fields leaves no Git metadata.
    pub(crate) fn apply(&self, current: &mut Option<GitInfo>) {
        let mut info = current.clone().unwrap_or_default();
        if let Some(sha) = &self.sha {
            info.commit_hash.clone_from(sha);
        }
        if let Some(branch) = &self.branch {
            info.branch.clone_from(branch);
        }
        if let Some(origin) = &self.origin_url {
            info.repository_url.clone_from(origin);
        }
        *current =
            (info.commit_hash.is_some() || info.branch.is_some() || info.repository_url.is_some())
                .then_some(info);
    }
}
