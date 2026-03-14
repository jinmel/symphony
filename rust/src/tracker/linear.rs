use std::collections::HashMap;

use chrono::{DateTime, Utc};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use serde_json::{Value, json};

use crate::config::{EffectiveConfig, normalize_issue_state};
use crate::error::truncate_for_log;
use crate::issue::{BlockerRef, Issue};

use super::TrackerError;

const ISSUE_PAGE_SIZE: usize = 50;

const QUERY_CANDIDATES: &str = r#"
query SymphonyLinearPoll($projectSlug: String!, $stateNames: [String!]!, $first: Int!, $relationFirst: Int!, $after: String) {
  issues(filter: {project: {slugId: {eq: $projectSlug}}, state: {name: {in: $stateNames}}}, first: $first, after: $after) {
    nodes {
      id
      identifier
      title
      description
      priority
      state { name }
      branchName
      url
      assignee { id }
      labels { nodes { name } }
      inverseRelations(first: $relationFirst) {
        nodes {
          type
          issue {
            id
            identifier
            state { name }
          }
        }
      }
      createdAt
      updatedAt
    }
    pageInfo {
      hasNextPage
      endCursor
    }
  }
}
"#;

const QUERY_BY_IDS: &str = r#"
query SymphonyLinearIssuesById($ids: [ID!]!, $first: Int!, $relationFirst: Int!) {
  issues(filter: {id: {in: $ids}}, first: $first) {
    nodes {
      id
      identifier
      title
      description
      priority
      state { name }
      branchName
      url
      assignee { id }
      labels { nodes { name } }
      inverseRelations(first: $relationFirst) {
        nodes {
          type
          issue {
            id
            identifier
            state { name }
          }
        }
      }
      createdAt
      updatedAt
    }
  }
}
"#;

const QUERY_VIEWER: &str = r#"
query SymphonyLinearViewer {
  viewer {
    id
  }
}
"#;

#[derive(Debug, Clone)]
pub struct LinearTrackerClient {
    http: reqwest::Client,
}

impl Default for LinearTrackerClient {
    fn default() -> Self {
        Self::new()
    }
}

