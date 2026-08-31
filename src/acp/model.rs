//! Model selection and ACP session configuration helpers.

use std::str::FromStr;

use agent_client_protocol::{
    Agent, ConnectionTo,
    schema::v1::{
        SessionConfigId, SessionConfigKind, SessionConfigOption, SessionConfigOptionCategory,
        SessionConfigOptionValue, SessionConfigSelectOptions, SessionId,
        SetSessionConfigOptionRequest, SetSessionConfigOptionResponse,
    },
};
use tracing::debug;

use super::{
    protocol::request_with_timeout,
    runtime::{Signal, stop_aware},
};
use crate::{BotError, BotResult, error::ModelSpecError};

/// A user-selected ACP model and optional reasoning level.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelSpec {
    /// Optional provider portion of the model identifier.
    pub provider: String,
    /// Model identifier returned by the ACP agent.
    pub model: String,
    /// Optional reasoning level requested from the ACP agent.
    ///
    /// An empty value means that the agent does not expose a separate
    /// reasoning selector, or that the user did not select one.
    pub reasoning: String,
}

impl ModelSpec {
    /// Parses the canonical `model[:reasoning]` form.
    pub fn parse(input: &str) -> BotResult<Self> {
        input.parse().map_err(BotError::from)
    }

    /// Returns the canonical model string shown in Discord.
    #[must_use]
    pub fn canonical(&self) -> String {
        let model = self.model_value();
        if self.reasoning.is_empty() {
            model
        } else {
            format!("{model}:{}", self.reasoning)
        }
    }

    /// Returns the model selector sent to ACP.
    #[must_use]
    pub fn model_value(&self) -> String {
        if self.provider.is_empty() {
            self.model.clone()
        } else {
            format!("{}/{}", self.provider, self.model)
        }
    }
}

impl FromStr for ModelSpec {
    type Err = ModelSpecError;

    /// Parses the canonical `model[:reasoning]` form.
    fn from_str(input: &str) -> Result<Self, Self::Err> {
        let input = input.trim();
        let (model_value, reasoning) = input
            .split_once(':')
            .map_or((input, ""), |(model, reasoning)| (model, reasoning));
        if model_value.contains(':') || reasoning.contains(':') {
            return Err(ModelSpecError::ExtraSeparator {
                input: input.to_owned(),
            });
        }
        let (provider, model) = match model_value.split_once('/') {
            Some((provider, model)) if provider.is_empty() || model.is_empty() => {
                return Err(ModelSpecError::EmptyPart {
                    input: input.to_owned(),
                });
            }
            Some((provider, model)) => (provider, model),
            None => ("", model_value),
        };
        if model.is_empty() || input.ends_with(':') {
            return Err(ModelSpecError::EmptyPart {
                input: input.to_owned(),
            });
        }
        if provider.chars().any(char::is_whitespace)
            || model.chars().any(char::is_whitespace)
            || reasoning.chars().any(char::is_whitespace)
        {
            return Err(ModelSpecError::Whitespace {
                input: input.to_owned(),
            });
        }
        Ok(Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
            reasoning: reasoning.to_owned(),
        })
    }
}

impl std::fmt::Display for ModelSpec {
    /// Formats this model using the user-facing canonical form.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.canonical())
    }
}

/// Agent-advertised session configuration used by model autocomplete.
#[derive(Clone, Debug, Default)]
pub struct SessionUiState {
    /// Current ACP configuration options and their selectable values.
    pub config_options: Vec<SessionConfigOption>,
}

impl SessionUiState {
    /// Replaces the cached configuration options with an ACP snapshot.
    pub(super) fn apply_config_options(&mut self, options: Vec<SessionConfigOption>) {
        self.config_options = options;
    }
}

/// Flattens grouped and ungrouped ACP select values into one list.
fn select_options(
    select: &agent_client_protocol::schema::v1::SessionConfigSelect,
) -> Vec<agent_client_protocol::schema::v1::SessionConfigSelectOption> {
    match &select.options {
        SessionConfigSelectOptions::Ungrouped(options) => options.clone(),
        SessionConfigSelectOptions::Grouped(groups) => groups
            .iter()
            .flat_map(|group| group.options.iter().cloned())
            .collect(),
        _ => Vec::new(),
    }
}

/// Returns selectable values for a semantic configuration category.
pub fn category_values(
    options: &[SessionConfigOption],
    category: &SessionConfigOptionCategory,
    aliases: &[&str],
) -> Option<Vec<String>> {
    let option = find_config_option(options, category, aliases)?;
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    Some(
        select_options(select)
            .into_iter()
            .map(|value| value.value.to_string())
            .collect(),
    )
}

