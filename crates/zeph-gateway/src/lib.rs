//! HTTP gateway for webhook ingestion with bearer auth and health endpoint.

mod error;
mod handlers;
mod router;
mod server;

pub use error::GatewayError;
pub use server::GatewayServer;
