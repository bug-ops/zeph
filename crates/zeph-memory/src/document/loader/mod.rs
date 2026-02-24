// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

mod text;
pub use text::TextLoader;

#[cfg(feature = "pdf")]
mod pdf;
#[cfg(feature = "pdf")]
pub use pdf::PdfLoader;