impl LinearTrackerClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client should build"),
        }
    }

    /// Fetch the current viewer's user ID via the `{ viewer { id } }` query.
    /// Used to resolve `assignee: "me"` to an actual Linear user ID.
    pub async fn fetch_viewer_id(
        &self,
        config: &EffectiveConfig,
    ) -> Result<String, TrackerError> {
        let body = self
            .graphql(config, QUERY_VIEWER, json!({}))
            .await?;

        body.pointer("/data/viewer/id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .map(ToOwned::to_owned)
            .ok_or(TrackerError::LinearUnknownPayload)
    }

    /// Resolve the configured assignee to an actual user ID.
    /// - `None` config => no filter (returns `Ok(None)`)
    /// - `"me"` => calls `fetch_viewer_id` to get the current user's ID
    /// - anything else => used as a literal user ID
    pub async fn resolve_assignee_filter(
        &self,
        config: &EffectiveConfig,
    ) -> Result<Option<String>, TrackerError> {
        match config.tracker.assignee.as_deref() {
            None => Ok(None),
            Some(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    Ok(None)
                } else if trimmed.eq_ignore_ascii_case("me") {
                    let viewer_id = self.fetch_viewer_id(config).await?;
                    Ok(Some(viewer_id))
                } else {
                    Ok(Some(trimmed.to_owned()))
                }
            }
        }
    }

    pub async fn fetch_candidate_issues(
        &self,
        config: &EffectiveConfig,
    ) -> Result<Vec<Issue>, TrackerError> {
        let project_slug = config
            .tracker
            .project_slug
            .as_deref()
            .ok_or(TrackerError::MissingTrackerProjectSlug)?;
        let resolved_assignee = self.resolve_assignee_filter(config).await?;
        let mut after: Option<String> = None;
        let mut issues = Vec::new();

        loop {
            let body = self
                .graphql(
                    config,
                    QUERY_CANDIDATES,
                    json!({
                        "projectSlug": project_slug,
                        "stateNames": config.tracker.active_states,
                        "first": ISSUE_PAGE_SIZE,
                        "relationFirst": ISSUE_PAGE_SIZE,
                        "after": after,
                    }),
                )
                .await?;

            let issue_nodes = body
                .pointer("/data/issues/nodes")
                .and_then(Value::as_array)
                .ok_or(TrackerError::LinearUnknownPayload)?;

            issues.extend(
                issue_nodes
                    .iter()
                    .filter_map(|v| normalize_issue(v, resolved_assignee.as_deref())),
            );

            let page_info = body
                .pointer("/data/issues/pageInfo")
                .and_then(Value::as_object)
                .ok_or(TrackerError::LinearUnknownPayload)?;

            let has_next_page = page_info
                .get("hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            if has_next_page {
                let next = page_info
                    .get("endCursor")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or(TrackerError::LinearMissingEndCursor)?;
                after = Some(next.to_owned());
            } else {
                break;
            }
        }

        Ok(issues)
    }

    pub async fn fetch_issues_by_states(
        &self,
        config: &EffectiveConfig,
        states: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        if states.is_empty() {
            return Ok(Vec::new());
        }

        let project_slug = config
            .tracker
            .project_slug
            .as_deref()
            .ok_or(TrackerError::MissingTrackerProjectSlug)?;

        let mut after: Option<String> = None;
        let mut issues = Vec::new();

        loop {
            let body = self
                .graphql(
                    config,
                    QUERY_CANDIDATES,
                    json!({
                        "projectSlug": project_slug,
                        "stateNames": states,
                        "first": ISSUE_PAGE_SIZE,
                        "relationFirst": ISSUE_PAGE_SIZE,
                        "after": after,
                    }),
                )
                .await?;

            let issue_nodes = body
                .pointer("/data/issues/nodes")
                .and_then(Value::as_array)
                .ok_or(TrackerError::LinearUnknownPayload)?;

            // No assignee filter for state-based queries (used for terminal cleanup)
            issues.extend(
                issue_nodes
                    .iter()
                    .filter_map(|v| normalize_issue(v, None)),
            );

            let page_info = body
                .pointer("/data/issues/pageInfo")
                .and_then(Value::as_object)
                .ok_or(TrackerError::LinearUnknownPayload)?;

            let has_next_page = page_info
                .get("hasNextPage")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            if has_next_page {
                let next = page_info
                    .get("endCursor")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or(TrackerError::LinearMissingEndCursor)?;
                after = Some(next.to_owned());
            } else {
                break;
            }
        }

        Ok(issues)
    }

    pub async fn fetch_issue_states_by_ids(
        &self,
        config: &EffectiveConfig,
        ids: &[String],
    ) -> Result<Vec<Issue>, TrackerError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let resolved_assignee = self.resolve_assignee_filter(config).await?;
        let mut issues = Vec::new();
        for batch in ids.chunks(ISSUE_PAGE_SIZE) {
            let body = self
                .graphql(
                    config,
                    QUERY_BY_IDS,
                    json!({
                        "ids": batch,
                        "first": batch.len(),
                        "relationFirst": ISSUE_PAGE_SIZE,
                    }),
                )
                .await?;

            let nodes = body
                .pointer("/data/issues/nodes")
                .and_then(Value::as_array)
                .ok_or(TrackerError::LinearUnknownPayload)?;
            issues.extend(
                nodes
                    .iter()
                    .filter_map(|v| normalize_issue(v, resolved_assignee.as_deref())),
            );
        }

        let order_index: HashMap<&str, usize> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| (id.as_str(), index))
            .collect();
        issues.sort_by_key(|issue| {
            order_index
                .get(issue.id.as_str())
                .copied()
                .unwrap_or(usize::MAX)
        });
        Ok(issues)
    }

    async fn graphql(
        &self,
        config: &EffectiveConfig,
        query: &str,
        variables: Value,
    ) -> Result<Value, TrackerError> {
        let token = config
            .tracker
            .api_key
            .as_deref()
            .ok_or(TrackerError::MissingTrackerApiKey)?;

        let response = self
            .http
            .post(&config.tracker.endpoint)
            .header(AUTHORIZATION, token)
            .header(CONTENT_TYPE, "application/json")
            .json(&json!({ "query": query, "variables": variables }))
            .send()
            .await
            .map_err(|error| TrackerError::LinearApiRequest(error.to_string()))?;

        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|error| TrackerError::LinearApiRequest(error.to_string()))?;

        if !status.is_success() {
            tracing::error!(
                status = status.as_u16(),
                body = %truncate_for_log(&body.to_string(), 1_000),
                "linear graphql request failed"
            );
            return Err(TrackerError::LinearApiStatus(status.as_u16()));
        }

        if let Some(errors) = body.get("errors") {
            return Err(TrackerError::LinearGraphqlErrors(errors.to_string()));
        }

        Ok(body)
    }
}

/// Execute a raw GraphQL query against the Linear API.
///
/// Unlike [`LinearTrackerClient::graphql`], this function intentionally does
/// *not* treat GraphQL-level `errors` as a hard failure — callers (e.g. the
/// `linear_graphql` dynamic tool) need the full response body so they can
/// surface errors to the agent.
pub async fn execute_raw_graphql(
    endpoint: &str,
    api_key: &str,
    query: &str,
    variables: Value,
) -> Result<Value, TrackerError> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|error| TrackerError::LinearApiRequest(error.to_string()))?;

    let response = http
        .post(endpoint)
        .header(AUTHORIZATION, api_key)
        .header(CONTENT_TYPE, "application/json")
        .json(&json!({ "query": query, "variables": variables }))
        .send()
        .await
        .map_err(|error| TrackerError::LinearApiRequest(error.to_string()))?;

    let status = response.status();
    let body: Value = response
        .json()
        .await
        .map_err(|error| TrackerError::LinearApiRequest(error.to_string()))?;

    if !status.is_success() {
        tracing::error!(
            status = status.as_u16(),
            body = %truncate_for_log(&body.to_string(), 1_000),
            "linear raw graphql request failed"
        );
        return Err(TrackerError::LinearApiStatus(status.as_u16()));
    }

    Ok(body)
}

