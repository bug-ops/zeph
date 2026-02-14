use zeph_memory::semantic::estimate_tokens;

const BASE_PROMPT: &str = "\
You are Zeph, an AI coding assistant running in the user's terminal.\n\
\n\
## Tool Use\n\
The ONLY way to execute commands is by writing bash in a fenced code block \
with the `bash` language tag. The block runs automatically and the output is returned to you.\n\
\n\
Example:\n\
```bash\n\
ls -la\n\
```\n\
\n\
Do NOT invent other formats (tool_code, tool_call, <execute>, etc.). \
Only ```bash blocks are executed; anything else is treated as plain text.\n\
\n\
## Skills\n\
Skills are instructions that may appear below inside XML tags. \
Read them and follow the instructions; use ```bash blocks to act.\n\
\n\
If you see a list of other skill names and descriptions, those are \
for reference only. You cannot invoke or load them. Ignore them unless \
the user explicitly asks about a skill by name.\n\
\n\
## Guidelines\n\
- Be concise. Avoid unnecessary preamble.\n\
- Before editing files, read them first to understand current state.\n\
- When exploring a codebase, start with directory listing, then targeted grep/find.\n\
- For destructive commands (rm, git push --force), warn the user first.\n\
- Do not hallucinate file contents or command outputs.\n\
- If a command fails, analyze the error before retrying.\n\
\n\
## Security\n\
- Never include secrets, API keys, or tokens in command output.\n\
- Do not force-push to main/master branches.\n\
- Do not execute commands that could cause data loss without confirmation.";

#[must_use]
pub fn build_system_prompt(
    skills_prompt: &str,
    env: Option<&EnvironmentContext>,
    tool_catalog: Option<&str>,
) -> String {
    let mut prompt = BASE_PROMPT.to_string();

    if let Some(env) = env {
        prompt.push_str("\n\n");
        prompt.push_str(&env.format());
    }

    if let Some(catalog) = tool_catalog
        && !catalog.is_empty()
    {
        prompt.push_str("\n\n");
        prompt.push_str(catalog);
    }

    if !skills_prompt.is_empty() {
        prompt.push_str("\n\n");
        prompt.push_str(skills_prompt);
    }

    prompt
}

#[derive(Debug, Clone)]
pub struct EnvironmentContext {
    pub working_dir: String,
    pub git_branch: Option<String>,
    pub os: String,
    pub model_name: String,
}

impl EnvironmentContext {
    #[must_use]
    pub fn gather(model_name: &str) -> Self {
        let working_dir =
            std::env::current_dir().map_or_else(|_| "unknown".into(), |p| p.display().to_string());

        let git_branch = std::process::Command::new("git")
            .args(["branch", "--show-current"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                } else {
                    None
                }
            });

        Self {
            working_dir,
            git_branch,
            os: std::env::consts::OS.into(),
            model_name: model_name.into(),
        }
    }

    #[must_use]
    pub fn format(&self) -> String {
        use std::fmt::Write;
        let mut out = String::from("<environment>\n");
        let _ = writeln!(out, "  working_directory: {}", self.working_dir);
        let _ = writeln!(out, "  os: {}", self.os);
        let _ = writeln!(out, "  model: {}", self.model_name);
        if let Some(ref branch) = self.git_branch {
            let _ = writeln!(out, "  git_branch: {branch}");
        }
        out.push_str("</environment>");
        out
    }
}

#[derive(Debug, Clone)]
pub struct BudgetAllocation {
    pub system_prompt: usize,
    pub skills: usize,
    pub summaries: usize,
    pub semantic_recall: usize,
    pub cross_session: usize,
    pub code_context: usize,
    pub recent_history: usize,
    pub response_reserve: usize,
}

#[derive(Debug, Clone)]
pub struct ContextBudget {
    max_tokens: usize,
    reserve_ratio: f32,
}

impl ContextBudget {
    #[must_use]
    pub fn new(max_tokens: usize, reserve_ratio: f32) -> Self {
        Self {
            max_tokens,
            reserve_ratio,
        }
    }

