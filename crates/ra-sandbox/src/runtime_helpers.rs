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

/// Where a path really leads inside the sandbox, and whether the session may use it.
///
/// Takes the workspace root, the candidate path, `1` or `0` for whether the use is a write, and then
/// pairs of extra grant root and `1` or `0` for read-only. Resolves every symlink along the
/// candidate — as the sandbox sees them, which a caller outside it cannot — and prints the resolved
/// path when it lands under the root or a grant. The exit status says what went wrong otherwise:
/// `111` for a path that escapes everything, `112` for a symlink loop, `113` for a grant that
/// resolves to the filesystem root, `114` for a write under a read-only grant, `64` for arguments
/// that do not parse.
///
/// The reference's `RESOLVE_WORKSPACE_PATH_HELPER`, script for script.
#[must_use]
pub fn resolve_workspace_path_helper() -> RuntimeHelperScript {
    RuntimeHelperScript::from_content("resolve-workspace-path", RESOLVE_WORKSPACE_PATH_SCRIPT)
}

/// The script behind [`resolve_workspace_path_helper`], byte for byte the reference's.
///
/// Including the run of spaces in the middle of the read-only `printf`: the reference's source
/// breaks that line with a backslash inside a Python string, which joins the two halves rather than
/// continuing a shell line, and the text that reaches the sandbox is the joined one.
const RESOLVE_WORKSPACE_PATH_SCRIPT: &str = r#"#!/bin/sh
# RESOLVE_WORKSPACE_REALPATH_V1
set -eu

root="$1"
candidate="$2"
for_write="$3"
shift 3
max_symlink_depth=64

case "$for_write" in
    0|1) ;;
    *)
        printf 'for_write must be 0 or 1: %s\n' "$for_write" >&2
        exit 64
        ;;
esac

if [ $(( $# % 2 )) -ne 0 ]; then
    printf 'extra path grants must be root/read_only pairs\n' >&2
    exit 64
fi

resolve_path() {
    path="$1"
    depth="${2:-0}"
    seen="${3:-}"
    if [ "$path" = "/" ]; then
        printf '/\n'
        return 0
    fi

    if [ "$depth" -ge "$max_symlink_depth" ]; then
        printf 'symlink resolution depth exceeded: %s\n' "$path" >&2
        exit 112
    fi

    if [ -d "$path" ]; then
        (
            cd "$path"
            pwd -P
        )
        return 0
    fi

    parent=${path%/*}
    base=${path##*/}
    if [ -z "$parent" ] || [ "$parent" = "$path" ]; then
        parent="/"
    fi

    resolved_parent=$(resolve_path "$parent" "$depth" "$seen")
    candidate_path="$resolved_parent/$base"
    if [ -L "$candidate_path" ]; then
        case ":$seen:" in
            *":$candidate_path:"*)
                printf 'symlink resolution depth exceeded: %s\n' "$candidate_path" >&2
                exit 112
                ;;
        esac
        target=$(readlink "$candidate_path")
        next_depth=$((depth + 1))
        next_seen="${seen}:$candidate_path"
        case "$target" in
            /*) resolve_path "$target" "$next_depth" "$next_seen" ;;
            *) resolve_path "$resolved_parent/$target" "$next_depth" "$next_seen" ;;
        esac
        return 0
    fi

    printf '%s\n' "$candidate_path"
}

resolved_candidate=$(resolve_path "$candidate" 0)
best_grant_root=""
best_grant_original=""
best_grant_read_only="0"
best_grant_len=0

check_root() {
    allowed_root="$1"
    resolved_root=$(resolve_path "$allowed_root" 0)
    case "$resolved_candidate" in
        "$resolved_root"|"$resolved_root"/*)
            printf '%s\n' "$resolved_candidate"
            exit 0
            ;;
    esac
}

reject_root_grant() {
    allowed_root="$1"
    resolved_root=$(resolve_path "$allowed_root" 0)
    if [ "$resolved_root" = "/" ]; then
        printf 'extra path grant must not resolve to filesystem root: %s\n' "$allowed_root" >&2
        exit 113
    fi
}

consider_extra_grant() {
    allowed_root="$1"
    read_only="$2"
    case "$read_only" in
        0|1) ;;
        *)
            printf 'extra path grant read_only must be 0 or 1: %s\n' "$read_only" >&2
            exit 64
            ;;
    esac

    reject_root_grant "$allowed_root"
    resolved_root=$(resolve_path "$allowed_root" 0)
    case "$resolved_candidate" in
        "$resolved_root"|"$resolved_root"/*)
            root_len=${#resolved_root}
            if [ "$root_len" -gt "$best_grant_len" ]; then
                best_grant_root="$resolved_root"
                best_grant_original="$allowed_root"
                best_grant_read_only="$read_only"
                best_grant_len="$root_len"
            fi
            ;;
    esac
}

while [ "$#" -gt 0 ]; do
    consider_extra_grant "$1" "$2"
    shift 2
done

check_root "$root"
if [ -n "$best_grant_root" ]; then
    if [ "$for_write" = "1" ] && [ "$best_grant_read_only" = "1" ]; then
        printf 'read-only extra path grant: %s\nresolved path: %s\n'             "$best_grant_original" "$resolved_candidate" >&2
        exit 114
    fi
    printf '%s\n' "$resolved_candidate"
    exit 0
fi

printf 'workspace escape: %s\n' "$resolved_candidate" >&2
exit 111"#;

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
