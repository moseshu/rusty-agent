// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
#![allow(
    missing_docs,
    reason = "Internal records retain upstream field names and documentation; serde defines their wire contract."
)]
use ra_paths::LegacyAppPathString;
use ra_paths::PathUri;
use serde::Deserialize;
use serde::Serialize;
use std::sync::Arc;

/// A user-selected root that can expose one or more runtime capabilities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedCapabilityRoot {
    /// Stable identifier supplied by the capability selection platform.
    pub id: String,
    /// Where the selected root can be resolved.
    pub location: CapabilityRootLocation,
}

/// Roots selected for one environment, retaining their order in the thread's root list.
///
/// The index is each root's position in the original thread list. Executor skills assign
/// aliases (`e0`, `e1`, ...) in root order. Keeping the index lets us split roots across
/// environments and combine them again without changing those aliases.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvironmentCapabilityRoots(Arc<[(usize, SelectedCapabilityRoot)]>);

impl EnvironmentCapabilityRoots {
    /// Selects this environment's roots without losing their original positions.
    #[must_use]
    pub fn for_environment(environment_id: &str, roots: &[SelectedCapabilityRoot]) -> Self {
        let mut selected = Vec::new();
        for (index, root) in roots.iter().enumerate() {
            let CapabilityRootLocation::Environment {
                environment_id: root_environment_id,
                ..
            } = &root.location;
            if root_environment_id == environment_id {
                selected.push((index, root.clone()));
            }
        }
        Self(selected.into())
    }

    /// Restores root order so changing environment order does not change skill aliases.
    pub fn collect<'a>(
        selections: impl IntoIterator<Item = &'a Self>,
    ) -> Vec<SelectedCapabilityRoot> {
        let mut roots = Vec::new();
        for selection in selections {
            roots.extend(selection.0.iter());
        }
        roots.sort_by_key(|(index, _)| *index);
        roots.into_iter().map(|(_, root)| root.clone()).collect()
    }
}

/// Location used to resolve a selected capability root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum CapabilityRootLocation {
    /// A path owned by an execution environment.
    Environment {
        #[serde(rename = "environmentId")]
        environment_id: String,
        /// Absolute path for the root in the selected environment.
        #[serde(deserialize_with = "deserialize_path_uri_from_api_path")]
        path: PathUri,
    },
}

fn deserialize_path_uri_from_api_path<'de, D>(deserializer: D) -> Result<PathUri, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let path = LegacyAppPathString::deserialize(deserializer)?;
    if let Ok(path_uri) = PathUri::parse(path.as_str()) {
        return Ok(path_uri);
    }
    path.try_into().map_err(serde::de::Error::custom)
}
