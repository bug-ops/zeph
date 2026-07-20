// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Shared `macro_rules!` helpers for the per-domain command-handler impls.

/// Delegates a command handler to an inner async method, wrapping the
/// inner method's error in [`CommandError`](zeph_commands::CommandError). Covers the
/// pass-through shape shared by most command handlers: an optional single argument
/// forwarded verbatim, and the returned `Result<T, E>` mapped to `Result<T, CommandError>`
/// via `e.to_string()`.
macro_rules! delegate_cmd {
    ($name:ident, $inner:ident $(, $arg:ident : $arg_ty:ty)? => $out:ty) => {
        fn $name<'a>(
            &'a mut self,
            $($arg: $arg_ty)?
        ) -> Pin<Box<dyn Future<Output = Result<$out, CommandError>> + Send + 'a>> {
            Box::pin(async move {
                self.$inner($($arg)?)
                    .await
                    .map_err(|e| CommandError::new(e.to_string()))
            })
        }
    };
}

pub(crate) use delegate_cmd;
