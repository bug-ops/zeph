// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

use std::io::{self, Write as _};
use std::path::Path;
use std::process::Command;

use anyhow::{Context as _, bail};
use zeph_core::bootstrap::resolve_config_path;
use zeph_subagent::error::SubAgentError;
use zeph_subagent::{SubAgentDef, ToolPolicy, is_valid_agent_name, resolve_agent_paths};

use crate::cli::AgentsCommand;

pub(crate) async fn handle_agents_command(
    cmd: AgentsCommand,
    config_path: Option<&Path>,
) -> anyhow::Result<()> {
    match cmd {
        AgentsCommand::List => handle_list(config_path),
        AgentsCommand::Show { name } => handle_show(&name, config_path),
        AgentsCommand::Create {
            name,
            description,
            dir,
            model,
        } => handle_create(&name, &description, dir.as_path(), model.as_deref()),
        AgentsCommand::Edit { name } => handle_edit(&name, config_path),
        AgentsCommand::Delete { name, yes } => handle_delete(&name, yes, config_path),
    }
}

fn load_all_defs(config_path: Option<&Path>) -> anyhow::Result<Vec<SubAgentDef>> {
    let config_file = resolve_config_path(config_path);
    let config = zeph_core::config::Config::load(&config_file).unwrap_or_default();
    let paths = resolve_agent_paths(
        &[],
        config.agents.user_agents_dir.as_ref(),
        &config.agents.extra_dirs,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    SubAgentDef::load_all_with_sources(
        &paths,
        &[],
        config.agents.user_agents_dir.as_ref(),
        &config.agents.extra_dirs,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

fn handle_list(config_path: Option<&Path>) -> anyhow::Result<()> {
    let defs = load_all_defs(config_path)?;
    if defs.is_empty() {
        println!("No sub-agent definitions found.");
        return Ok(());
    }

    let name_w = defs.iter().map(|d| d.name.len()).max().unwrap_or(4).max(4);
    let scope_w = defs
        .iter()
        .map(|d| d.source.as_deref().unwrap_or("-").len())
        .max()
        .unwrap_or(5)
        .max(5);
    let desc_w = 40usize;

    println!(
        "{:<name_w$}  {:<scope_w$}  {:<desc_w$}  MODEL",
        "NAME", "SCOPE", "DESCRIPTION"
    );
    println!("{}", "-".repeat(name_w + scope_w + desc_w + 20usize));

    for d in &defs {
        let scope = d.source.as_deref().unwrap_or("-");
        let desc = truncate(&d.description, desc_w);
        let model = d.model.as_ref().map_or("-", |m| m.as_str());
        println!(
            "{:<name_w$}  {:<scope_w$}  {:<desc_w$}  {}",
            d.name, scope, desc, model
        );
    }

    Ok(())
}

fn handle_show(name: &str, config_path: Option<&Path>) -> anyhow::Result<()> {
    let defs = load_all_defs(config_path)?;
    let def = defs
        .iter()
        .find(|d| d.name == name)
        .ok_or_else(|| anyhow::anyhow!("agent not found: {name}"))?;

    println!("Name:        {}", def.name);
    println!("Description: {}", def.description);
    println!("Source:      {}", def.source.as_deref().unwrap_or("-"));
    println!(
        "Model:       {}",
        def.model.as_ref().map_or("-", |m| m.as_str())
    );
    println!("Mode:        {:?}", def.permissions.permission_mode);
    println!("Max turns:   {}", def.permissions.max_turns);
    println!("Background:  {}", def.permissions.background);

    let tools_str = match &def.tools {
        ToolPolicy::AllowList(v) => format!("allow {v:?}"),
        ToolPolicy::DenyList(v) => format!("deny {v:?}"),
        ToolPolicy::InheritAll => "inherit_all".to_owned(),
    };
    if def.disallowed_tools.is_empty() {
        println!("Tools:       {tools_str}");
    } else {
        println!("Tools:       {tools_str} except {:?}", def.disallowed_tools);
    }

    if !def.skills.include.is_empty() || !def.skills.exclude.is_empty() {
        println!(
            "Skills:      include {:?} exclude {:?}",
            def.skills.include, def.skills.exclude
        );
    }

    if !def.system_prompt.is_empty() {
        println!("\nSystem prompt:\n{}", def.system_prompt);
    }

    Ok(())
}

fn handle_create(
    name: &str,
    description: &str,
    dir: &Path,
    model: Option<&str>,
) -> anyhow::Result<()> {
    if !is_valid_agent_name(name) {
        anyhow::bail!("invalid agent name '{name}': must match ^[a-zA-Z0-9][a-zA-Z0-9_-]{{0,63}}$");
    }
    let target_path = dir.join(format!("{name}.md"));
    if target_path.exists() {
        anyhow::bail!(
            "agent '{name}' already exists at {}; use `zeph agents edit {name}` to modify it",
            target_path.display()
        );
    }
    let mut def = SubAgentDef::default_template(name, description);
    if let Some(m) = model {
        def.model = Some(zeph_subagent::ModelSpec::Named(m.to_owned()));
    }

    let target = def
        .save_atomic(dir)
        .map_err(|e: SubAgentError| anyhow::anyhow!("{e}"))?;
    println!("Created {}", target.display());
    Ok(())
}

fn handle_edit(name: &str, config_path: Option<&Path>) -> anyhow::Result<()> {
    let defs = load_all_defs(config_path)?;
    let def = defs
        .iter()
        .find(|d| d.name == name)
        .ok_or_else(|| anyhow::anyhow!("agent not found: {name}"))?;

    let path = def
        .file_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("cannot determine file path for agent '{name}'"))?;

    // CRIT-03: VISUAL > EDITOR > vi fallback chain.
    // Security: $VISUAL/$EDITOR are trusted env vars (like `git commit` behavior).
    // The value is used as the executable name/path, not shell-expanded, so shell
    // metacharacters are not a concern. However, the caller controls which binary
    // runs with the agent file path as its first argument — only use in trusted envs.
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "vi".to_owned());

    let status = Command::new(&editor).arg(path).status().with_context(|| {
        format!(
            "failed to launch editor '{editor}'; \
                 set $EDITOR or $VISUAL environment variable"
        )
    })?;

    if !status.success() {
        bail!("editor exited with non-zero status");
    }

    // Re-parse to validate after editing.
    let content =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    SubAgentDef::parse(&content)
        .map_err(|e| anyhow::anyhow!("definition is invalid after editing: {e}"))?;

    println!("Updated {}", path.display());
    Ok(())
}

fn handle_delete(name: &str, yes: bool, config_path: Option<&Path>) -> anyhow::Result<()> {
    let defs = load_all_defs(config_path)?;
    let def = defs
        .iter()
        .find(|d| d.name == name)
        .ok_or_else(|| anyhow::anyhow!("agent not found: {name}"))?;

    let path = def
        .file_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("cannot determine file path for agent '{name}'"))?;

    if !yes {
        print!("Delete {}? [y/N] ", path.display());
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Aborted.");
            return Ok(());
        }
    }

    SubAgentDef::delete_file(path).map_err(|e: SubAgentError| anyhow::anyhow!("{e}"))?;
    println!("Deleted {name}");
    Ok(())
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_owned()
    } else {
        let truncated: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{truncated}…")
    }
}
