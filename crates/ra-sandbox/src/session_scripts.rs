//! Scripts a session runs inside its sandbox to answer questions about a path as some account.
//!
//! Shared by every backend that checks access by running a command rather than by looking: the local
//! backend when a caller names another account, and a container backend always, since the
//! container's filesystem is only reachable through commands run inside it. The scripts are the
//! reference's, byte for byte, so a copy is recognisable by its marker line whichever backend ran it.

/// Asks whether a file is readable by the account the check runs as.
// Only the local backend checks access this way, and it is built for unix alone.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) const READ_ACCESS_CHECK_SCRIPT: &str = r#"[ -r "$1" ]"#;

/// How long the existence probe may take before a refused read is reported as a read failure.
pub(crate) const READ_PATH_PROBE_TIMEOUT_S: f64 = 10.0;

/// Tells "is not there" from "may not be looked at", as the account that was refused.
///
/// A failed `[ -r ]` means either, and the difference is the difference between a not-found and a
/// read failure. The probe resolves the path's symlinks itself — as that account, so a link the
/// account cannot follow is not followed — and then walks up to the deepest ancestor that exists.
/// Exit 0 is "it is there after all", 1 is "the account can see that it is missing", and 2 is
/// everything it could not decide, including a parent the account may not search.
///
/// The reference's script, byte for byte; the marker on its first line is how a copy is recognised.
pub(crate) const READ_PATH_PROBE_SCRIPT: &str = r#"# READ_PATH_PROBE_V3
LC_ALL=C
export LC_ALL
path=$1
resolved_path=
symlink_depth=0

resolve_probe_path() {
    if [ "$symlink_depth" -gt 40 ] || [ "${#1}" -gt 4095 ]; then
        return 2
    fi
    if [ "$1" = "/" ]; then
        resolved_path=/
        return 0
    fi

    parent=${1%/*}
    if [ -z "$parent" ] || [ "$parent" = "$1" ]; then
        parent=/
    fi
    resolve_probe_path "$parent" || return 2
    resolved_parent=$resolved_path
    base=${1##*/}
    if [ "${#base}" -gt 255 ]; then
        return 2
    fi
    if [ "$resolved_parent" = "/" ]; then
        candidate=/$base
    else
        candidate=$resolved_parent/$base
    fi

    if [ -L "$candidate" ]; then
        target_with_marker=$(readlink -n "$candidate" && printf .) || return 2
        target=${target_with_marker%.}
        symlink_depth=$((symlink_depth + 1))
        if [ "$symlink_depth" -gt 40 ]; then
            return 2
        fi
        case "$target" in
            /*)
                resolve_probe_path "$target"
                ;;
            *)
                resolve_probe_path "$resolved_parent/$target"
                ;;
        esac
        return $?
    fi

    resolved_path=$candidate
}

resolve_probe_path "$path" || exit 2
path=$resolved_path
candidate=$path
child=

while :; do
    if [ -e "$candidate" ]; then
        if [ "$candidate" = "$path" ]; then
            exit 0
        fi
        if [ ! -d "$candidate" ] || [ ! -x "$candidate" ]; then
            exit 2
        fi
        lookup_result=$(
            find "$child" -prune -print 2>&1 >/dev/null
            lookup_status=$?
            printf '.%s' "$lookup_status"
        )
        lookup_status=${lookup_result##*.}
        lookup_error=${lookup_result%.*}
        if [ "$lookup_status" -eq 1 ]; then
            lookup_error=$(printf %s "$lookup_error")
            case "$lookup_error" in
                *": No such file or directory")
                    exit 1
                    ;;
            esac
        fi
        exit 2
    fi
    if [ "$candidate" = "/" ]; then
        exit 2
    fi
    child=$candidate
    candidate=${candidate%/*}
    if [ -z "$candidate" ]; then
        candidate=/
    fi
done"#;

/// How much of a command's error output a failure keeps.
const DIAGNOSTIC_MAX_CHARS: usize = 4096;

/// Decodes a command's output for an error context, truncated the way the reference truncates it.
///
/// Counted in characters rather than bytes, and marked with an ellipsis when cut, so a long stream
/// of output is still readable text and still says that it is not all there.
pub(crate) fn diagnostic_text(bytes: &[u8]) -> String {
    let decoded = String::from_utf8_lossy(bytes);
    match decoded.char_indices().nth(DIAGNOSTIC_MAX_CHARS) {
        Some((cut, _)) => format!("{}…", &decoded[..cut]),
        None => decoded.into_owned(),
    }
}

/// Asks whether a directory could be created; the second argument says whether parents may be too.
// Only the local backend checks access this way, and it is built for unix alone.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) const MKDIR_ACCESS_CHECK_SCRIPT: &str = concat!(
    "target=\"$1\"\n",
    "parents=\"$2\"\n",
    "if [ -e \"$target\" ] || [ -L \"$target\" ]; then\n",
    "    [ -d \"$target\" ] && [ -x \"$target\" ]\n",
    "    exit $?\n",
    "fi\n",
    "parent=$(dirname \"$target\")\n",
    "if [ \"$parents\" = \"1\" ]; then\n",
    "    while [ ! -e \"$parent\" ]; do\n",
    "        next=$(dirname \"$parent\")\n",
    "        if [ \"$next\" = \"$parent\" ]; then\n",
    "            exit 1\n",
    "        fi\n",
    "        parent=\"$next\"\n",
    "    done\n",
    "fi\n",
    "[ -d \"$parent\" ] && [ -w \"$parent\" ] && [ -x \"$parent\" ]\n",
);

/// Asks whether a path could be removed; the second argument says whether a missing one is fine.
// Only the local backend checks access this way, and it is built for unix alone.
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) const RM_ACCESS_CHECK_SCRIPT: &str = concat!(
    "target=\"$1\"\n",
    "recursive=\"$2\"\n",
    "if [ ! -e \"$target\" ] && [ ! -L \"$target\" ]; then\n",
    "    [ \"$recursive\" = \"1\" ]\n",
    "    exit $?\n",
    "fi\n",
    "parent=$(dirname \"$target\")\n",
    "[ -d \"$parent\" ] && [ -w \"$parent\" ] && [ -x \"$parent\" ]\n",
);
