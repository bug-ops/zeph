use zeph_memory::semantic::estimate_tokens;

const BASE_PROMPT: &str = "You are Zeph, a helpful assistant. \
When you need to perform actions, write bash commands in fenced code blocks with the `bash` language tag. \
The commands will be executed automatically and the output will be provided back to you.";

#[must_use]
pub fn build_system_prompt(skills_prompt: &str) -> String {
    if skills_prompt.is_empty() {
        return BASE_PROMPT.to_string();
    }
    format!("{BASE_PROMPT}\n\n{skills_prompt}")
}

#[derive(Debug, Clone)]
pub struct BudgetAllocation {
    pub system_prompt: usize,
    pub skills: usize,
    pub summaries: usize,
    pub semantic_recall: usize,
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
                recent_history: 0,
                response_reserve: 0,
            };
        }

        let response_reserve = (self.max_tokens as f32 * self.reserve_ratio) as usize;
        let mut available = self.max_tokens.saturating_sub(response_reserve);

        let system_prompt_tokens = estimate_tokens(system_prompt);
        let skills_tokens = estimate_tokens(skills_prompt);

        available = available.saturating_sub(system_prompt_tokens + skills_tokens);

        let summaries = (available as f32 * 0.15) as usize;
        let semantic_recall = (available as f32 * 0.25) as usize;
        let recent_history = (available as f32 * 0.60) as usize;

        BudgetAllocation {
            system_prompt: system_prompt_tokens,
            skills: skills_tokens,
            summaries,
            semantic_recall,
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
        let prompt = build_system_prompt("");
        assert!(prompt.starts_with("You are Zeph"));
        assert!(!prompt.contains("available_skills"));
    }

    #[test]
    fn with_skills() {
        let prompt = build_system_prompt("<available_skills>test</available_skills>");
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
}
