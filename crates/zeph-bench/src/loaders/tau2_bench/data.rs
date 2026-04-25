// SPDX-FileCopyrightText: 2026 Andrei G <bug-ops>
// SPDX-License-Identifier: MIT OR Apache-2.0

//! Data model matching the tau2-bench JSON schema.
//!
//! All types derive `Deserialize` and map directly onto the upstream
//! `data/tau2/domains/<domain>/tasks.json` format.

use serde::{Deserialize, Serialize};

/// tau2-bench domain selector for routing loader and env construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Domain {
    /// Customer-service retail domain.
    Retail,
    /// Flight-reservation airline domain.
    Airline,
}

impl Domain {
    /// Short lowercase identifier used in file paths and scenario IDs.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Retail => "retail",
            Self::Airline => "airline",
        }
    }
}

/// One task from tau2-bench `tasks.json`.
///
/// Unused upstream fields (`description`, `ticket`, `initial_state`, `annotations`)
/// are collected by `_rest` and silently discarded so we don't fail on schema evolution.
#[derive(Debug, Clone, Deserialize)]
pub struct Task {
    /// Unique task identifier within the domain (e.g. `"0"`, `"retail_1"`).
    pub id: String,
    /// User-facing scenario: instructions + optional persona.
    pub user_scenario: UserScenario,
    /// Expected tool calls and reward criteria.
    pub evaluation_criteria: Option<EvaluationCriteria>,
    /// Forward-compat catch-all for fields not modelled here.
    #[serde(flatten)]
    _rest: serde_json::Map<String, serde_json::Value>,
}

/// Wraps the user instructions for a scenario.
#[derive(Debug, Clone, Deserialize)]
pub struct UserScenario {
    /// Instructions given to the user simulator (or a plain string prompt).
    pub instructions: UserInstructions,
    /// Optional persona text.
    #[serde(default)]
    pub persona: Option<String>,
}

/// Instructions are either a structured object or a plain string.
///
/// The upstream schema uses structured objects for most tasks, but older
/// or synthetic tasks may provide a plain string.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum UserInstructions {
    /// Structured object with individual fields.
    Structured(StructuredUserInstructions),
    /// Legacy or synthetic plain-string prompt.
    Plain(String),
}

/// Structured form of the user instructions.
#[derive(Debug, Clone, Deserialize)]
pub struct StructuredUserInstructions {
    /// Domain identifier string (e.g. `"retail"`).
    pub domain: String,
    /// Why the user is calling customer support.
    pub reason_for_call: String,
    /// Step-by-step instructions for the user simulator.
    pub task_instructions: String,
    /// Information the user already knows (injected into prompt).
    #[serde(default)]
    pub known_info: Option<String>,
    /// Information the user deliberately hides (not injected into prompt).
    #[serde(default)]
    pub unknown_info: Option<String>,
}

/// Expected actions and reward policy for a task.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EvaluationCriteria {
    /// Gold tool calls the agent must make (in any order).
    #[serde(default)]
    pub actions: Vec<Action>,
    /// Reward components required for full credit (e.g. `["ACTION"]`, `["DB", "ACTION"]`).
    #[serde(default)]
    pub reward_basis: Vec<String>,
}

/// One expected tool call from the upstream `Action` data model.
///
/// Scoring uses `Action.compare_with_tool_call` semantics:
/// - If `compare_args` is `None`, all argument keys are compared.
/// - If `compare_args` is `Some([])`, only the tool name is checked.
/// - If `compare_args` is `Some(keys)`, only those keys are compared.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Action {
    /// Unique identifier for this action within the task.
    pub action_id: String,
    /// Who performs this action — `"assistant"` or `"user"`.
    #[serde(default = "default_requestor")]
    pub requestor: String,
    /// Tool name (must match an available tool exactly).
    pub name: String,
    /// Expected arguments to the tool call.
    #[serde(default)]
    pub arguments: serde_json::Map<String, serde_json::Value>,
    /// Argument keys to compare, or `None` for all keys.
    #[serde(default)]
    pub compare_args: Option<Vec<String>>,
    /// Optional human-readable description of the action.
    #[serde(default)]
    pub info: Option<String>,
}

fn default_requestor() -> String {
    "assistant".to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const RETAIL_FIXTURE: &str = r##"[
      {
        "id": "0",
        "user_scenario": {
          "instructions": {
            "domain": "retail",
            "reason_for_call": "I need to cancel an order",
            "task_instructions": "Cancel order #W1234567",
            "known_info": "Order id: #W1234567"
          },
          "persona": "Impatient customer"
        },
        "evaluation_criteria": {
          "actions": [
            {
              "action_id": "a1",
              "requestor": "assistant",
              "name": "cancel_pending_order",
              "arguments": {"order_id": "#W1234567", "reason": "no_longer_needed"},
              "compare_args": ["order_id", "reason"]
            }
          ],
          "reward_basis": ["ACTION"]
        }
      },
      {
        "id": "1",
        "user_scenario": {
          "instructions": "Plain string instructions for a simple task"
        },
        "evaluation_criteria": {
          "actions": [],
          "reward_basis": ["ACTION"]
        }
      }
    ]"##;

    #[test]
    fn parse_structured_instructions() {
        let tasks: Vec<Task> = serde_json::from_str(RETAIL_FIXTURE).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, "0");
        match &tasks[0].user_scenario.instructions {
            UserInstructions::Structured(s) => {
                assert_eq!(s.domain, "retail");
                assert_eq!(s.reason_for_call, "I need to cancel an order");
                assert!(s.known_info.is_some());
            }
            UserInstructions::Plain(_) => panic!("expected structured"),
        }
    }

    #[test]
    fn parse_plain_instructions() {
        let tasks: Vec<Task> = serde_json::from_str(RETAIL_FIXTURE).unwrap();
        match &tasks[1].user_scenario.instructions {
            UserInstructions::Plain(s) => assert!(s.contains("Plain string")),
            UserInstructions::Structured(_) => panic!("expected plain"),
        }
    }

    #[test]
    fn parse_evaluation_criteria() {
        let tasks: Vec<Task> = serde_json::from_str(RETAIL_FIXTURE).unwrap();
        let criteria = tasks[0].evaluation_criteria.as_ref().unwrap();
        assert_eq!(criteria.actions.len(), 1);
        assert_eq!(criteria.actions[0].name, "cancel_pending_order");
        assert_eq!(
            criteria.actions[0].compare_args,
            Some(vec!["order_id".to_owned(), "reason".to_owned()])
        );
    }

    #[test]
    fn metadata_roundtrip() {
        let tasks: Vec<Task> = serde_json::from_str(RETAIL_FIXTURE).unwrap();
        let criteria = tasks[0].evaluation_criteria.as_ref().unwrap();
        let value = serde_json::to_value(criteria).unwrap();
        let back: EvaluationCriteria = serde_json::from_value(value).unwrap();
        assert_eq!(back.actions.len(), 1);
        assert_eq!(back.actions[0].name, "cancel_pending_order");
    }

    #[test]
    fn domain_as_str() {
        assert_eq!(Domain::Retail.as_str(), "retail");
        assert_eq!(Domain::Airline.as_str(), "airline");
    }
}
