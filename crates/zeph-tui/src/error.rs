/// Errors specific to zeph-tui.
#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("TUI channel closed")]
    ChannelClosed,

    #[error("confirm dialog cancelled")]
    ConfirmCancelled,
}
