// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Boilerplate-reduction macros for [`ToolExecutor`](crate::ToolExecutor) and
//! [`ErasedToolExecutor`](crate::ErasedToolExecutor) implementors.
//!
//! Issue #6019: both traits used to give the six risk-bearing methods
//! (`requires_confirmation`, `execute_tool_call_confirmed`, the checkpoint trio, and
//! `is_tool_speculatable` — plus their `_erased` counterparts) permissive default bodies.
//! Wrapper types that forgot to override one silently inherited a default that could
//! disable a security check, a checkpoint capability, or a confirmation gate. This
//! recurred five times across prior PRs. The traits no longer provide those defaults —
//! every implementor must now supply all six, and the compiler enforces it.
//!
//! These four macros exist only to keep that compiler-forced boilerplate short. They do
//! **not** themselves close the defect class — only the removal of the default bodies
//! does that. A macro invoked on the wrong type (e.g. `tool_executor_no_inner_defaults!()`
//! on a wrapper that owns an inner executor) silently reintroduces the exact bug this
//! issue fixes, because `macro_rules!` cannot check "this type has no delegate field."
//! Read each macro's own doc comment before using it.

/// Forwards the four mechanical capability methods of
/// [`ToolExecutor`](crate::ToolExecutor) — the checkpoint trio and `is_tool_speculatable`
/// — to `self.$inner`.
///
/// Use inside `impl ToolExecutor for YourWrapper` where `$inner` is the **field name**
/// (an identifier, not an expression — macro hygiene forbids capturing `self` at item
/// position) of a field whose type implements [`ToolExecutor`](crate::ToolExecutor).
///
/// The two policy methods, `requires_confirmation` and `execute_tool_call_confirmed`, are
/// intentionally **not** emitted by this macro — wrappers that gate on confirmation or
/// checkpoint policy must implement those two explicitly, so the compiler forces every
/// wrapper author to make a deliberate decision about them instead of inheriting one
/// silently.
///
/// # Examples
///
/// ```rust
/// use zeph_tools::{ToolExecutor, ToolCall, ToolOutput, ToolError};
///
/// struct PassThrough<T> {
///     inner: T,
/// }
///
/// impl<T: ToolExecutor> ToolExecutor for PassThrough<T> {
///     async fn execute(&self, response: &str) -> Result<Option<ToolOutput>, ToolError> {
///         self.inner.execute(response).await
///     }
///
///     fn requires_confirmation(&self, call: &ToolCall) -> bool {
///         self.inner.requires_confirmation(call)
///     }
///
///     async fn execute_tool_call_confirmed(
///         &self,
///         call: &ToolCall,
///     ) -> Result<Option<ToolOutput>, ToolError> {
///         self.inner.execute_tool_call_confirmed(call).await
///     }
///
///     zeph_tools::tool_executor_forward!(inner);
/// }
/// ```
#[macro_export]
macro_rules! tool_executor_forward {
    ($inner:ident) => {
        fn checkpoint_undo(&self, n: usize) -> $crate::CheckpointActionResult {
            $crate::ToolExecutor::checkpoint_undo(&self.$inner, n)
        }

        fn checkpoint_redo(&self) -> $crate::CheckpointActionResult {
            $crate::ToolExecutor::checkpoint_redo(&self.$inner)
        }

        fn checkpoint_list(&self) -> $crate::CheckpointListResult {
            $crate::ToolExecutor::checkpoint_list(&self.$inner)
        }

        fn is_tool_speculatable(&self, tool_id: &str) -> bool {
            $crate::ToolExecutor::is_tool_speculatable(&self.$inner, tool_id)
        }
    };
}