/// Returns the current selectable value for a semantic configuration category.
fn category_current_value(
    options: &[SessionConfigOption],
    category: &SessionConfigOptionCategory,
    aliases: &[&str],
) -> Option<String> {
    let option = find_config_option(options, category, aliases)?;
    let SessionConfigKind::Select(select) = &option.kind else {
        return None;
    };
    Some(select.current_value.to_string())
}

/// Returns the model currently selected by an ACP session.
pub fn default_model(options: &[SessionConfigOption]) -> Option<String> {
    let model = category_current_value(options, &SessionConfigOptionCategory::Model, &["model"])?;
    let reasoning = category_current_value(
        options,
        &SessionConfigOptionCategory::ThoughtLevel,
        &["reasoning", "thought_level", "thinking"],
    );
    Some(
        reasoning
            .filter(|value| !value.is_empty())
            .map_or_else(|| model.clone(), |reasoning| format!("{model}:{reasoning}")),
    )
}

/// Resolves the ACP configuration changes represented by one model spec.
pub(super) fn model_changes(
    options: &[SessionConfigOption],
    model: &ModelSpec,
) -> Result<Vec<(SessionConfigId, SessionConfigOptionValue)>, agent_client_protocol::Error> {
    let model_option = find_config_option(options, &SessionConfigOptionCategory::Model, &["model"])
        .ok_or_else(|| {
            agent_client_protocol::Error::invalid_request()
                .data("agent does not expose a model option")
        })?;
    let mut changes = vec![(
        model_option.id.clone(),
        select_value(model_option, &model.model_value(), "model")?,
    )];
    if !model.reasoning.is_empty() {
        let reasoning_option = find_config_option(
            options,
            &SessionConfigOptionCategory::ThoughtLevel,
            &["reasoning", "thought_level", "thinking"],
        )
        .ok_or_else(|| {
            agent_client_protocol::Error::invalid_request()
                .data("agent does not expose a reasoning option")
        })?;
        changes.push((
            reasoning_option.id.clone(),
            select_value(reasoning_option, &model.reasoning, "reasoning")?,
        ));
    }
    Ok(changes)
}

/// Finds a semantic configuration category, with compatibility aliases for
/// agents that predate ACP's category fields.
fn find_config_option<'a>(
    options: &'a [SessionConfigOption],
    category: &SessionConfigOptionCategory,
    aliases: &[&str],
) -> Option<&'a SessionConfigOption> {
    options
        .iter()
        .find(|option| option.category.as_ref() == Some(category))
        .or_else(|| {
            options.iter().find(|option| {
                aliases.iter().any(|alias| {
                    option.id.to_string().eq_ignore_ascii_case(alias)
                        || option.name.eq_ignore_ascii_case(alias)
                })
            })
        })
}

/// Resolves one user value against an ACP select option.
fn select_value(
    option: &SessionConfigOption,
    input: &str,
    label: &str,
) -> Result<SessionConfigOptionValue, agent_client_protocol::Error> {
    let SessionConfigKind::Select(select) = &option.kind else {
        return Err(agent_client_protocol::Error::invalid_request()
            .data(format!("agent's {label} option is not selectable")));
    };
    let needle = input.to_lowercase();
    let candidates = select_options(select);
    let selected = candidates
        .iter()
        .find(|candidate| candidate.value.to_string() == input)
        .or_else(|| {
            candidates
                .iter()
                .find(|candidate| candidate.name.to_lowercase() == needle)
        })
        .ok_or_else(|| {
            agent_client_protocol::Error::invalid_request()
                .data(format!("unknown {label} `{input}`"))
        })?;
    Ok(SessionConfigOptionValue::value_id(selected.value.clone()))
}

