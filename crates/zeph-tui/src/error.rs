#[derive(Debug, thiserror::Error)]
pub enum TuiError {
    #[error("terminal I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("channel error: {0}")]
    Channel(#[from] zeph_core::channel::ChannelError),
}
