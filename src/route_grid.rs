//! Route-owned selection and recovery. Model references are concrete
//! `connection_id/upstream_model_id` destinations, never route aliases.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub type FallbackMap = BTreeMap<String, Vec<String>>;
pub const MAX_ALTERNATIVES: usize = 16;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteGrid {
    #[serde(default)]
    pub fallbacks: FallbackMap,
    #[serde(default)]
    pub judge: Option<RouteJudge>,
    pub default_target: RouteTarget,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeMode {
    #[default]
    Off,
    Shadow,
    Live,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Classification {
    #[default]
    Task,
    Difficulty,
}

impl Classification {
    /// Starter labels and criteria shared by admin clients and remote judges.
    /// These do not assign models or change an existing route's outcome map.
    pub fn default_outcomes(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Difficulty => &[
                ("0", "Straightforward work with a clear goal and sufficient information: greetings, simple factual questions, short rewrites, direct summaries or extraction, and small self-contained code changes. Choose this when the answer needs little inference and can be produced through direct retrieval, transformation, or a few routine steps. Complexity depends on the work required, not the topic alone."),
                ("1", "Work requiring sustained multi-step reasoning, resolving ambiguity, combining multiple constraints or sources, nontrivial mathematics, diagnosing subtle bugs, or designing systems. Choose this when correctness depends on careful analysis, planning, or synthesis beyond routine steps. A short request can still be complex; when uncertain between the two difficulty levels, choose this one."),
            ],
            Self::Task => &[
                ("coding", "The primary deliverable is writing, modifying, reviewing, debugging, or explaining code, tests, queries, scripts, or technical configuration. Includes software design and implementation guidance. Merely summarizing a document about software belongs in summarization; copying specified fields or code verbatim from a source belongs in extraction."),
                ("summarization", "The primary deliverable is a shorter, faithful account of supplied material: a summary, recap, digest, key points, or meeting notes. Preserve the source's meaning and prioritize its important information. Choose extraction instead when the user asks for specific fields, entities, or records rather than an overall account."),
                ("extraction", "The primary deliverable is locating and returning specified information from supplied material: facts, entities, dates, fields, records, or verbatim passages, including JSON or tables. The requested information should come from the source rather than be invented. Choose summarization for an overall condensed account and coding for creating or modifying code."),
                ("other", "The primary deliverable does not fit coding, summarization, or extraction. Includes general questions, conversation, original writing, translation, brainstorming, planning, and reasoning unrelated to software. For mixed requests, choose the category of the main requested deliverable; use other when none of the specific categories is a clear fit."),
            ],
        }
    }
}

/// Explicit connection identity and upstream model, never a route alias.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteTarget {
    pub connection_id: String,
    pub upstream_model_id: String,
}

