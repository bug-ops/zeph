// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! No-op compressor used when `[tools.compression] enabled = false`.

use std::future::Future;
use std::pin::Pin;

use zeph_common::ToolName;

use super::{CompressionError, OutputCompressor};

/// Pass-through compressor that never modifies tool output.
///
/// Used as the default compressor when `[tools.compression] enabled = false`.
/// The `compress` method always returns `Ok(None)`.
///
/// # Examples
///
/// ```rust
/// # let rt = tokio::runtime::Runtime::new().unwrap();
/// # rt.block_on(async {
/// use zeph_tools::compression::{IdentityCompressor, OutputCompressor};
/// use zeph_common::ToolName;
/// let c = IdentityCompressor;
/// let name = ToolName::new("shell");
/// let result = c.compress(&name, "some output").await;
/// assert!(result.unwrap().is_none());
/// # });
/// ```
#[derive(Debug, Default, Clone)]
pub struct IdentityCompressor;

impl OutputCompressor for IdentityCompressor {
    fn compress<'a>(
        &'a self,
        _tool_name: &'a ToolName,
        _output: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<Option<String>, CompressionError>> + Send + 'a>> {
        Box::pin(std::future::ready(Ok(None)))
    }

    fn name(&self) -> &'static str {
        "identity"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn identity_always_passes_through() {
        let c = IdentityCompressor;
        let name = ToolName::new("shell");
        let result = c.compress(&name, "hello world").await.unwrap();
        assert!(result.is_none());
    }
}
