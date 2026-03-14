use minijinja::{Environment, UndefinedBehavior};
use serde::Serialize;
use thiserror::Error;

use crate::issue::Issue;
use crate::workflow::WorkflowDefinition;

const DEFAULT_PROMPT: &str = r#"You are working on a Linear issue.

Identifier: {{ issue.identifier }}
Title: {{ issue.title }}

Body:
{% if issue.description %}
{{ issue.description }}
{% else %}
No description provided.
{% endif %}"#;

#[derive(Debug, Error)]
pub enum PromptError {
    #[error("template_parse_error: {0}")]
    TemplateParse(String),
    #[error("template_render_error: {0}")]
    TemplateRender(String),
}

#[derive(Debug, Serialize)]
struct PromptContext<'a> {
    issue: &'a Issue,
    attempt: Option<u32>,
}

pub fn build_prompt(
    workflow: &WorkflowDefinition,
    issue: &Issue,
    attempt: Option<u32>,
) -> Result<String, PromptError> {
    let template_source = if workflow.prompt_template.trim().is_empty() {
        DEFAULT_PROMPT
    } else {
        workflow.prompt_template.as_str()
    };

    let mut environment = Environment::new();
    environment.set_undefined_behavior(UndefinedBehavior::Strict);
    environment
        .add_template("workflow", template_source)
        .map_err(|error| PromptError::TemplateParse(error.to_string()))?;

    let template = environment
        .get_template("workflow")
        .map_err(|error| PromptError::TemplateParse(error.to_string()))?;

    template
        .render(PromptContext { issue, attempt })
        .map_err(|error| PromptError::TemplateRender(error.to_string()))
}

pub fn continuation_prompt(current_turn: u32, max_turns: u32) -> String {
    let remaining = max_turns.saturating_sub(current_turn);
    format!(
        "Continuation guidance:\n\n\
         - The previous Codex turn completed normally, but the issue is still in an active state.\n\
         - This is turn {current_turn} of {max_turns} ({remaining} remaining).\n\
         - Resume from the current workspace state instead of restarting from scratch.\n\
         - The original task instructions and prior thread context are already present, so do not restate them before acting.\n\
         - Focus on the remaining ticket work and do not end the turn while the issue stays active unless you are truly blocked.\n"
    )
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;
    use crate::workflow::WorkflowDefinition;

    fn sample_issue() -> Issue {
        Issue {
            id: "issue_1".to_owned(),
            identifier: "ABC-1".to_owned(),
            title: "Test".to_owned(),
            description: Some("Description".to_owned()),
            priority: Some(1),
            state: "Todo".to_owned(),
            branch_name: None,
            url: None,
            assignee_id: None,
            labels: vec!["bug".to_owned()],
            blocked_by: vec![],
            assigned_to_worker: false,
            created_at: Some(Utc::now()),
            updated_at: Some(Utc::now()),
        }
    }

    #[test]
    fn renders_prompt() {
        let workflow = WorkflowDefinition {
            config: Default::default(),
            prompt_template: "Issue {{ issue.identifier }} / {{ attempt or 0 }}".to_owned(),
        };
        let prompt = build_prompt(&workflow, &sample_issue(), Some(2)).unwrap();
        assert_eq!(prompt, "Issue ABC-1 / 2");
    }

    #[test]
    fn strict_missing_variables_fail() {
        let workflow = WorkflowDefinition {
            config: Default::default(),
            prompt_template: "Issue {{ missing }}".to_owned(),
        };
        let error = build_prompt(&workflow, &sample_issue(), None).unwrap_err();
        assert!(matches!(error, PromptError::TemplateRender(_)));
    }

    #[test]
    fn continuation_prompt_includes_turn_info() {
        let prompt = continuation_prompt(2, 5);
        assert!(prompt.contains("turn 2 of 5"), "should include current/max turns");
        assert!(prompt.contains("3 remaining"), "should include remaining turns");
    }

    #[test]
    fn continuation_prompt_last_turn_shows_zero_remaining() {
        let prompt = continuation_prompt(5, 5);
        assert!(prompt.contains("turn 5 of 5"), "should show final turn");
        assert!(prompt.contains("0 remaining"), "should show zero remaining");
    }
}