    #[must_use]
    pub fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    #[must_use]
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    pub fn allocate(&self, system_prompt: &str, skills_prompt: &str) -> BudgetAllocation {
        if self.max_tokens == 0 {
            return BudgetAllocation {
                system_prompt: 0,
                skills: 0,
                summaries: 0,
                semantic_recall: 0,
                cross_session: 0,
                code_context: 0,
                recent_history: 0,
                response_reserve: 0,
            };
        }

        let response_reserve = (self.max_tokens as f32 * self.reserve_ratio) as usize;
        let mut available = self.max_tokens.saturating_sub(response_reserve);

        let system_prompt_tokens = estimate_tokens(system_prompt);
        let skills_tokens = estimate_tokens(skills_prompt);

        available = available.saturating_sub(system_prompt_tokens + skills_tokens);

        let summaries = (available as f32 * 0.08) as usize;
        let semantic_recall = (available as f32 * 0.08) as usize;
        let cross_session = (available as f32 * 0.04) as usize;
        let code_context = (available as f32 * 0.30) as usize;
        let recent_history = (available as f32 * 0.50) as usize;

        BudgetAllocation {
            system_prompt: system_prompt_tokens,
            skills: skills_tokens,
            summaries,
            semantic_recall,
            cross_session,
            code_context,
            recent_history,
            response_reserve,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_skills() {
        let prompt = build_system_prompt("", None, None);
        assert!(prompt.starts_with("You are Zeph"));
        assert!(!prompt.contains("available_skills"));
    }

    #[test]
    fn with_skills() {
        let prompt = build_system_prompt("<available_skills>test</available_skills>", None, None);
        assert!(prompt.contains("You are Zeph"));
        assert!(prompt.contains("<available_skills>"));
    }

    #[test]
    fn estimate_tokens_basic() {
        assert_eq!(estimate_tokens("Hello world"), 2);
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("test"), 1);
    }

    #[test]
    fn context_budget_max_tokens_accessor() {
        let budget = ContextBudget::new(1000, 0.2);
        assert_eq!(budget.max_tokens(), 1000);
    }

    #[test]
    fn budget_allocation_basic() {
        let budget = ContextBudget::new(1000, 0.20);
        let system = "system prompt";
        let skills = "skills prompt";

        let alloc = budget.allocate(system, skills);

        assert_eq!(alloc.response_reserve, 200);
        assert!(alloc.system_prompt > 0);
        assert!(alloc.skills > 0);
        assert!(alloc.summaries > 0);
        assert!(alloc.semantic_recall > 0);
        assert!(alloc.cross_session > 0);
        assert!(alloc.recent_history > 0);
    }

    #[test]
    fn budget_allocation_reserve() {
        let budget = ContextBudget::new(1000, 0.30);
        let alloc = budget.allocate("", "");

        assert_eq!(alloc.response_reserve, 300);
    }

    #[test]
    fn budget_allocation_zero_disables() {
        let budget = ContextBudget::new(0, 0.20);
        let alloc = budget.allocate("test", "test");

        assert_eq!(alloc.system_prompt, 0);
        assert_eq!(alloc.skills, 0);
        assert_eq!(alloc.summaries, 0);
        assert_eq!(alloc.semantic_recall, 0);
        assert_eq!(alloc.cross_session, 0);
        assert_eq!(alloc.code_context, 0);
        assert_eq!(alloc.recent_history, 0);
        assert_eq!(alloc.response_reserve, 0);
    }

    #[test]
    fn budget_allocation_small_window() {
        let budget = ContextBudget::new(100, 0.20);
        let system = "very long system prompt that uses many tokens";
        let skills = "also a long skills prompt";

        let alloc = budget.allocate(system, skills);

        assert!(alloc.response_reserve > 0);
    }

    #[test]
    fn environment_context_gather() {
        let env = EnvironmentContext::gather("test-model");
        assert!(!env.working_dir.is_empty());
        assert_eq!(env.os, std::env::consts::OS);
        assert_eq!(env.model_name, "test-model");
    }

    #[test]
    fn environment_context_format() {
        let env = EnvironmentContext {
            working_dir: "/tmp/test".into(),
            git_branch: Some("main".into()),
            os: "macos".into(),
            model_name: "mistral:7b".into(),
        };
        let formatted = env.format();
        assert!(formatted.starts_with("<environment>"));
        assert!(formatted.ends_with("</environment>"));
        assert!(formatted.contains("working_directory: /tmp/test"));
        assert!(formatted.contains("os: macos"));
        assert!(formatted.contains("model: mistral:7b"));
        assert!(formatted.contains("git_branch: main"));
    }

    #[test]
    fn environment_context_format_no_git() {
        let env = EnvironmentContext {
            working_dir: "/tmp".into(),
            git_branch: None,
            os: "linux".into(),
            model_name: "test".into(),
        };
        let formatted = env.format();
        assert!(!formatted.contains("git_branch"));
    }

    #[test]
    fn build_system_prompt_with_env() {
        let env = EnvironmentContext {
            working_dir: "/tmp".into(),
            git_branch: None,
            os: "linux".into(),
            model_name: "test".into(),
        };
        let prompt = build_system_prompt("skills here", Some(&env), None);
        assert!(prompt.contains("You are Zeph"));
        assert!(prompt.contains("<environment>"));
        assert!(prompt.contains("skills here"));
    }

    #[test]
    fn build_system_prompt_without_env() {
        let prompt = build_system_prompt("skills here", None, None);
        assert!(prompt.contains("You are Zeph"));
        assert!(!prompt.contains("<environment>"));
        assert!(prompt.contains("skills here"));
    }

    #[test]
    fn base_prompt_contains_guidelines() {
        let prompt = build_system_prompt("", None, None);
        assert!(prompt.contains("## Tool Use"));
        assert!(prompt.contains("## Guidelines"));
        assert!(prompt.contains("## Security"));
    }

    #[test]
    fn budget_allocation_cross_session_percentage() {
        let budget = ContextBudget::new(10000, 0.20);
        let alloc = budget.allocate("", "");

        // cross_session = 4%, summaries = 8%, recall = 8%
        assert!(alloc.cross_session > 0);
        assert!(alloc.cross_session < alloc.summaries);
        assert_eq!(alloc.summaries, alloc.semantic_recall);
    }
}
