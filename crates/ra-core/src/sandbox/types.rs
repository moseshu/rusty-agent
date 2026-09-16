//! Value types shared by every sandbox backend.
//!
//! These are the leaves of the sandbox protocol: identities a command runs as, the permission bits
//! a materialized file carries, the result of running something, and the address a forwarded port
//! resolves to. Nothing here performs IO or knows which backend produced it.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

/// The identity a sandbox operation runs as.
///
/// Equality and hashing are by name alone. A user is a handle onto an account inside the sandbox,
/// and two handles naming the same account are the same user however they were constructed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct User {
    /// Account name inside the sandbox.
    pub name: String,
}

impl User {
    /// Names a sandbox user.
    #[must_use]
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl PartialEq for User {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for User {}

impl std::hash::Hash for User {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

/// A named collection of sandbox users.
///
/// Equality and hashing are by name alone, for the same reason as [`User`]: the membership list is
/// data about the group, not part of its identity. Two views of `admin` taken before and after a
/// member joined are still the same group.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Group {
    /// Group name inside the sandbox.
    pub name: String,
    /// Members of the group.
    pub users: Vec<User>,
}

impl Group {
    /// Names a group and its members.
    #[must_use]
    pub fn new(name: impl Into<String>, users: Vec<User>) -> Self {
        Self {
            name: name.into(),
            users,
        }
    }
}

impl PartialEq for Group {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

impl Eq for Group {}

impl std::hash::Hash for Group {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.name.hash(state);
    }
}

/// One triplet's worth of permission bits.
///
/// The values are the conventional octal digits, so they compose with shifts the way a mode does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileMode {
    /// No access.
    None,
    /// Execute only.
    Exec,
    /// Write only.
    Write,
    /// Read only.
    Read,
    /// Read, write and execute.
    All,
}

impl FileMode {
    /// The octal digit this mode contributes to one triplet.
    #[must_use]
    pub const fn bits(self) -> u32 {
        match self {
            Self::None => 0,
            Self::Exec => 1,
            Self::Write => 1 << 1,
            Self::Read => 1 << 2,
            Self::All => 0o7,
        }
    }
}

impl From<FileMode> for u32 {
    fn from(mode: FileMode) -> Self {
        mode.bits()
    }
}

impl std::ops::BitOr for FileMode {
    type Output = u32;

    fn bitor(self, rhs: Self) -> Self::Output {
        self.bits() | rhs.bits()
    }
}

impl std::ops::BitOr<FileMode> for u32 {
    type Output = u32;

    fn bitor(self, rhs: FileMode) -> Self::Output {
        self | rhs.bits()
    }
}

/// The permission bits a sandbox path carries, plus whether it is a directory.
///
/// # Why the default is owner-only
///
/// `owner` defaults to `0o7` while `group` and `other` default to nothing, so a value constructed
/// without an explicit mode is `0700`. Materialized workspace content is readable by the account
/// the sandbox runs as and by nobody else unless the caller says otherwise.
///
/// # Equality is by mode, not by field
///
/// Two values that render to the same mode are the same permissions, which is what makes this
/// usable as a map key. Comparing field by field would be the same partition today and would stop
/// being so the moment a field carries a value wider than its triplet.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(default)]
pub struct Permissions {
    /// Owner triplet.
    pub owner: u32,
    /// Group triplet.
    pub group: u32,
    /// Other triplet.
    pub other: u32,
    /// Whether the path is a directory.
    pub directory: bool,
}

/// The `S_IFDIR` bit, which [`Permissions::to_mode`] folds in for a directory.
const S_IFDIR: u32 = 0o040_000;

impl Default for Permissions {
    fn default() -> Self {
        Self {
            owner: 0o7,
            group: 0,
            other: 0,
            directory: false,
        }
    }
}

impl Permissions {
    /// Collapses the triplets, and the directory flag, into a single mode.
    #[must_use]
    pub const fn to_mode(self) -> u32 {
        let mut mode = (self.owner << 6) | (self.group << 3) | self.other;
        if self.directory {
            mode |= S_IFDIR;
        }
        mode
    }

