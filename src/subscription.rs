//! Subscription lifecycle handle (port of `pymt5._subscription`).

use crate::client::Mt5Client;
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscriptionKind {
    Ticks,
    Book,
}

/// Handle for a tick/book subscription.
///
/// Rust has no async `Drop`, so call [`SubscriptionHandle::unsubscribe`] explicitly. As a
/// safety net, dropping an *active* handle inside a Tokio runtime spawns a best-effort
/// unsubscribe task.
pub struct SubscriptionHandle {
    client: Mt5Client,
    kind: SubscriptionKind,
    ids: Vec<u32>,
    active: bool,
}

impl SubscriptionHandle {
    pub(crate) fn new(client: Mt5Client, kind: SubscriptionKind, ids: &[u32]) -> Self {
        Self { client, kind, ids: ids.to_vec(), active: true }
    }

    /// Symbol ids covered by this subscription.
    pub fn ids(&self) -> &[u32] { &self.ids }

    pub fn active(&self) -> bool { self.active }

    /// Explicitly unsubscribe. Idempotent.
    pub async fn unsubscribe(&mut self) -> Result<()> {
        if self.active {
            self.active = false;
            match self.kind {
                SubscriptionKind::Ticks => self.client.unsubscribe_ticks(&self.ids).await?,
                SubscriptionKind::Book => self.client.unsubscribe_book(&self.ids).await?,
            }
        }
        Ok(())
    }
}

impl Drop for SubscriptionHandle {
    fn drop(&mut self) {
        if !self.active { return; }
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let (client, kind, ids) = (self.client.clone(), self.kind, std::mem::take(&mut self.ids));
            rt.spawn(async move {
                let _ = match kind {
                    SubscriptionKind::Ticks => client.unsubscribe_ticks(&ids).await,
                    SubscriptionKind::Book => client.unsubscribe_book(&ids).await,
                };
            });
        }
    }
}
