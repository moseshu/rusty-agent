//! Role-aware composition of workspace capabilities and configured host services.
//!
//! Specialist and planner roles inherit the host's tools and sandbox by default, as in Codex.
//! An explicitly selected product extension withholds patches and collaboration for these roles.
//! One-off answers still install no tools. The withheld inventory narrows profile selection and
//! budgets together, while missing required services remain assembly errors.
//!
//! Capability fragments and runtime bindings are selected from the same installed surface.
//! Planner tools bind Plan-mode behavior without changing their advertised schemas.

use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use ra_core::{
    capability::{Capability, CapabilityFamily},
    error::{Error, Result},
    prompt::{PromptRole, PromptSection},
    tool::{Tool, ToolLookupKey, ToolNamespace},
};
use ra_runtime::{capability::CapabilityPlan, tool::profile::ToolSurface};
use ra_tools::{
    agent_ns::collaboration_tools,
    request_user_input::{ModeKind, RequestUserInputTool, request_user_input_available_modes},
};

use crate::{agent::InstalledCapability, host::CodingHost};

/// What one role installs, and what being that role costs it.
///
/// Both halves are answers this module gives, not inputs a caller supplies: which capabilities a
/// role gets is decided here, and so is which ones it gives up.
pub(crate) struct RoleCapabilities {
    installed: CapabilityPlan,
    withheld: Vec<Arc<dyn Capability>>,
    native_web: Option<crate::web_search::CodingWebSearch>,
    native_web_offered: bool,
    web_fallback: bool,
}

pub(crate) struct SurfaceContributions {
    pub(crate) sections: Vec<PromptSection>,
    pub(crate) installed: Vec<InstalledCapability>,
    pub(crate) runtime: Vec<Arc<dyn Capability>>,
}

impl RoleCapabilities {
    /// Resolves the tool-bearing capabilities a coding agent installs for one role.
    ///
    /// The order the plan is built in is the order the tools reach the registry: observation first,
    /// then the plan board, then the editing entry, then execution. Nothing here declares a
    /// dependency, so resolution keeps it.
    ///
    /// **A one-off role installs nothing, whatever host is handed in.** Its role text says it
    /// answers without tool execution, and installing a capability anyway would put dispatchable
    /// entries behind a prompt that denies they exist. It withholds every one rather than never
    /// naming them: the profile has to be told that an empty surface is this role's shape and not a
    /// tier that lost every entry it declared.
    ///
    /// Optional services are installed only when configured on the host. A tier that requires an
    /// unconfigured service still fails by name; the role filter cannot disguise its absence.
    ///
    /// # Errors
    ///
    /// Propagates tool construction failures, and any incoherence the plan finds in the installed
    /// set — two capabilities claiming one family or a dependency nothing installs.
    pub(crate) fn resolve(
        role: &PromptRole,
        host: &CodingHost,
        profile: crate::profile::CodingProfile,
    ) -> Result<Self> {
        let mode = if matches!(role, PromptRole::Planner) {
            ModeKind::Plan
        } else {
            ModeKind::Default
        };
        let restricted = role.is_read_only() && host.read_only_role_restrictions();
        let mut declared: Vec<Arc<dyn Capability>> = vec![
            Arc::new(host.filesystem_capability()?),
            Arc::new(host.search_capability()?),
            Arc::new(host.view_image_capability()?),
            Arc::new(host.todo_capability()?.with_mode(mode)),
            Arc::new(host.apply_patch_capability()?),
            Arc::new(host.shell_capability()?),
        ];
        if let Some(capability) = host.web_capability()? {
            declared.push(capability);
        }
        if let Some(capability) = host.skills_capability()? {
            declared.push(Arc::new(capability));
        }
        if let Some(capability) = host.skills_ns_capability(profile)? {
            declared.push(Arc::new(capability));
        }
        if host.request_user_input_enabled() {
            declared.push(Arc::new(ControlCapability::user_input(mode)?));
        }
        if let Some(contributor) = host.plugin_input_contributor()? {
            declared.push(contributor);
        }
        declared.extend(host.capabilities().iter().cloned());
        // A spawned agent is assembled without the collaboration tools unless the host says its
        // model takes them, as Codex plans a sub-agent session's tools; the family is then
        // withheld, so the tier's band shrinks with it rather than failing its floor.
        let mut not_offered: Vec<Arc<dyn Capability>> = Vec::new();
        if host.collaboration_enabled() {
            let capability =
                ControlCapability::collaboration(host.collaboration_namespace()?.as_ref())?;
            if host.collaboration_offered() {
                declared.push(Arc::new(capability));
            } else {
                not_offered.push(Arc::new(capability));
            }
        }

        let (installed, mut withheld): (Vec<_>, Vec<_>) = if role.is_one_off() {
            (Vec::new(), declared)
        } else if restricted {
            declared.into_iter().partition(|capability| {
                capability.kind() != CapabilityFamily::APPLY_PATCH
                    && capability.kind().as_str() != COLLABORATION_FAMILY
            })
        } else {
            (declared, Vec::new())
        };
        withheld.extend(not_offered);

        Ok(Self {
            installed: CapabilityPlan::resolve(installed)?,
            withheld,
            native_web: host.web_search().cloned(),
            native_web_offered: !role.is_one_off(),
            web_fallback: host.has_web_fallback(),
        })
    }

