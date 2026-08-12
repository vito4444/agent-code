//! Memory.
//!
//! Three separate things share this crate because they share a table, and one of the reasons
//! they are worth reading together is precisely that they must not be allowed to touch each
//! other:
//!
//! - [`rules`] — instructions the user typed. Verbatim, never rewritten, never retired by
//!   anything automatic.
//! - [`facts`] — inferences with two time axes, superseded rather than deleted.
//! - [`recall`] — retrieval, and the rendering that marks a recalled fact as evidence.

pub mod extract;
pub mod facts;
pub mod recall;
pub mod rules;

use anyhow::Result;
use std::sync::Arc;
use wkbd_agent::session::{Prelude, PreludeProvider, SessionPurpose};
use wkbd_store::Store;

/// Supplies the prelude for a session from rules plus recalled facts.
///
/// The budget is small on purpose. Instruction files are context, not enforced configuration:
/// the vendor's own guidance is to stay under a couple of hundred lines because adherence
/// falls as the file grows, and that contradictory instructions get resolved arbitrarily. So
/// injection is capped and the long tail is left to an on-demand query during the run rather
/// than pushed in ahead of time.
pub struct StorePrelude {
    store: Store,
    max_memories: usize,
}

impl StorePrelude {
    pub fn new(store: Store) -> Self {
        Self { store, max_memories: 12 }
    }

    pub fn with_budget(store: Store, max_memories: usize) -> Self {
        Self { store, max_memories }
    }

    async fn build(&self, project_root: &str, purpose: SessionPurpose) -> Result<Prelude> {
        let rules = rules::applicable(&self.store, project_root).await?;

        // Recall is scoped to the project and ordered by recency. A query-driven recall
        // happens during the run through a tool; this is the standing context, so there is no
        // query to match against yet.
        let facts = facts::live(&self.store, Some(project_root)).await?;
        let mut candidates: Vec<_> = facts
            .into_iter()
            // External-sourced facts never auto-inject. Memory poisoning needs one successful
            // write, and the write channels include everything an agent reads: issue text,
            // fetched pages, a third-party repository's own instruction files.
            .filter(|f| f.source_trust.may_auto_inject())
            .collect();
        candidates.sort_by_key(|f| std::cmp::Reverse(f.created_ms));
        candidates.truncate(self.max_memories);

        let memories = candidates
            .iter()
            .map(|f| {
                recall::render_for_prelude(&recall::Recalled { fact: f.clone(), rank: 0.0 })
            })
            .collect();

        // The purpose is recorded so that "which entry points actually applied the rules" is
        // an observation rather than a claim. Rules quietly not applying in one situation is
        // the failure this whole arrangement exists to prevent.
        tracing::debug!(
            ?purpose,
            rules = rules.len(),
            memories = candidates.len(),
            "built session prelude"
        );

        Ok(Prelude { rules, memories })
    }
}

impl PreludeProvider for StorePrelude {
    fn prelude_for(&self, project_root: &str, purpose: SessionPurpose) -> Prelude {
        // The trait is synchronous because session opening is, so this blocks on the store.
        // Failing to build a prelude must not fail the session: a session with no recalled
        // context is degraded, a session that will not open is broken.
        let store = self.store.clone();
        let root = project_root.to_string();
        let budget = self.max_memories;
        let handle = tokio::runtime::Handle::try_current();

        let result = match handle {
            Ok(handle) => std::thread::scope(|scope| {
                scope
                    .spawn(|| {
                        handle.block_on(async {
                            StorePrelude { store, max_memories: budget }
                                .build(&root, purpose)
                                .await
                        })
                    })
                    .join()
                    .unwrap_or_else(|_| Ok(Prelude::default()))
            }),
            Err(_) => Ok(Prelude::default()),
        };

        match result {
            Ok(prelude) => prelude,
            Err(e) => {
                tracing::error!(error = %e, "could not build the session prelude");
                Prelude::default()
            }
        }
    }
}

/// Convenience constructor for the daemon.
pub fn prelude_provider(store: Store) -> Arc<dyn PreludeProvider> {
    Arc::new(StorePrelude::new(store))
}

#[cfg(test)]
mod tests;
