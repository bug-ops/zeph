use std::path::Path;

use zeph_core::agent::Agent;
use zeph_core::config::Config;
use zeph_llm::ollama::OllamaProvider;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config = Config::load(Path::new("config/default.toml"))?;
    let provider = OllamaProvider::new(&config.llm.base_url, config.llm.model.clone());

    println!("zeph v{}", env!("CARGO_PKG_VERSION"));

    let mut agent = Agent::new(provider);
    agent.run().await
}
