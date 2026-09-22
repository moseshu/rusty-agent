//! Small scripts a session installs inside its own sandbox, and then runs there.
//!
//! Some questions can only be answered from inside: what the workspace currently hashes to, for
//! one, which means walking every file in it. Asking from outside would mean streaming the whole
//! workspace out just to hash it, and for a backend whose workspace is not on this machine it would
//! mean streaming it across a network. So the question travels instead of the data: a short POSIX
//! shell script is written into the sandbox once and executed there.
//!
//! # Installed under a content digest
//!
//! A helper's path carries a digest of its own text, so a session running against a workspace where
//! an older version was installed does not silently run the older one. Installation is idempotent:
//! a helper already there with the same content is left alone.
//!
//! # `/tmp` is not always writable
//!
//! The helpers install under `/tmp`, as the reference's do. The local backend's macOS fence denies
//! writes there — the reference's fence denies them too, from the same profile — so on that
//! platform installation fails and whatever needed the helper degrades instead. Every caller here
//! treats a helper it could not install as an answer it could not get, never as a bad answer.

use ra_core::sandbox::{ExecRequest, SandboxError, SandboxResult, SandboxSession, ShellInvocation};
use sha2::{Digest, Sha256};

/// Where helper scripts are installed inside a sandbox.
///
/// Named after this product rather than the reference's Python distribution, for the same reason
/// the default snapshot directory is: it is scratch space this SDK owns, not an interchange format.
const HELPER_INSTALL_ROOT: &str = "/tmp/rusty-agent/bin";

/// How much of the content digest goes in the installed name.
const DIGEST_PREFIX_LENGTH: usize = 12;

/// A script that runs inside a sandbox, and where it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeHelperScript {
    name: String,
    content: String,
    install_path: String,
}

impl RuntimeHelperScript {
    /// Names a helper, deriving its installed path from what it contains.
    #[must_use]
    pub fn from_content(name: &str, content: &str) -> Self {
        let digest = format!("{:x}", Sha256::digest(content.as_bytes()));
        Self {
            name: name.to_owned(),
            content: content.to_owned(),
            install_path: format!(
                "{HELPER_INSTALL_ROOT}/{name}-{}",
                &digest[..DIGEST_PREFIX_LENGTH]
            ),
        }
    }

    /// What this helper is called.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Where it lives inside the sandbox.
    #[must_use]
    pub fn install_path(&self) -> &str {
        &self.install_path
    }

    /// The command that writes it there.
    ///
    /// Written to a temporary name and moved into place, so a helper that is being installed while
    /// another session runs the same one never exists half-written under the path they both use. A
    /// destination whose content already matches is left as it is.
    #[must_use]
    pub fn install_command(&self) -> Vec<String> {
        let marker = self
            .install_path
            .rsplit('/')
            .next()
            .unwrap_or(&self.name)
            .to_uppercase()
            .replace(['-', '.'], "_");
        let heredoc = format!("RUSTY_AGENT_HELPER_{marker}");
        let script = format!(
            r#"# RUSTY_AGENT_INSTALL_RUNTIME_HELPER_V1
set -eu

dest="$1"
tmp="$dest.tmp.$$"

mkdir -p -- "$(dirname -- "$dest")"

cleanup() {{
    rm -f -- "$tmp"
}}
trap cleanup EXIT INT TERM

cat > "$tmp" <<'{heredoc}'
{content}
{heredoc}
chmod 0555 "$tmp"
if [ -d "$dest" ]; then
    rm -rf -- "$dest"
fi
if [ -x "$dest" ] && command -v cmp >/dev/null 2>&1 && cmp -s "$dest" "$tmp"; then
    rm -f -- "$tmp"
    trap - EXIT INT TERM
    exit 0
fi
rm -f -- "$dest"
mv -f -- "$tmp" "$dest"
trap - EXIT INT TERM"#,
            content = self.content,
        );
        vec![
            "sh".to_owned(),
            "-c".to_owned(),
            script,
            "sh".to_owned(),
            self.install_path.clone(),
        ]
    }

    /// The command that asks whether it is already there and runnable.
    #[must_use]
    pub fn present_command(&self) -> Vec<String> {
        vec![
            "test".to_owned(),
            "-x".to_owned(),
            self.install_path.clone(),
        ]
    }
}

/// Puts a helper in the sandbox, if an identical one is not already there.
///
/// **Run every time rather than remembered.** The reference keeps a per-session set of installed
/// helpers and probes with `test -x` when it finds one there, which costs exactly the same single
/// command this does, because the installer itself is the idempotent check: it compares the
/// destination with what it was about to write and exits without touching it when they match.
///
/// # Errors
///
/// Returns [`ra_core::sandbox::ErrorCode::ExecNonzero`] when the script could not be written, which
/// on a sandbox whose `/tmp` is not writable is the ordinary outcome.
pub async fn ensure_installed(
    session: &dyn SandboxSession,
    helper: &RuntimeHelperScript,
) -> SandboxResult<()> {
    let command = helper.install_command();
    let result = session
        .exec(ExecRequest::new(command).with_shell(ShellInvocation::None))
        .await?;
    if result.ok() {
        return Ok(());
    }
    Err(SandboxError::exec_nonzero(
        result,
        vec![
            "install_runtime_helper".to_owned(),
            helper.install_path.clone(),
        ],
    ))
}