impl RouteTarget {
    pub fn concrete_model(&self) -> String {
        target(&self.connection_id, &self.upstream_model_id)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.connection_id.contains('/')
            || self.connection_id.trim() != self.connection_id
            || self.upstream_model_id.trim() != self.upstream_model_id
        {
            return Err("target requires a concrete connection ID and upstream model ID".into());
        }
        split_target(&self.concrete_model()).map(|_| ())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct HeuristicJudgeBackend {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub use_default_keywords: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub added_hard_keywords: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed_hard_keywords: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_blocks_are_hard: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_easy_input_bytes: Option<u32>,
}

impl HeuristicJudgeBackend {
    pub fn validate(&self) -> Result<(), String> {
        if self.added_hard_keywords.len() + self.removed_hard_keywords.len() > 256 {
            return Err("heuristic keyword overrides must contain at most 256 entries".into());
        }
        for keyword in self
            .added_hard_keywords
            .iter()
            .chain(&self.removed_hard_keywords)
        {
            if keyword.trim().is_empty() || keyword.len() > 256 {
                return Err("heuristic keywords must be non-blank and at most 256 bytes".into());
            }
        }
        let removed: BTreeSet<String> = self
            .removed_hard_keywords
            .iter()
            .map(|keyword| keyword.to_lowercase())
            .collect();
        if self
            .added_hard_keywords
            .iter()
            .any(|keyword| removed.contains(&keyword.to_lowercase()))
        {
            return Err("a heuristic keyword cannot be both added and removed".into());
        }
        Ok(())
    }
}

pub const DEFAULT_JUDGE_TIMEOUT_MS: u64 = 1_000;

fn default_judge_timeout_ms() -> u64 {
    DEFAULT_JUDGE_TIMEOUT_MS
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompletionJudgeBackend {
    pub target: RouteTarget,
    /// Zero or omission uses the default of 1000 milliseconds.
    #[serde(default = "default_judge_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for CompletionJudgeBackend {
    fn default() -> Self {
        Self {
            target: RouteTarget::default(),
            timeout_ms: DEFAULT_JUDGE_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionsJudgeBackend {
    pub target: RouteTarget,
    /// Zero or omission uses the default of 1000 milliseconds.
    #[serde(default = "default_judge_timeout_ms")]
    pub timeout_ms: u64,
}

impl Default for DecisionsJudgeBackend {
    fn default() -> Self {
        Self {
            target: RouteTarget::default(),
            timeout_ms: DEFAULT_JUDGE_TIMEOUT_MS,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RouteJudgeBackend {
    Heuristic(HeuristicJudgeBackend),
    Completion(CompletionJudgeBackend),
    Decisions(DecisionsJudgeBackend),
}

impl Default for RouteJudgeBackend {
    fn default() -> Self {
        Self::Heuristic(HeuristicJudgeBackend::default())
    }
}

impl RouteJudgeBackend {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Heuristic(_) => "heuristic",
            Self::Completion(_) => "completion",
            Self::Decisions(_) => "decisions",
        }
    }

    pub fn target(&self) -> Option<&RouteTarget> {
        match self {
            Self::Heuristic(_) => None,
            Self::Completion(config) => Some(&config.target),
            Self::Decisions(config) => Some(&config.target),
        }
    }

    pub fn timeout_ms(&self) -> Option<u64> {
        let timeout = match self {
            Self::Heuristic(_) => return None,
            Self::Completion(config) => config.timeout_ms,
            Self::Decisions(config) => config.timeout_ms,
        };
        Some(if timeout == 0 {
            DEFAULT_JUDGE_TIMEOUT_MS
        } else {
            timeout
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouteJudge {
    pub mode: JudgeMode,
    pub classification: Classification,
    pub backend: RouteJudgeBackend,
    pub outcomes: BTreeMap<String, JudgeOutcomeConfig>, // classification bucket -> (model, description, fallbacks)
}

impl Default for RouteJudge {
    fn default() -> Self {
        Self {
            mode: JudgeMode::Off,
            classification: Classification::Task,
            backend: RouteJudgeBackend::default(),
            outcomes: BTreeMap::new(),
        }
    }
}

impl RouteJudge {
    /// Blank descriptions for standard buckets use backend criteria. Never
    /// apply binary difficulty criteria to a user-defined multi-level scale.
    pub fn outcome_description<'a>(&self, label: &str, outcome: &'a JudgeOutcomeConfig) -> &'a str {
        if !outcome.description.trim().is_empty()
            || (self.classification == Classification::Difficulty && self.outcomes.len() != 2)
        {
            return &outcome.description;
        }
        self.classification
            .default_outcomes()
            .iter()
            .find_map(|(default_label, description)| {
                (*default_label == label).then_some(*description)
            })
            .unwrap_or(&outcome.description)
    }
}

/// Configured routing action for one judge classification label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeOutcomeConfig {
    pub model: String,
    #[serde(default)]
    pub description: String,
    /// None inherits the route's model entry; Some([]) explicitly stops.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallbacks: Option<Vec<String>>,
}

pub fn target(connection: &str, model: &str) -> String {
    format!("{connection}/{model}")
}

pub fn split_target(model: &str) -> Result<(&str, &str), String> {
    let (connection, upstream) = model
        .split_once('/')
        .ok_or("model must be connection_id/upstream_model_id")?;
    if connection.is_empty()
        || connection == "chalk"
        || upstream.is_empty()
        || model.trim() != model
        || model.len() > 768
        || upstream.starts_with("chalk/")
    {
        return Err(
            "model must identify a concrete connection and upstream model, not a route alias"
                .into(),
        );
    }
    Ok((connection, upstream))
}

pub fn validate_chain(model: &str, alternatives: &[String]) -> Result<(), String> {
    split_target(model)?;
    if alternatives.len() > MAX_ALTERNATIVES {
        return Err(format!(
            "at most {MAX_ALTERNATIVES} fallback alternatives are allowed"
        ));
    }
    let mut seen = BTreeSet::from([model]);
    for alternative in alternatives {
        split_target(alternative)?;
        if !seen.insert(alternative.as_str()) {
            return Err("an attempt list must not repeat a model".into());
        }
    }
    Ok(())
}

pub fn validate_fallbacks(map: &FallbackMap) -> Result<(), String> {
    if map.len() > 128 {
        return Err("at most 128 model fallback entries are allowed".into());
    }
    for (model, alternatives) in map {
        validate_chain(model, alternatives)?;
    }
    Ok(())
}

impl RouteGrid {
    /// Stable identity for the executable routing policy. Judge mode is an
    /// operational rollout control, so promoting an otherwise-identical policy
    /// from shadow to live keeps the same revision.
    pub fn policy_revision(&self) -> String {
        let mut canonical = self.clone();
        if let Some(judge) = &mut canonical.judge {
            judge.mode = JudgeMode::Off;
        }
        let encoded = serde_json::to_vec(&canonical)
            .expect("route grid contains only JSON-serializable fields");
        let mut digest = Sha256::new();
        digest.update(b"chalk-route-grid-policy-v1\0");
        digest.update(encoded);
        format!("sha256:{:x}", digest.finalize())
    }

    /// Exact connection identity, independent of names or public prefixes.
    pub fn references_connection(&self, id: &str) -> bool {
        let uses = |model: &str| split_target(model).is_ok_and(|(connection, _)| connection == id);
        self.default_target.connection_id == id
            || self
                .fallbacks
                .iter()
                .any(|(model, alternatives)| uses(model) || alternatives.iter().any(|m| uses(m)))
            || self.judge.as_ref().is_some_and(|judge| {
                judge
                    .backend
                    .target()
                    .is_some_and(|target| target.connection_id == id)
                    || judge
                        .outcomes
                        .values()
                        .any(|o| uses(&o.model) || o.fallbacks.iter().flatten().any(|m| uses(m)))
            })
    }

    pub fn validate(&self) -> Result<(), String> {
        self.default_target.validate()?;
        validate_fallbacks(&self.fallbacks)?;
        if let Some(judge) = &self.judge {
            if let RouteJudgeBackend::Heuristic(config) = &judge.backend {
                config.validate()?;
            }
            if !(1..=32).contains(&judge.outcomes.len()) {
                return Err("a judge needs 1–32 configured outcomes".into());
            }
            if judge
                .backend
                .timeout_ms()
                .is_some_and(|timeout| !(1..=30_000).contains(&timeout))
            {
                return Err("judge timeout must be 1–30000 milliseconds".into());
            }
            if let Some(target) = judge.backend.target() {
                target.validate()?;
            } else if judge.classification != Classification::Difficulty
                || judge.outcomes.len() != 2
            {
                return Err("the heuristic requires difficulty outcomes 0 and 1".into());
            }
            for (label, outcome) in &judge.outcomes {
                if label.is_empty()
                    || label.trim() != label
                    || label.len() > 128
                    || outcome.description.len() > 2048
                {
                    return Err("invalid judge outcome label or description".into());
                }
                validate_chain(&outcome.model, outcome.fallbacks.as_deref().unwrap_or(&[]))?;
            }
            if judge.classification == Classification::Difficulty
                && (judge.outcomes.len() < 2
                    || (0..judge.outcomes.len())
                        .any(|i| !judge.outcomes.contains_key(&i.to_string())))
            {
                return Err(
                    "difficulty outcomes must be consecutive integers starting at 0".into(),
                );
            }
        }
        Ok(())
    }

    /// Exactly one lookup at each level. Never expand another model's chain.
    pub fn alternatives(
        &self,
        selected: &str,
        outcome: Option<&JudgeOutcomeConfig>,
        overrides: &FallbackMap,
    ) -> Vec<String> {
        overrides
            .get(selected)
            .or_else(|| outcome.and_then(|o| o.fallbacks.as_ref()))
            .or_else(|| self.fallbacks.get(selected))
            .cloned()
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_revision_ignores_rollout_mode_but_changes_with_behavior() {
        let mut grid = RouteGrid {
            fallbacks: BTreeMap::new(),
            judge: Some(RouteJudge {
                mode: JudgeMode::Shadow,
                classification: Classification::Task,
                backend: RouteJudgeBackend::Completion(CompletionJudgeBackend {
                    target: RouteTarget {
                        connection_id: "judge".into(),
                        upstream_model_id: "classifier".into(),
                    },
                    timeout_ms: 1_000,
                }),
                outcomes: BTreeMap::from([(
                    "coding".into(),
                    JudgeOutcomeConfig {
                        model: "primary/coder".into(),
                        description: "code".into(),
                        fallbacks: None,
                    },
                )]),
            }),
            default_target: RouteTarget {
                connection_id: "primary".into(),
                upstream_model_id: "default".into(),
            },
        };
        let shadow = grid.policy_revision();
        grid.judge.as_mut().unwrap().mode = JudgeMode::Live;
        assert_eq!(grid.policy_revision(), shadow);
        grid.judge
            .as_mut()
            .unwrap()
            .outcomes
            .get_mut("coding")
            .unwrap()
            .model = "primary/new-coder".into();
        assert_ne!(grid.policy_revision(), shadow);
    }

    #[test]
    fn default_criteria_cover_binary_difficulty_and_standard_tasks() {
        assert_eq!(
            Classification::Difficulty
                .default_outcomes()
                .iter()
                .map(|(label, _)| *label)
                .collect::<Vec<_>>(),
            ["0", "1"]
        );
        assert_eq!(
            Classification::Task
                .default_outcomes()
                .iter()
                .map(|(label, _)| *label)
                .collect::<Vec<_>>(),
            ["coding", "summarization", "extraction", "other"]
        );
        for classification in [Classification::Difficulty, Classification::Task] {
            let judge = RouteJudge {
                classification,
                outcomes: classification
                    .default_outcomes()
                    .iter()
                    .map(|(label, _)| {
                        (
                            (*label).into(),
                            JudgeOutcomeConfig {
                                model: "conn/model".into(),
                                description: String::new(),
                                fallbacks: None,
                            },
                        )
                    })
                    .collect(),
                ..Default::default()
            };
            for (label, description) in classification.default_outcomes() {
                assert!(description.len() > 100 && description.len() <= 2048);
                let outcome = &judge.outcomes[*label];
                assert_eq!(judge.outcome_description(label, outcome), *description);
                let custom = JudgeOutcomeConfig {
                    description: "My custom criteria".into(),
                    ..outcome.clone()
                };
                assert_eq!(
                    judge.outcome_description(label, &custom),
                    "My custom criteria"
                );
            }
        }
    }

    #[test]
    fn defaults_do_not_reinterpret_custom_labels_or_multilevel_difficulty() {
        let outcome = JudgeOutcomeConfig {
            model: "conn/model".into(),
            description: String::new(),
            fallbacks: None,
        };
        let judge = RouteJudge {
            classification: Classification::Difficulty,
            outcomes: ["0", "1", "2"]
                .into_iter()
                .map(|label| (label.into(), outcome.clone()))
                .collect(),
            ..Default::default()
        };
        assert_eq!(judge.outcome_description("1", &outcome), "");
        let judge = RouteJudge {
            classification: Classification::Task,
            ..judge
        };
        assert_eq!(judge.outcome_description("custom-category", &outcome), "");
    }

    #[test]
    fn grid_requires_a_target_but_judge_and_fallbacks_can_be_omitted() {
        for value in [
            serde_json::json!({}),
            serde_json::json!({"default_target": null}),
            serde_json::json!({"judge": null, "fallbacks": {}}),
        ] {
            assert!(serde_json::from_value::<RouteGrid>(value).is_err());
        }
        let grid: RouteGrid = serde_json::from_value(serde_json::json!({
            "default_target": {"connection_id": "conn", "upstream_model_id": "astra"}
        }))
        .unwrap();
        grid.validate().unwrap();
        assert!(grid.judge.is_none());
        assert!(grid.fallbacks.is_empty());
    }

    #[test]
    fn remote_timeouts_default_on_omission_or_zero_and_validate_the_upper_bound() {
        for kind in ["completion", "decisions"] {
            for configured in [None, Some(0), Some(1), Some(30_000), Some(30_001)] {
                let mut json = serde_json::json!({
                    "type": kind,
                    "target": {"connection_id":"judge-connection", "upstream_model_id":"judge"}
                });
                if let Some(value) = configured {
                    json["timeout_ms"] = serde_json::json!(value);
                }
                let backend: RouteJudgeBackend = serde_json::from_value(json).unwrap();
                let expected = match configured {
                    None | Some(0) => 1_000,
                    Some(value) => value,
                };
                assert_eq!(backend.timeout_ms(), Some(expected));
                let grid = RouteGrid {
                    default_target: RouteTarget {
                        connection_id: "serving-connection".into(),
                        upstream_model_id: "default".into(),
                    },
                    judge: Some(RouteJudge {
                        backend,
                        classification: Classification::Task,
                        outcomes: BTreeMap::from([(
                            "general".into(),
                            JudgeOutcomeConfig {
                                model: "serving-connection/selected".into(),
                                description: String::new(),
                                fallbacks: None,
                            },
                        )]),
                        ..Default::default()
                    }),
                    fallbacks: BTreeMap::new(),
                };
                assert_eq!(grid.validate().is_ok(), expected <= 30_000);
            }
        }
    }

    #[test]
    fn heuristic_has_no_timeout_setting() {
        assert_eq!(RouteJudgeBackend::default().timeout_ms(), None);
        assert!(
            serde_json::from_value::<RouteJudgeBackend>(serde_json::json!({
                "type":"heuristic", "timeout_ms":1000
            }))
            .is_err()
        );
    }

    #[test]
    fn heuristic_keyword_validation_rejects_blank_oversized_and_conflicting_entries() {
        for keyword in [" ".into(), "🦀".repeat(65)] {
            for added in [true, false] {
                let mut config = HeuristicJudgeBackend::default();
                if added {
                    config.added_hard_keywords.push(keyword.clone());
                } else {
                    config.removed_hard_keywords.push(keyword.clone());
                }
                assert!(config.validate().is_err());
            }
        }
        let config = HeuristicJudgeBackend {
            added_hard_keywords: vec!["ÉTAPE".into()],
            removed_hard_keywords: vec!["étape".into()],
            ..Default::default()
        };
        assert!(config.validate().is_err());
        let config = HeuristicJudgeBackend {
            added_hard_keywords: vec!["🦀".repeat(64)],
            ..Default::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn heuristic_keyword_count_limit_applies_to_both_lists_together() {
        let mut config = HeuristicJudgeBackend {
            added_hard_keywords: (0..128).map(|i| format!("added-{i}")).collect(),
            removed_hard_keywords: (0..128).map(|i| format!("removed-{i}")).collect(),
            ..Default::default()
        };
        assert!(config.validate().is_ok());
        config.removed_hard_keywords.push("one-too-many".into());
        assert!(config.validate().is_err());
    }

    #[test]
    fn backend_json_keeps_the_target_with_its_transport() {
        for (backend, json) in [
            (
                RouteJudgeBackend::default(),
                serde_json::json!({"type":"heuristic"}),
            ),
            (
                RouteJudgeBackend::Completion(CompletionJudgeBackend {
                    target: RouteTarget {
                        connection_id: "conn".into(),
                        upstream_model_id: "judge".into(),
                    },
                    timeout_ms: 1500,
                }),
                serde_json::json!({"type":"completion","target":{"connection_id":"conn","upstream_model_id":"judge"},"timeout_ms":1500}),
            ),
            (
                RouteJudgeBackend::Decisions(DecisionsJudgeBackend {
                    target: RouteTarget {
                        connection_id: "conn".into(),
                        upstream_model_id: "decider".into(),
                    },
                    timeout_ms: 2000,
                }),
                serde_json::json!({"type":"decisions","target":{"connection_id":"conn","upstream_model_id":"decider"},"timeout_ms":2000}),
            ),
        ] {
            assert_eq!(serde_json::to_value(&backend).unwrap(), json);
            assert_eq!(
                serde_json::from_value::<RouteJudgeBackend>(json).unwrap(),
                backend
            );
        }
        for json in [
            serde_json::json!({"type":"completion"}),
            serde_json::json!({"type":"decisions"}),
            serde_json::json!({"type":"heuristic","model":"conn/judge"}),
            serde_json::json!({"type":"heuristic","timeout_ms":1000}),
            serde_json::json!({"type":"completion","model":"conn/judge"}),
        ] {
            assert!(serde_json::from_value::<RouteJudgeBackend>(json).is_err());
        }
        assert!(
            serde_json::from_value::<RouteJudge>(serde_json::json!({
                "timeout_ms":1000
            }))
            .is_err(),
            "the retired judge-level timeout is rejected"
        );
    }

    #[test]
    fn override_matches_selected_model_once_and_empty_is_explicit() {
        let grid = RouteGrid {
            fallbacks: BTreeMap::from([
                ("openai/gpt-4o".into(), vec!["openai/gpt-5.5".into()]),
                ("openai/astra".into(), vec!["anthropic/opus".into()]),
            ]),
            default_target: RouteTarget {
                connection_id: "conn".into(),
                upstream_model_id: "astra".into(),
            },
            judge: None,
        };
        let mut overrides =
            BTreeMap::from([("openai/gpt-5.6".into(), vec!["openai/astra".into()])]);
        assert_eq!(
            grid.alternatives("openai/gpt-5.6", None, &overrides),
            ["openai/astra"]
        );
        assert_eq!(
            grid.alternatives("openai/gpt-4o", None, &overrides),
            ["openai/gpt-5.5"]
        );
        overrides.insert("openai/gpt-4o".into(), vec![]);
        assert!(grid
            .alternatives("openai/gpt-4o", None, &overrides)
            .is_empty());
    }

    #[test]
    fn outcome_chain_overrides_default_but_request_wins() {
        let grid = RouteGrid {
            fallbacks: BTreeMap::from([("openai/gpt-4o".into(), vec!["openai/gpt-5.5".into()])]),
            default_target: RouteTarget {
                connection_id: "conn".into(),
                upstream_model_id: "astra".into(),
            },
            judge: None,
        };
        let outcome = JudgeOutcomeConfig {
            model: "openai/gpt-4o".into(),
            description: String::new(),
            fallbacks: Some(vec!["anthropic/opus".into()]),
        };
        assert_eq!(
            grid.alternatives(&outcome.model, Some(&outcome), &BTreeMap::new()),
            ["anthropic/opus"]
        );
        let overrides = BTreeMap::from([(outcome.model.clone(), vec![])]);
        assert!(grid
            .alternatives(&outcome.model, Some(&outcome), &overrides)
            .is_empty());
    }

    #[test]
    fn aliases_duplicates_and_self_fallback_are_rejected() {
        assert!(validate_chain("chalk/assistant", &[]).is_err());
        assert!(validate_chain("conn/gpt-4o", &["chalk/assistant".into()]).is_err());
        assert!(validate_chain("conn/gpt-4o", &["conn/gpt-4o".into()]).is_err());
        assert!(
            validate_chain("conn/gpt-4o", &["conn/astra".into(), "conn/astra".into()]).is_err()
        );
    }
}