/// Emits the six risk-bearing [`ToolExecutor`](crate::ToolExecutor) methods with the same
/// trivial bodies the trait's removed defaults used to provide: no confirmation required,
/// confirmed execution falls back to `execute_tool_call`, checkpoints unsupported, not
/// speculatable.
///
/// # Use ONLY on leaf executors that own no wrapped executor
///
/// Invoking this macro on a type that wraps another [`ToolExecutor`](crate::ToolExecutor)
/// (i.e. has an `inner`/delegate field) silently disables forwarding for all six methods
/// and **reintroduces issue #6019**. Wrappers must use [`tool_executor_forward!`] for the
/// mechanical four and hand-write `requires_confirmation` /
/// `execute_tool_call_confirmed`. `macro_rules!` has no way to enforce this at compile
/// time — review carefully.
///
/// # Examples
///
/// ```rust
/// use zeph_tools::{ToolExecutor, ToolOutput, ToolError};
///
/// struct EchoExecutor;
///
/// impl ToolExecutor for EchoExecutor {
///     async fn execute(&self, _response: &str) -> Result<Option<ToolOutput>, ToolError> {
///         Ok(None)
///     }
///
///     zeph_tools::tool_executor_no_inner_defaults!();
/// }
/// ```
#[macro_export]
macro_rules! tool_executor_no_inner_defaults {
    () => {
        fn requires_confirmation(&self, _call: &$crate::ToolCall) -> bool {
            false
        }

        fn execute_tool_call_confirmed(
            &self,
            call: &$crate::ToolCall,
        ) -> impl ::std::future::Future<
            Output = ::std::result::Result<Option<$crate::ToolOutput>, $crate::ToolError>,
        > + Send {
            $crate::ToolExecutor::execute_tool_call(self, call)
        }

        fn checkpoint_undo(&self, _n: usize) -> $crate::CheckpointActionResult {
            $crate::CheckpointActionResult::unsupported()
        }

        fn checkpoint_redo(&self) -> $crate::CheckpointActionResult {
            $crate::CheckpointActionResult::unsupported()
        }

        fn checkpoint_list(&self) -> $crate::CheckpointListResult {
            $crate::CheckpointListResult::default()
        }

        fn is_tool_speculatable(&self, _tool_id: &str) -> bool {
            false
        }
    };
}

/// Erased-trait counterpart of [`tool_executor_forward!`]: forwards the checkpoint trio
/// and `is_tool_speculatable_erased` to `self.$inner`.
///
/// Use inside `impl ErasedToolExecutor for YourWrapper` where `$inner` is the **field
/// name** (an identifier, not an expression — see [`tool_executor_forward!`] for why) of
/// a field whose type implements [`ErasedToolExecutor`](crate::ErasedToolExecutor). As
/// with the static-side macro, the policy methods `requires_confirmation_erased` and
/// `execute_tool_call_confirmed_erased` are not emitted and must be hand-written.
#[macro_export]
macro_rules! erased_tool_executor_forward {
    ($inner:ident) => {
        fn checkpoint_undo_erased(&self, n: usize) -> $crate::CheckpointActionResult {
            $crate::ErasedToolExecutor::checkpoint_undo_erased(&*self.$inner, n)
        }

        fn checkpoint_redo_erased(&self) -> $crate::CheckpointActionResult {
            $crate::ErasedToolExecutor::checkpoint_redo_erased(&*self.$inner)
        }

        fn checkpoint_list_erased(&self) -> $crate::CheckpointListResult {
            $crate::ErasedToolExecutor::checkpoint_list_erased(&*self.$inner)
        }

        fn is_tool_speculatable_erased(&self, tool_id: &str) -> bool {
            $crate::ErasedToolExecutor::is_tool_speculatable_erased(&*self.$inner, tool_id)
        }
    };
}

/// Erased-trait counterpart of [`tool_executor_no_inner_defaults!`]: emits the six
/// risk-bearing [`ErasedToolExecutor`](crate::ErasedToolExecutor) methods with the same
/// trivial bodies the trait's removed defaults used to provide.
///
/// # Use ONLY on leaf executors that own no wrapped executor
///
/// Invoking this macro on a type that wraps another
/// [`ErasedToolExecutor`](crate::ErasedToolExecutor) silently disables forwarding for all
/// six methods and **reintroduces issue #6019**. Wrappers must use
/// [`erased_tool_executor_forward!`] for the mechanical four and hand-write
/// `requires_confirmation_erased` / `execute_tool_call_confirmed_erased`.
#[macro_export]
macro_rules! erased_tool_executor_no_inner_defaults {
    () => {
        fn requires_confirmation_erased(&self, _call: &$crate::ToolCall) -> bool {
            true
        }

        fn execute_tool_call_confirmed_erased<'a>(
            &'a self,
            call: &'a $crate::ToolCall,
        ) -> ::std::pin::Pin<
            Box<
                dyn ::std::future::Future<
                        Output = ::std::result::Result<
                            Option<$crate::ToolOutput>,
                            $crate::ToolError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            $crate::ErasedToolExecutor::execute_tool_call_erased(self, call)
        }

        fn checkpoint_undo_erased(&self, _n: usize) -> $crate::CheckpointActionResult {
            $crate::CheckpointActionResult::unsupported()
        }

        fn checkpoint_redo_erased(&self) -> $crate::CheckpointActionResult {
            $crate::CheckpointActionResult::unsupported()
        }

        fn checkpoint_list_erased(&self) -> $crate::CheckpointListResult {
            $crate::CheckpointListResult::default()
        }

        fn is_tool_speculatable_erased(&self, _tool_id: &str) -> bool {
            false
        }
    };
}
