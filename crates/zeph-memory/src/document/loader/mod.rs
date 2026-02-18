mod text;
pub use text::TextLoader;

#[cfg(feature = "pdf")]
mod pdf;
#[cfg(feature = "pdf")]
pub use pdf::PdfLoader;