fn normalize_issue(value: &Value, resolved_assignee_id: Option<&str>) -> Option<Issue> {
    let object = value.as_object()?;
    let assignee_id = object
        .get("assignee")
        .and_then(|a| a.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    let assigned_to_worker = match resolved_assignee_id {
        None => true,
        Some(filter_id) => assignee_id
            .as_deref()
            .map(|id| id == filter_id)
            .unwrap_or(false),
    };

    Some(Issue {
        id: object.get("id")?.as_str()?.to_owned(),
        identifier: object.get("identifier")?.as_str()?.to_owned(),
        title: object.get("title")?.as_str()?.to_owned(),
        description: object
            .get("description")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        priority: object.get("priority").and_then(Value::as_i64),
        state: object.get("state")?.get("name")?.as_str()?.to_owned(),
        branch_name: object
            .get("branchName")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        url: object
            .get("url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        labels: object
            .get("labels")
            .and_then(|labels| labels.get("nodes"))
            .and_then(Value::as_array)
            .map(|labels| {
                labels
                    .iter()
                    .filter_map(|label| label.get("name").and_then(Value::as_str))
                    .map(|label| label.to_ascii_lowercase())
                    .collect()
            })
            .unwrap_or_default(),
        assignee_id,
        blocked_by: extract_blockers(object.get("inverseRelations")),
        assigned_to_worker,
        created_at: parse_datetime(object.get("createdAt")),
        updated_at: parse_datetime(object.get("updatedAt")),
    })
}

fn extract_blockers(value: Option<&Value>) -> Vec<BlockerRef> {
    value
        .and_then(|relations| relations.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|relation| {
            let relation_type = relation.get("type")?.as_str()?;
            if normalize_issue_state(relation_type) != "blocks" {
                return None;
            }
            let issue = relation.get("issue")?;
            Some(BlockerRef {
                id: issue
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                identifier: issue
                    .get("identifier")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                state: issue
                    .get("state")
                    .and_then(|state| state.get("name"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            })
        })
        .collect()
}

fn parse_datetime(value: Option<&Value>) -> Option<DateTime<Utc>> {
    let raw = value?.as_str()?;
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|value| value.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_labels_and_blockers() {
        let issue = normalize_issue(&json!({
            "id": "1",
            "identifier": "ABC-1",
            "title": "Test",
            "description": null,
            "priority": 2,
            "state": { "name": "Todo" },
            "branchName": null,
            "url": null,
            "labels": { "nodes": [{ "name": "Bug" }] },
            "inverseRelations": {
                "nodes": [
                    {
                        "type": "blocks",
                        "issue": {
                            "id": "2",
                            "identifier": "ABC-2",
                            "state": { "name": "In Progress" }
                        }
                    }
                ]
            },
            "createdAt": "2026-03-14T00:00:00Z",
            "updatedAt": "2026-03-14T00:00:00Z"
        }), None)
        .unwrap();

        assert_eq!(issue.labels, vec!["bug"]);
        assert_eq!(issue.blocked_by.len(), 1);
        assert!(issue.assigned_to_worker);
        assert_eq!(issue.assignee_id, None);
    }

    #[test]
    fn assignee_filtering_matches_correct_user() {
        let issue_json = json!({
            "id": "1",
            "identifier": "ABC-1",
            "title": "Test",
            "description": null,
            "priority": 2,
            "state": { "name": "Todo" },
            "branchName": null,
            "url": null,
            "assignee": { "id": "user-123" },
            "labels": { "nodes": [] },
            "inverseRelations": { "nodes": [] },
            "createdAt": "2026-03-14T00:00:00Z",
            "updatedAt": "2026-03-14T00:00:00Z"
        });

        // No filter => assigned_to_worker is true
        let issue = normalize_issue(&issue_json, None).unwrap();
        assert!(issue.assigned_to_worker);
        assert_eq!(issue.assignee_id.as_deref(), Some("user-123"));

        // Matching filter => assigned_to_worker is true
        let issue = normalize_issue(&issue_json, Some("user-123")).unwrap();
        assert!(issue.assigned_to_worker);

        // Non-matching filter => assigned_to_worker is false
        let issue = normalize_issue(&issue_json, Some("user-999")).unwrap();
        assert!(!issue.assigned_to_worker);
    }

    #[test]
    fn assignee_filtering_unassigned_issue() {
        let issue_json = json!({
            "id": "1",
            "identifier": "ABC-1",
            "title": "Test",
            "description": null,
            "priority": 2,
            "state": { "name": "Todo" },
            "branchName": null,
            "url": null,
            "assignee": null,
            "labels": { "nodes": [] },
            "inverseRelations": { "nodes": [] },
            "createdAt": "2026-03-14T00:00:00Z",
            "updatedAt": "2026-03-14T00:00:00Z"
        });

        // No filter => assigned_to_worker is true
        let issue = normalize_issue(&issue_json, None).unwrap();
        assert!(issue.assigned_to_worker);

        // With filter but no assignee => not assigned to worker
        let issue = normalize_issue(&issue_json, Some("user-123")).unwrap();
        assert!(!issue.assigned_to_worker);
    }
}