    /// Splits a mode back into triplets and a directory flag.
    ///
    /// Bits above the three triplets are dropped apart from `S_IFDIR`: setuid, setgid and the
    /// sticky bit are not modelled, and keeping them in a field named `owner` would make
    /// [`Self::to_mode`] round-trip a value this type never claimed to carry.
    #[must_use]
    pub const fn from_mode(mode: u32) -> Self {
        Self {
            owner: (mode >> 6) & 0b111,
            group: (mode >> 3) & 0b111,
            other: mode & 0b111,
            directory: (mode & S_IFDIR) != 0,
        }
    }

    /// Grants the owner the given triplet.
    #[must_use]
    pub fn owner_can(mut self, mode: impl Into<u32>) -> Self {
        self.owner = mode.into();
        self
    }

    /// Grants the group the given triplet.
    #[must_use]
    pub fn group_can(mut self, mode: impl Into<u32>) -> Self {
        self.group = mode.into();
        self
    }

    /// Grants everyone else the given triplet.
    #[must_use]
    pub fn others_can(mut self, mode: impl Into<u32>) -> Self {
        self.other = mode.into();
        self
    }

    /// Parses the ten-character mode field `ls` prints.
    ///
    /// # Errors
    ///
    /// Returns [`PermissionsParseError`] when the string is not a mode field this type can
    /// represent: a wrong length, a type character other than `d` or `-`, or a flag that is not one
    /// of the accepted characters for its position.
    pub fn from_str_mode(perms: &str) -> Result<Self, PermissionsParseError> {
        // coreutils and BSD `ls` append one marker to the mode field to flag an alternate access
        // method: `+` for an ACL, `@` for macOS extended attributes, `.` for an SELinux context.
        // It is not a permission bit, and parsing would fail on the length check without this.
        let perms = match perms.chars().count() {
            11 if perms.ends_with(['@', '+', '.']) => &perms[..perms.len() - 1],
            _ => perms,
        };

        let chars: Vec<char> = perms.chars().collect();
        if chars.len() != 10 {
            return Err(PermissionsParseError::Length {
                input: perms.to_owned(),
            });
        }

        let directory = match chars[0] {
            'd' => true,
            '-' => false,
            _ => {
                return Err(PermissionsParseError::Type {
                    input: perms.to_owned(),
                });
            }
        };

        // The execute position doubles as the setuid/setgid/sticky flag, and which letters mean
        // that differs between the owner/group triplets and the last one.
        let owner = parse_triplet(&chars[1..4], ('s', 'S'))?;
        let group = parse_triplet(&chars[4..7], ('s', 'S'))?;
        let other = parse_triplet(&chars[7..10], ('t', 'T'))?;

        Ok(Self {
            owner,
            group,
            other,
            directory,
        })
    }
}

/// Parses one `rwx`-style triplet, accepting the special-bit letters for its position.
fn parse_triplet(triplet: &[char], special: (char, char)) -> Result<u32, PermissionsParseError> {
    let rendered: String = triplet.iter().collect();
    let invalid = || PermissionsParseError::Triplet {
        input: rendered.clone(),
    };

    let mut mask = 0;
    match triplet[0] {
        'r' => mask |= FileMode::Read.bits(),
        '-' => {}
        _ => return Err(invalid()),
    }
    match triplet[1] {
        'w' => mask |= FileMode::Write.bits(),
        '-' => {}
        _ => return Err(invalid()),
    }

    let (exec_with_special, special_without_exec) = special;
    let exec_flag = triplet[2];
    if exec_flag == 'x' || exec_flag == exec_with_special {
        mask |= FileMode::Exec.bits();
    } else if exec_flag != '-' && exec_flag != special_without_exec {
        return Err(invalid());
    }

    Ok(mask)
}

impl PartialEq for Permissions {
    fn eq(&self, other: &Self) -> bool {
        self.to_mode() == other.to_mode()
    }
}

impl Eq for Permissions {}

impl std::hash::Hash for Permissions {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.to_mode().hash(state);
    }
}

impl fmt::Display for Permissions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(if self.directory { "d" } else { "-" })?;
        for triplet in [self.owner, self.group, self.other] {
            for (bit, letter) in [
                (FileMode::Read.bits(), 'r'),
                (FileMode::Write.bits(), 'w'),
                (FileMode::Exec.bits(), 'x'),
            ] {
                formatter.write_str(if triplet & bit == 0 {
                    "-"
                } else {
                    match letter {
                        'r' => "r",
                        'w' => "w",
                        _ => "x",
                    }
                })?;
            }
        }
        Ok(())
    }
}

