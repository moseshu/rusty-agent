// Adapted from openai/codex (Apache-2.0) @ 815ef9bbc47bef3bde9cfceaaaf3ef74bb43d052, modified.
//! Explicit plugin instructions for accepted user input.

use std::sync::Arc;

use async_trait::async_trait;
use ra_core::{
    capability::{Capability, CapabilityFamily},
    context::RunContext,
    error::{Error, Result},
    item::{ContentBlock, Message, MessageRole, ModelInputItem},
};
use ra_paths::AbsolutePathBuf;
use ra_protocol::server::protocol::UserInput;

pub(crate) struct PluginInputContributor {
    services: Arc<super::PluginServices>,
    cwd: AbsolutePathBuf,
    mcp: Option<Arc<crate::mcp::CodingMcpRuntime>>,
    family: CapabilityFamily,
}

impl PluginInputContributor {
    pub(crate) fn new(
        services: Arc<super::PluginServices>,
        cwd: AbsolutePathBuf,
        mcp: Option<Arc<crate::mcp::CodingMcpRuntime>>,
    ) -> Result<Self> {
        Ok(Self {
            services,
            cwd,
            mcp,
            family: CapabilityFamily::new("plugin_mentions")?,
        })
    }
}

#[async_trait]
impl Capability for PluginInputContributor {
    fn kind(&self) -> CapabilityFamily {
        self.family.clone()
    }

    async fn contribute_turn_input(
        &self,
        run: &RunContext,
        input: &[ModelInputItem],
    ) -> Result<Vec<Message>> {
        let input: Vec<_> = input
            .iter()
            .filter_map(|item| match item {
                ModelInputItem::Message(message) if message.role() == MessageRole::User => {
                    Some(message)
                }
                _ => None,
            })
            .flat_map(Message::content)
            .filter_map(|block| match block {
                ContentBlock::Text(text) => Some(UserInput::text(text.text())),
                ContentBlock::Mention { name, path } => Some(UserInput::mention(name, path)),
                _ => None,
            })
            .collect();
        if super::collect_explicit_plugin_ids(&input).is_empty() {
            return Ok(Vec::new());
        }
        let snapshot = self
            .services
            .snapshot(self.cwd.clone())
            .await
            .map_err(|error| {
                Error::config("failed to resolve mentioned plugins").with_source(error)
            })?;
        let visible = match &self.mcp {
            Some(mcp) => mcp.visible_server_names(run).await?,
            None => Vec::new(),
        };
        Ok(snapshot
            .explicit_plugin_instructions(&input, &visible)
            .into_iter()
            .map(Message::developer)
            .collect())
    }
}
