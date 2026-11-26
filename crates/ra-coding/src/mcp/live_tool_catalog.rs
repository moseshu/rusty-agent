// Adapted from openai/codex (Apache-2.0) @ e58bf8f5233b51e7cd1679d947e3028c1ed91b06, modified.
//! Refreshes one connection's catalog on demand after a tool-list notification.

use std::{
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use ra_core::error::{Error, Result};
use ra_mcp::{client::McpClient, tool::McpToolCatalog};
use tokio::{
    sync::OnceCell,
    time::{Instant, timeout_at},
};

const REFRESH_COOLDOWN: Duration = Duration::from_secs(5);

#[derive(Clone, Copy)]
pub(super) struct CatalogListing {
    pub(super) timeout: Duration,
    pub(super) refresh_after_notification: bool,
}

#[derive(Default)]
pub(super) struct LiveToolCatalog {
    listed: Mutex<Option<Arc<CatalogSnapshot>>>,
    initialized: OnceCell<()>,
    retry_after: tokio::sync::Mutex<Option<Instant>>,
}

pub(super) struct CatalogSnapshot {
    pub(super) catalog: McpToolCatalog,
    notification_revision: u64,
}

#[derive(Clone)]
pub(super) struct CatalogBinding {
    pub(super) live: Arc<LiveToolCatalog>,
    pub(super) snapshot: Arc<CatalogSnapshot>,
}

impl CatalogBinding {
    pub(super) fn is_current(&self) -> bool {
        let Some(current) = self.live.latest() else {
            return false;
        };
        if Arc::ptr_eq(&current, &self.snapshot) {
            return true;
        }
        let mut current = current.catalog.tools();
        let mut expected = self.snapshot.catalog.tools();
        if current == expected {
            return true;
        }
        // Ordering between different tool names is immaterial. Preserve the order of duplicates.
        current.sort_by(|left, right| left.name.cmp(&right.name));
        expected.sort_by(|left, right| left.name.cmp(&right.name));
        current == expected
    }
}

impl LiveToolCatalog {
    pub(super) fn latest(&self) -> Option<Arc<CatalogSnapshot>> {
        self.listed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn revision(&self) -> Option<u64> {
        self.listed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|snapshot| snapshot.notification_revision)
    }

    fn publish(&self, catalog: McpToolCatalog, notification_revision: u64) {
        *self.listed.lock().unwrap_or_else(PoisonError::into_inner) =
            Some(Arc::new(CatalogSnapshot {
                catalog,
                notification_revision,
            }));
    }

    // Startup listing is lazy in this host. Calls and discovery share the first publication.
    pub(super) async fn initialize(&self, client: &McpClient) -> Result<()> {
        self.initialized
            .get_or_try_init(|| async {
                // The client's startup deadline covers the listing itself. Readers waiting
                // for initialization do not spend a notification refresh's deadline.
                let catalog = McpToolCatalog::fetch(client).await?;
                // Notifications arriving during initialization stay pending.
                self.publish(catalog, 0);
                Ok::<(), Error>(())
            })
            .await?;
        Ok(())
    }

    pub(super) async fn get(
        &self,
        client: &McpClient,
        listing: CatalogListing,
    ) -> Result<McpToolCatalog> {
        self.initialize(client).await?;
        if listing.refresh_after_notification
            && self.refresh(client, listing.timeout).await.is_err()
        {
            tracing::warn!(
                server_name = client.server_name(),
                "MCP tool catalog refresh failed; retaining the previous catalog"
            );
        }
        self.catalog()
    }

    pub(super) fn catalog(&self) -> Result<McpToolCatalog> {
        self.latest()
            .map(|snapshot| snapshot.catalog.clone())
            .ok_or_else(|| Error::config("MCP tool catalog was not initialized"))
    }

    async fn refresh(&self, client: &McpClient, timeout: Duration) -> Result<()> {
        let notifications = client.tool_list_revision();
        if self.revision() == Some(notifications.current()) {
            return Ok(());
        }
        let deadline = Instant::now() + timeout;
        let timed_out = |_| Error::config("MCP tool catalog refresh timed out");
        let mut retry_after = timeout_at(deadline, self.retry_after.lock())
            .await
            .map_err(timed_out)?;
        if self.revision() == Some(notifications.current())
            || retry_after.is_some_and(|retry| Instant::now() < retry)
        {
            return Ok(());
        }

        // If the reader cancels the future, prevent immediate retries of the abandoned request.
        *retry_after = Some(deadline + REFRESH_COOLDOWN);
        let result = timeout_at(deadline, async {
            for _ in 0..2 {
                let revision = notifications.current();
                let result = McpToolCatalog::fetch_for_revision(client, revision).await;
                if revision != notifications.current() {
                    continue;
                }
                self.publish(result?, revision);
                return Ok(());
            }
            Err(Error::config(
                "MCP tool list changed during catalog refresh",
            ))
        })
        .await
        .map_err(timed_out)
        .and_then(std::convert::identity);
        *retry_after = result.is_err().then(|| Instant::now() + REFRESH_COOLDOWN);
        result
    }
}
