// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Generic plugin metadata and resource mapping, without activation or execution.

use super::hook_config::HooksFile;

/// Parsed plugin metadata parameterized by its resource locator representation.
///
/// Host loading uses absolute paths. Owners can map resources to their own locators
/// without changing the metadata or inline declarations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifest<Resource> {
    /// The manifest name.
    pub name: String,
    /// The optional declared version.
    pub version: Option<String>,
    /// The optional description.
    pub description: Option<String>,
    /// The declared search keywords.
    pub keywords: Vec<String>,
    /// The declared component resources.
    pub paths: PluginManifestPaths<Resource>,
    /// Optional presentation metadata.
    pub interface: Option<PluginManifestInterface<Resource>>,
}

/// Component resources declared by a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifestPaths<Resource> {
    /// Declared skill roots.
    pub skills: Vec<Resource>,
    /// The optional onboarding skill resource.
    pub onboarding_skill: Option<Resource>,
    /// Declared MCP configuration.
    pub mcp_servers: Option<PluginManifestMcpServers<Resource>>,
    /// The optional app configuration resource.
    pub apps: Option<Resource>,
    /// Declared hook files or typed inline declarations.
    pub hooks: Option<PluginManifestHooks<Resource>>,
}

/// MCP server declarations embedded in or referenced by a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginManifestMcpServers<Resource> {
    /// A referenced configuration resource.
    Path(Resource),
    /// The compact JSON object, with stable key ordering.
    Object(String),
}

/// Hook declarations embedded in or referenced by a plugin manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginManifestHooks<Resource> {
    /// Referenced hook configuration resources.
    Paths(Vec<Resource>),
    /// Typed inline hook declarations.
    Inline(Vec<HooksFile>),
}

/// Optional model- and UI-facing plugin metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginManifestInterface<Resource> {
    /// The optional presentation name.
    pub display_name: Option<String>,
    /// The optional short description.
    pub short_description: Option<String>,
    /// The optional long description.
    pub long_description: Option<String>,
    /// The optional developer label.
    pub developer_name: Option<String>,
    /// The optional category label.
    pub category: Option<String>,
    /// Declared capability labels.
    pub capabilities: Vec<String>,
    /// The optional website URL.
    pub website_url: Option<String>,
    /// The optional privacy-policy URL.
    pub privacy_policy_url: Option<String>,
    /// The optional terms-of-service URL.
    pub terms_of_service_url: Option<String>,
    /// Normalized suggested prompts.
    pub default_prompt: Option<Vec<String>>,
    /// The optional brand color.
    pub brand_color: Option<String>,
    /// The optional composer icon resource.
    pub composer_icon: Option<Resource>,
    /// The optional logo resource.
    pub logo: Option<Resource>,
    /// The optional dark-theme logo resource.
    pub logo_dark: Option<Resource>,
    /// Screenshot resources.
    pub screenshots: Vec<Resource>,
}

impl<Resource> Default for PluginManifestInterface<Resource> {
    fn default() -> Self {
        Self {
            display_name: None,
            short_description: None,
            long_description: None,
            developer_name: None,
            category: None,
            capabilities: Vec::new(),
            website_url: None,
            privacy_policy_url: None,
            terms_of_service_url: None,
            default_prompt: None,
            brand_color: None,
            composer_icon: None,
            logo: None,
            logo_dark: None,
            screenshots: Vec::new(),
        }
    }
}

impl<Resource> PluginManifest<Resource> {
    /// Returns the model- and UI-facing package name, falling back to the manifest name.
    #[must_use]
    pub fn display_name(&self) -> &str {
        self.interface
            .as_ref()
            .and_then(|interface| interface.display_name.as_deref())
            .map(str::trim)
            .filter(|display_name| !display_name.is_empty())
            .unwrap_or(&self.name)
    }

    /// Maps every path-bearing resource in the manifest.
    pub fn try_map_resources<Mapped, Error>(
        self,
        mut map: impl FnMut(Resource) -> Result<Mapped, Error>,
    ) -> Result<PluginManifest<Mapped>, Error> {
        let PluginManifest {
            name,
            version,
            description,
            keywords,
            paths,
            interface,
        } = self;
        let PluginManifestPaths {
            skills,
            onboarding_skill,
            mcp_servers,
            apps,
            hooks,
        } = paths;
        let hooks = match hooks {
            Some(PluginManifestHooks::Paths(paths)) => Some(PluginManifestHooks::Paths(
                paths
                    .into_iter()
                    .map(&mut map)
                    .collect::<Result<Vec<_>, _>>()?,
            )),
            Some(PluginManifestHooks::Inline(hooks)) => Some(PluginManifestHooks::Inline(hooks)),
            None => None,
        };
        let mcp_servers = match mcp_servers {
            Some(PluginManifestMcpServers::Path(path)) => {
                Some(PluginManifestMcpServers::Path(map(path)?))
            }
            Some(PluginManifestMcpServers::Object(servers)) => {
                Some(PluginManifestMcpServers::Object(servers))
            }
            None => None,
        };
        let interface = match interface {
            Some(interface) => {
                let PluginManifestInterface {
                    display_name,
                    short_description,
                    long_description,
                    developer_name,
                    category,
                    capabilities,
                    website_url,
                    privacy_policy_url,
                    terms_of_service_url,
                    default_prompt,
                    brand_color,
                    composer_icon,
                    logo,
                    logo_dark,
                    screenshots,
                } = interface;
                Some(PluginManifestInterface {
                    display_name,
                    short_description,
                    long_description,
                    developer_name,
                    category,
                    capabilities,
                    website_url,
                    privacy_policy_url,
                    terms_of_service_url,
                    default_prompt,
                    brand_color,
                    composer_icon: composer_icon.map(&mut map).transpose()?,
                    logo: logo.map(&mut map).transpose()?,
                    logo_dark: logo_dark.map(&mut map).transpose()?,
                    screenshots: screenshots
                        .into_iter()
                        .map(&mut map)
                        .collect::<Result<Vec<_>, _>>()?,
                })
            }
            None => None,
        };

        Ok(PluginManifest {
            name,
            version,
            description,
            keywords,
            paths: PluginManifestPaths {
                skills: skills
                    .into_iter()
                    .map(&mut map)
                    .collect::<Result<Vec<_>, _>>()?,
                onboarding_skill: onboarding_skill.map(&mut map).transpose()?,
                mcp_servers,
                apps: apps.map(&mut map).transpose()?,
                hooks,
            },
            interface,
        })
    }
}
