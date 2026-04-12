// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Adapters implementing `zeph-context` traits for `zeph-core` internal types.
//!
//! Each impl wraps an `Agent`-internal type and exposes only the methods needed
//! by `ContextAssembler`. This keeps `zeph-context` free of any `zeph-core` dependency.

use std::pin::Pin;

use zeph_context::input::IndexAccess;

use super::state::IndexState;

impl IndexAccess for IndexState {
    fn fetch_code_rag<'a>(
        &'a self,
        query: &'a str,
        budget_tokens: usize,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<Option<String>, zeph_context::error::ContextError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            self.fetch_code_rag(query, budget_tokens)
                .await
                .map_err(|e| zeph_context::error::ContextError::Assembly(format!("{e:#}")))
        })
    }
}