    /// Native declarations selected by the role; tiers select them with the ordinary profile.
    pub(crate) fn hosted_tools(&self) -> Result<Vec<ra_core::model::ModelToolDefinition>> {
        if self.web_fallback {
            return Ok(Vec::new());
        }
        match &self.native_web {
            Some(config) if self.native_web_offered => Ok(config
                .definition_for_turn(config.sandbox_disabled)?
                .into_iter()
                .collect()),
            _ => Ok(Vec::new()),
        }
    }

    fn native_withheld_keys(&self) -> BTreeSet<ToolLookupKey> {
        let mut keys = BTreeSet::new();
        if self.web_fallback {
            return keys;
        }
        if let Some(config) = &self.native_web {
            if let Ok(key) = ToolLookupKey::for_call("web_fetch", None) {
                keys.insert(key);
            }
            if (!self.native_web_offered
                || !config.enabled
                || !config.supported
                || config.mode_for_turn(config.sandbox_disabled)
                    == crate::web_search::WebSearchMode::Disabled)
                && let Ok(key) = ToolLookupKey::for_call("web_search", None)
            {
                keys.insert(key);
            }
        }
        keys
    }

    /// The tools the installed set contributes, in assembly order.
    pub(crate) fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.installed
            .capabilities()
            .iter()
            .flat_map(|capability| capability.tools())
            .collect()
    }

    /// The routing identities this role gave up, for a profile to discount.
    ///
    /// Keys rather than names: a profile selects by routing identity, and the model-facing name a
    /// tool projects to is allowed to differ from it.
    pub(crate) fn withheld_tool_keys(&self) -> BTreeSet<ToolLookupKey> {
        self.withheld
            .iter()
            .flat_map(|capability| capability.tools())
            .map(|tool| tool.origin().lookup_key().clone())
            .chain(self.native_withheld_keys())
            .collect()
    }

    /// The withheld routing identities that would have occupied an advertised-budget slot.
    ///
    /// Profile selection removes every withheld tool, including host-only ones. Its entry-count
    /// budget measures only tools exposed to a model, so only this subset may lower its bounds.
    pub(crate) fn withheld_advertised_tool_keys(&self) -> BTreeSet<ToolLookupKey> {
        self.withheld
            .iter()
            .flat_map(|capability| capability.tools())
            .filter(|tool| tool.options().is_advertised_to_model())
            .map(|tool| tool.origin().lookup_key().clone())
            .chain(self.native_withheld_keys())
            .collect()
    }

    /// The families this role gave up, in the order they were declared.
    ///
    /// Families rather than tool names, because that is the unit the role decided on: a read-only
    /// agent did not decline three entries, it declined the capabilities those entries belong to.
    pub(crate) fn withheld_families(&self) -> Vec<CapabilityFamily> {
        self.withheld
            .iter()
            .map(|capability| capability.kind())
            .chain(
                (self.native_web.is_some() && !self.native_web_offered)
                    .then_some(CapabilityFamily::WEB),
            )
            .collect()
    }

    /// What the installed capabilities contribute to one assembled surface: fragments and entries.
    ///
    /// A role decides which capabilities are installed; a tier then decides which of their entries
    /// the request actually carries. This is where the second decision reaches the prompt: a
    /// fragment is kept only when every entry it can speak for survived into the advertised
    /// surface. The alternative is the failure the whole path exists to prevent — a paragraph
    /// telling the model how `grep` returns its matches, in a request that carries no `grep`.
    ///
    /// A family the tier dropped entirely is dropped silently, because that is the tier saying so:
    /// the surface's own floor is what catches a tier that lost entries it meant to keep, and
    /// re-reporting it here would name the prompt for a decision the profile made.
    ///
    /// A family the tier dropped *in part* is refused. A capability is atomic — that is the whole
    /// claim of the type — and its fragment describes the family as one thing, so half a family
    /// leaves text naming an entry the request does not carry with no way to tell which half the
    /// text meant.
    ///
    /// A capability with no tools keeps its fragment. Not every capability speaks
    /// for tools; one that contributes none has nothing for a tier to drop, and gating it on an
    /// empty intersection would silence exactly the fragments that are pure policy.
    ///
    /// **The entry attribution falls out of the same walk rather than being asked for separately.**
    /// Which family produced `write_stdin` is knowable only where the installed set meets the
    /// assembled surface, and a second walk asking the tools to project their names again could
    /// answer differently from the one the fragments were filtered against — an integration is
    /// allowed to be mutable, and a report is exactly where the two readings would be presented as
    /// one fact.
    ///
    /// # Errors
    ///
    /// Propagates fragment resolution and the section checks
    /// [`CapabilityPlan::static_prompt_sections`] performs, and refuses a tier that advertises part of a
    /// capability's entries.
    pub(crate) async fn contributions_for(
        &self,
        surface: &ToolSurface,
    ) -> Result<SurfaceContributions> {
        let charged: BTreeSet<&str> = surface.advertised_names().collect();
        let contributions: Vec<_> = self
            .installed
            .capabilities()
            .iter()
            .map(|capability| {
                let (present, absent) = split_advertised(capability, &charged);
                (capability, present, absent)
            })
            .collect();
        let selected: BTreeSet<_> = surface
            .tools()
            .iter()
            .map(|tool| tool.origin().lookup_key().clone())
            .chain(self.native_withheld_keys())
            .collect();
        for capability in self.installed.capabilities() {
            let tools = capability.tools();
            let count = tools
                .iter()
                .filter(|tool| selected.contains(tool.origin().lookup_key()))
                .count();
            if count != 0 && count != tools.len() {
                return Err(Error::config(format!(
                    "tool surface `{}` selects only part of capability `{}`; select its tools together",
                    surface.profile(),
                    capability.kind()
                )));
            }
        }
        // Resolve only fragments the tier can use. In particular, a Core agent must not load a
        // remote skill catalog that its selected tools cannot reach. The installed product
        // families have no dependencies on each other; the original plan validated their set.
        let described: Vec<_> = contributions
            .iter()
            .filter(|(capability, _, _)| {
                let tools = capability.tools();
                tools.is_empty()
                    || tools
                        .iter()
                        .any(|tool| selected.contains(tool.origin().lookup_key()))
            })
            .map(|(capability, _, _)| Arc::clone(capability))
            .collect();
        let runtime = described.clone();
        let mut fragments = CapabilityPlan::resolve(described)?
            .static_prompt_sections()
            .await?;
        let mut kept = Vec::new();
        let mut installed = Vec::new();

        for (capability, present, absent) in contributions {
            // Located by provenance rather than by position: a capability that contributes no
            // fragment leaves no row, so pairing by index would attribute every later fragment to
            // the wrong family. Resolution already refused a fragment whose source is not its own
            // family's, so the match is exact.
            let source = capability.kind().prompt_source();
            if let Some(index) = fragments
                .iter()
                .position(|fragment| fragment.source() == &source)
            {
                let fragment = fragments.remove(index);
                if absent.is_empty() {
                    kept.push(fragment);
                } else if !present.is_empty() {
                    return Err(Error::config(format!(
                        "the tool surface `{}` advertises {} of capability `{}` but not {}, and \
                         the capability's prompt fragment `{}` describes them as one mechanism; a \
                         prefix built from it would name an entry the request does not carry. \
                         Select the family's entries together, or leave all of them out",
                        surface.profile(),
                        render_names(&present),
                        capability.kind(),
                        render_names(&absent),
                        fragment.name()
                    )));
                }
            }
            installed.push(InstalledCapability::new(capability.kind(), present));
        }

        if let Some(orphan) = fragments.first() {
            return Err(Error::caller(format!(
                "prompt fragment `{}` is attributed to `{}`, which is not among the installed \
                 capabilities",
                orphan.name(),
                orphan.source()
            )));
        }
        if !self.hosted_tools()?.is_empty() {
            installed.push(InstalledCapability::new(
                CapabilityFamily::WEB,
                surface
                    .hosted_tools()
                    .iter()
                    .map(|tool| tool.name().to_owned())
                    .collect(),
            ));
        }
        Ok(SurfaceContributions {
            sections: kept,
            installed,
            runtime,
        })
    }
}