/// What the workspace currently hashes to, computed from inside the sandbox.
///
/// Takes the workspace root, the fingerprint scheme's name, where to cache the answer, a digest of
/// the manifest, and the workspace-relative paths to leave out. Prints the cached record and writes
/// it, so one run both answers and remembers.
///
/// The archive it hashes is produced by `tar` inside the sandbox rather than by this process, which
/// is what lets the answer be about a workspace this machine may not be able to see.
#[must_use]
pub fn workspace_fingerprint_helper() -> RuntimeHelperScript {
    RuntimeHelperScript::from_content("workspace-fingerprint", WORKSPACE_FINGERPRINT_SCRIPT)
}

/// The script behind [`workspace_fingerprint_helper`].
///
/// Carried over from the reference's `session/runtime_helpers.py`, behaviour for behaviour: the
/// argument contract, the refusal of an exclude path that is not concrete and relative, the two
/// spellings of each exclude, the `tar` that cannot be told to stop expanding wildcards having its
/// patterns escaped instead, the three hashing tools tried in order, and the cache file written
/// through a temporary name.
///
/// One difference: the archive is produced by `sh -c` where the reference uses `sh -lc`. A login
/// shell runs the profile files first, and anything they print lands in the pipe being hashed — the
/// fingerprint would then describe the shell's greeting as well as the workspace.
/// The reference also loses producer failures through POSIX pipelines. This script checks tar's
/// completion separately and checks each hashing command before parsing its output, so a failed
/// read cannot publish a fingerprint of an empty or partial archive.
const WORKSPACE_FINGERPRINT_SCRIPT: &str = r#"#!/bin/sh
# RUSTY_AGENT_WORKSPACE_FINGERPRINT_V1
set -eu

if [ "$#" -lt 4 ]; then
    printf 'usage: %s <workspace-root> <version> <output-path> <manifest-digest> [exclude ...]\n' \
        "$0" >&2
    exit 64
fi

workspace_root=$1
version=$2
output_path=$3
manifest_digest=$4
shift 4

if [ ! -d "$workspace_root" ]; then
    printf 'workspace root not found: %s\n' "$workspace_root" >&2
    exit 66
fi

case "$workspace_root" in
    *"'"*)
        printf 'workspace root contains unsupported single quote: %s\n' "$workspace_root" >&2
        exit 65
        ;;
esac

quote_sh() {
    case "$1" in
        *"'"*)
            printf 'unsupported single quote in argument: %s\n' "$1" >&2
            exit 65
            ;;
        *)
            printf "'%s'" "$1"
            ;;
    esac
}

hash_stdin() {
    if command -v sha256sum >/dev/null 2>&1; then
        hash_output=$(sha256sum) || return $?
        printf '%s\n' "$hash_output" | awk '{print $1}'
        return
    fi
    if command -v shasum >/dev/null 2>&1; then
        hash_output=$(shasum -a 256) || return $?
        printf '%s\n' "$hash_output" | awk '{print $1}'
        return
    fi
    if command -v openssl >/dev/null 2>&1; then
        hash_output=$(openssl dgst -sha256) || return $?
        printf '%s\n' "$hash_output" | awk '{print $NF}'
        return
    fi
    printf 'workspace fingerprint needs sha256sum, shasum or openssl\n' >&2
    exit 127
}

if tar --help 2>&1 | grep -q -- '--no-wildcards'; then
    tar_command="tar --no-wildcards"
    escape_patterns=0
else
    tar_command="tar"
    escape_patterns=1
fi

for rel in "$@"; do
    case "$rel" in
        ""|"."|"/"|".."|/*|../*|*/../*|*/..)
            printf 'exclude must be a concrete relative path: %s\n' "$rel" >&2
            exit 65
            ;;
    esac
    if [ "$escape_patterns" -eq 1 ]; then
        rel=$(printf '%s\n' "$rel" | sed 's/[][\\*?]/\\&/g')
    fi
    tar_command="$tar_command --exclude=$(quote_sh "$rel") --exclude=$(quote_sh "./$rel")"
done

tar_command="$tar_command -C $(quote_sh "$workspace_root") -cf - ."

# POSIX pipelines report only the last command's status. Keep a failure marker until tar
# completes successfully so even an empty or partial stream can never become a valid record.
status_dir=$(mktemp -d)
trap 'rm -rf -- "$status_dir"' EXIT
trap 'exit 1' INT TERM
: > "$status_dir/tar_failed"
workspace_fingerprint=$(
    { sh -c "$tar_command" && rm -- "$status_dir/tar_failed"; } | hash_stdin
)
if [ -e "$status_dir/tar_failed" ]; then
    printf 'workspace archive could not be fingerprinted\n' >&2
    exit 1
fi
fingerprint=$(printf '%s\n%s\n' "$workspace_fingerprint" "$manifest_digest" | hash_stdin)

payload=$(printf '{"fingerprint":"%s","version":"%s"}' "$fingerprint" "$version")
mkdir -p -- "$(dirname -- "$output_path")"
tmp_output="$output_path.tmp.$$"
printf '%s' "$payload" > "$tmp_output"
mv -f -- "$tmp_output" "$output_path"
printf '%s' "$payload"
"#;