/// Why a mode field could not be parsed.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PermissionsParseError {
    /// The field was not ten characters long, after any trailing access-method marker.
    #[error("invalid permissions string length: {input:?}")]
    Length {
        /// The field as given.
        input: String,
    },
    /// The leading type character was neither `d` nor `-`.
    #[error("invalid permissions type: {input:?}")]
    Type {
        /// The field as given.
        input: String,
    },
    /// One triplet carried a character that is not valid in its position.
    #[error("invalid permissions triplet: {input:?}")]
    Triplet {
        /// The triplet as given.
        input: String,
    },
}

/// What running a command produced.
///
/// The two streams stay separate all the way out. A caller that wants them interleaved can
/// concatenate; a caller handed one merged buffer cannot recover which bytes were diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    /// Bytes the command wrote to standard output.
    pub stdout: Vec<u8>,
    /// Bytes the command wrote to standard error.
    pub stderr: Vec<u8>,
    /// The command's exit status.
    pub exit_code: i32,
}

impl ExecResult {
    /// Records what a command produced.
    #[must_use]
    pub const fn new(stdout: Vec<u8>, stderr: Vec<u8>, exit_code: i32) -> Self {
        Self {
            stdout,
            stderr,
            exit_code,
        }
    }

    /// Whether the command succeeded.
    #[must_use]
    pub const fn ok(&self) -> bool {
        self.exit_code == 0
    }
}

/// Where a port exposed by the sandbox can be reached from the host.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ExposedPortEndpoint {
    /// Host or address the port is reachable on.
    pub host: String,
    /// Port number on that host.
    pub port: u16,
    /// Whether the endpoint terminates TLS.
    #[serde(default)]
    pub tls: bool,
    /// Query string appended to the built URL, with or without a leading `?`.
    #[serde(default)]
    pub query: String,
}

impl ExposedPortEndpoint {
    /// Records a reachable endpoint with no TLS and no query.
    #[must_use]
    pub fn new(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            tls: false,
            query: String::new(),
        }
    }

    /// Marks the endpoint as terminating TLS.
    #[must_use]
    pub fn with_tls(mut self, tls: bool) -> Self {
        self.tls = tls;
        self
    }

    /// Appends a query string to URLs built from this endpoint.
    #[must_use]
    pub fn with_query(mut self, query: impl Into<String>) -> Self {
        self.query = query.into();
        self
    }

    /// Builds a URL for `scheme`, which is either `http` or `ws`.
    ///
    /// The default port for the resulting scheme is omitted, an IPv6 host is bracketed, and the
    /// query is appended with exactly one `?`.
    ///
    /// # Errors
    ///
    /// Returns [`UnsupportedScheme`] for anything other than `http` or `ws`. The two schemes differ
    /// only in prefix, but accepting arbitrary text would let a caller build a URL this endpoint
    /// cannot actually serve.
    pub fn url_for(&self, scheme: &str) -> Result<String, UnsupportedScheme> {
        let normalized = scheme.to_ascii_lowercase();
        let prefix = match (normalized.as_str(), self.tls) {
            ("http", false) => "http",
            ("http", true) => "https",
            ("ws", false) => "ws",
            ("ws", true) => "wss",
            _ => {
                return Err(UnsupportedScheme {
                    scheme: scheme.to_owned(),
                });
            }
        };
        let default_port: u16 = if self.tls { 443 } else { 80 };

        let host = if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };

        let base = if self.port == default_port {
            format!("{prefix}://{host}/")
        } else {
            format!("{prefix}://{host}:{}/", self.port)
        };

        let query = self.query.strip_prefix('?').unwrap_or(&self.query);
        if query.is_empty() {
            Ok(base)
        } else {
            Ok(format!("{base}?{query}"))
        }
    }
}

/// A URL scheme this endpoint cannot serve.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("scheme must be either 'http' or 'ws', got {scheme:?}")]
pub struct UnsupportedScheme {
    /// The scheme as given.
    pub scheme: String,
}

/// Structured metadata attached to a sandbox error.
///
/// Ordered rather than hashed so that two renderings of the same error compare equal, which a
/// snapshot test and a log diff both depend on.
pub type ErrorContext = BTreeMap<String, serde_json::Value>;