/// The capability family of the collaboration tools.
pub(crate) const COLLABORATION_FAMILY: &str = "collaboration";

/// Product assembly of control tools that already obtain their runtime ports from tool services.
///
/// No handler or schema is redefined here, and no prompt text is added: Codex carries the guidance
/// for these tools in their own descriptions, and the capability only attributes them to a family
/// a role can withhold as a unit.
struct ControlCapability {
    family: CapabilityFamily,
    tools: Vec<Arc<dyn Tool>>,
}

impl ControlCapability {
    fn user_input(mode: ModeKind) -> Result<Self> {
        Ok(Self {
            family: CapabilityFamily::new("user_input")?,
            tools: vec![Arc::new(
                RequestUserInputTool::new(request_user_input_available_modes(true))?
                    .with_mode(mode),
            )],
        })
    }

    /// The six tools, in `namespace` when the host gives one, as Codex's configuration places
    /// them.
    fn collaboration(namespace: Option<&ToolNamespace>) -> Result<Self> {
        let tools = collaboration_tools()?
            .into_iter()
            .map(|tool| match namespace {
                Some(namespace) => tool.in_namespace(namespace.clone()),
                None => Ok(tool),
            })
            .map(|tool| tool.map(|tool| Arc::new(tool) as Arc<dyn Tool>))
            .collect::<Result<_>>()?;
        Ok(Self {
            family: CapabilityFamily::new(COLLABORATION_FAMILY)?,
            tools,
        })
    }
}

#[async_trait]
impl Capability for ControlCapability {
    fn kind(&self) -> CapabilityFamily {
        self.family.clone()
    }

    fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools.clone()
    }
}

/// One capability's advertised entries, split by whether the assembled surface carries them.
///
/// Read once and used for both answers a caller needs — which fragments survive, and which entries
/// each family put on the surface — because the projection it reads is allowed to change between
/// calls.
///
/// Sorted by model-facing name, the order the surface and the prompt inventory both use. A
/// capability is free to construct its tools in whatever order suits it, and a report or an error
/// that echoed that order would present a host's internal arrangement as a fact about the surface.
fn split_advertised(
    capability: &Arc<dyn Capability>,
    charged: &BTreeSet<&str>,
) -> (Vec<String>, Vec<String>) {
    let mut names: Vec<String> = capability
        .tools()
        .iter()
        .filter(|tool| tool.options().is_advertised_to_model())
        .map(|tool| tool.model_definition().qualified_name())
        .collect();
    names.sort();
    names
        .into_iter()
        .partition(|name| charged.contains(name.as_str()))
}

/// Renders one side of a partial selection, for an error that has to name both.
fn render_names(names: &[String]) -> String {
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(", ")
}
