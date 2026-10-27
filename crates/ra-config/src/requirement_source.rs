// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Provenance of managed requirements, separate from ordinary configuration origins.

use ra_paths::AbsolutePathBuf;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
/// The original authority contributing an effective requirement.
pub enum RequirementSource {
    /// No specific authority was supplied.
    Unknown,
    /// Forced managed preferences from the named domain and key.
    MdmManagedPreferences {
        /// The managed preferences domain.
        domain: String,
        /// The managed preferences key.
        key: String,
    },
    /// Multiple requirements layers contributed to the final value. Sources are
    /// stored highest-priority first, matching the order surfaced in errors.
    Composite {
        /// Distinct flattened authorities, highest priority first.
        sources: Vec<RequirementSource>,
    },
    /// A backend-delivered enterprise-managed layer. `id` is the stable backend
    /// identifier; `name` is the admin-facing display name.
    EnterpriseManaged {
        /// The stable backend fragment identifier.
        id: String,
        /// The administrator-facing fragment name.
        name: String,
    },
    /// The system requirements document.
    SystemRequirementsToml {
        /// The absolute source file.
        file: AbsolutePathBuf,
    },
    /// A legacy managed configuration document.
    LegacyManagedConfigTomlFromFile {
        /// The absolute source file.
        file: AbsolutePathBuf,
    },
    /// The legacy managed configuration in forced preferences.
    LegacyManagedConfigTomlFromMdm,
}

impl RequirementSource {
    /// Flattens nested sources and removes duplicates while retaining priority order.
    pub fn composite(sources: impl IntoIterator<Item = RequirementSource>) -> Self {
        let mut flattened = Vec::new();
        for source in sources {
            source.append_to_composite(&mut flattened);
        }

        match flattened.len() {
            0 => RequirementSource::Unknown,
            1 => flattened.remove(0),
            _ => RequirementSource::Composite { sources: flattened },
        }
    }

    fn append_to_composite(self, flattened: &mut Vec<RequirementSource>) {
        match self {
            RequirementSource::Composite { sources } => {
                for source in sources {
                    source.append_to_composite(flattened);
                }
            }
            source => {
                if !flattened.contains(&source) {
                    flattened.push(source);
                }
            }
        }
    }
}

impl fmt::Display for RequirementSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RequirementSource::Unknown => write!(f, "<unspecified>"),
            RequirementSource::MdmManagedPreferences { domain, key } => {
                write!(f, "MDM {domain}:{key}")
            }
            RequirementSource::Composite { sources } => {
                write!(f, "requirements layers: ")?;
                for (index, source) in sources.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{source}")?;
                }
                Ok(())
            }
            RequirementSource::EnterpriseManaged { id, name } => {
                write!(f, "enterprise-managed requirements {name} ({id})")
            }
            RequirementSource::SystemRequirementsToml { file }
            | RequirementSource::LegacyManagedConfigTomlFromFile { file } => {
                write!(f, "{}", file.as_path().display())
            }
            RequirementSource::LegacyManagedConfigTomlFromMdm => {
                write!(f, "MDM managed_config.toml (legacy)")
            }
        }
    }
}