/// Applies a model spec through ACP's session configuration requests.
pub(super) async fn apply_model(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    options: &[SessionConfigOption],
    model: &ModelSpec,
    timeout: std::time::Duration,
    stop: Option<&Signal>,
) -> Result<Vec<SessionConfigOption>, agent_client_protocol::Error> {
    let mut options = options.to_vec();
    let change_count = model_changes(&options, model)?.len();
    debug!(
        session = %session_id,
        model = %model,
        changes = change_count,
        "resolved acp model configuration"
    );
    for index in 0..change_count {
        let (config_id, value) = model_changes(&options, model)?
            .into_iter()
            .nth(index)
            .ok_or_else(|| {
                agent_client_protocol::Error::internal_error()
                    .data("model configuration did not produce enough options")
            })?;
        debug!(
            session = %session_id,
            model = %model,
            option = ?config_id,
            index,
            total = change_count,
            "sending acp `session/set_config_option`..."
        );
        let request = request_with_timeout(
            timeout,
            connection
                .send_request(SetSessionConfigOptionRequest::new(
                    session_id.clone(),
                    config_id.clone(),
                    value,
                ))
                .block_task(),
            "acp `session/set_config_option` timed out",
        );
        let response: SetSessionConfigOptionResponse = if let Some(stop) = stop {
            stop_aware(stop, request).await?
        } else {
            request.await?
        };
        debug!(
            session = %session_id,
            model = %model,
            option = ?config_id,
            options = response.config_options.len(),
            "acp `session/set_config_option` completed"
        );
        options = response.config_options;
    }
    Ok(options)
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{
        SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectGroup,
        SessionConfigSelectOption,
    };

    use super::{ModelSpec, category_values, default_model, model_changes};
    use crate::ModelSpecError;

    /// Parses and canonicalizes a provider-qualified model selector.
    #[test]
    fn parses_model_spec() {
        let model = ModelSpec::parse("openai/gpt-4o:high").unwrap();
        assert_eq!(model.model_value(), "openai/gpt-4o");
        assert_eq!(model.to_string(), "openai/gpt-4o:high");
        let parsed = "openai/gpt-4o:high".parse::<ModelSpec>().unwrap();
        assert_eq!(parsed, model);

        let model = ModelSpec::parse("claude-sonnet-4").unwrap();
        assert_eq!(model.model_value(), "claude-sonnet-4");
        assert_eq!(model.to_string(), "claude-sonnet-4");
    }

    /// Rejects model selectors with empty, repeated, or whitespace components.
    #[test]
    fn rejects_malformed_model_spec() {
        assert!(matches!(
            ":high".parse::<ModelSpec>(),
            Err(ModelSpecError::EmptyPart { .. })
        ));
        assert!(matches!(
            "openai/gpt-4o:".parse::<ModelSpec>(),
            Err(ModelSpecError::EmptyPart { .. })
        ));
        assert!(matches!(
            "openai/:high".parse::<ModelSpec>(),
            Err(ModelSpecError::EmptyPart { .. })
        ));
        assert!("openai/gpt/4o:high".parse::<ModelSpec>().is_ok());
        assert!(matches!(
            "openai/gpt-4o:high:extra".parse::<ModelSpec>(),
            Err(ModelSpecError::ExtraSeparator { .. })
        ));
        assert!(matches!(
            "openai/gpt 4o:high".parse::<ModelSpec>(),
            Err(ModelSpecError::Whitespace { .. })
        ));
    }

    /// Resolves model and reasoning values against ACP selectors.
    #[test]
    fn resolves_model_changes_from_categories() {
        let options = vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "openai/gpt-4o",
                vec![SessionConfigSelectOption::new("openai/gpt-4o", "GPT-4o")],
            )
            .category(SessionConfigOptionCategory::Model),
            SessionConfigOption::select(
                "reasoning",
                "Reasoning",
                "high",
                vec![SessionConfigSelectOption::new("high", "High")],
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ];
        let model = ModelSpec::parse("openai/gpt-4o:high").unwrap();
        let changes = model_changes(&options, &model).unwrap();
        assert_eq!(changes.len(), 2);
    }

    /// Resolves a model when the agent does not expose reasoning levels.
    #[test]
    fn resolves_model_only_change() {
        let options = vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "claude-sonnet-4",
                vec![SessionConfigSelectOption::new(
                    "claude-sonnet-4",
                    "Claude Sonnet 4",
                )],
            )
            .category(SessionConfigOptionCategory::Model),
        ];
        let model = ModelSpec::parse("claude-sonnet-4").unwrap();
        let changes = model_changes(&options, &model).unwrap();
        assert_eq!(changes.len(), 1);
    }

    /// Reads the agent-selected model and reasoning defaults.
    #[test]
    fn reads_default_model_from_categories() {
        let options = vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "openai/gpt-4o",
                vec![SessionConfigSelectOption::new("openai/gpt-4o", "GPT-4o")],
            )
            .category(SessionConfigOptionCategory::Model),
            SessionConfigOption::select(
                "reasoning",
                "Reasoning",
                "high",
                vec![SessionConfigSelectOption::new("high", "High")],
            )
            .category(SessionConfigOptionCategory::ThoughtLevel),
        ];

        assert_eq!(
            default_model(&options).as_deref(),
            Some("openai/gpt-4o:high")
        );
    }

    /// Flattens grouped ACP values for model autocomplete and selection.
    #[test]
    fn flattens_grouped_select_options() {
        let options = vec![
            SessionConfigOption::select(
                "model",
                "Model",
                "openai/gpt-4o",
                vec![SessionConfigSelectGroup::new(
                    "openai",
                    "OpenAI",
                    vec![SessionConfigSelectOption::new("openai/gpt-4o", "GPT-4o")],
                )],
            )
            .category(SessionConfigOptionCategory::Model),
        ];

        assert_eq!(
            category_values(&options, &SessionConfigOptionCategory::Model, &["model"]),
            Some(vec!["openai/gpt-4o".to_owned()])
        );
    }
}
