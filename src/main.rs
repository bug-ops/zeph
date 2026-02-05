use std::path::{Path, PathBuf};

use zeph_core::agent::Agent;
use zeph_core::config::Config;
use zeph_llm::ollama::OllamaProvider;
use zeph_skills::prompt::format_skills_prompt;
use zeph_skills::registry::SkillRegistry;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let config = Config::load(Path::new("config/default.toml"))?;
    let provider = OllamaProvider::new(&config.llm.base_url, config.llm.model.clone());

    let skill_paths: Vec<PathBuf> = config.skills.paths.iter().map(PathBuf::from).collect();
    let registry = SkillRegistry::load(&skill_paths);
    let skills_prompt = format_skills_prompt(registry.all());

    tracing::info!("loaded {} skill(s)", registry.all().len());

    println!("zeph v{}", env!("CARGO_PKG_VERSION"));

    let mut agent = Agent::new(provider, &skills_prompt);
    agent.run().await
}
