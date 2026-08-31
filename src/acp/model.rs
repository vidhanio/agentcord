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
    let model_option = find_config_option(options, &SessionConfigOptionCategory::Model, &["model"])
        .ok_or_else(|| {
            agent_client_protocol::Error::invalid_request()
                .data("agent does not expose a model option")
        })?;
    let model_id = model_option.id.clone();
    let model_value = select_value(model_option, &model.model_value(), "model")?;

    // Validate the complete user selection before applying its first
    // non-atomic ACP configuration change.
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
        select_value(reasoning_option, &model.reasoning, "reasoning")?;
    }

    let mut options =
        set_config_option(connection, session_id, model_id, model_value, timeout, stop).await?;

    if model.reasoning.is_empty() {
        return Ok(options);
    }

    let reasoning_option = find_config_option(
        &options,
        &SessionConfigOptionCategory::ThoughtLevel,
        &["reasoning", "thought_level", "thinking"],
    )
    .ok_or_else(|| {
        agent_client_protocol::Error::invalid_request()
            .data("agent does not expose a reasoning option")
    })?;
    let reasoning_id = reasoning_option.id.clone();
    let reasoning_value = select_value(reasoning_option, &model.reasoning, "reasoning")?;
    options = set_config_option(
        connection,
        session_id,
        reasoning_id,
        reasoning_value,
        timeout,
        stop,
    )
    .await?;
    Ok(options)
}

/// Sets one ACP configuration option and returns the agent's new snapshot.
async fn set_config_option(
    connection: &ConnectionTo<Agent>,
    session_id: &SessionId,
    config_id: SessionConfigId,
    value: SessionConfigOptionValue,
    timeout: std::time::Duration,
    stop: Option<&Signal>,
) -> Result<Vec<SessionConfigOption>, agent_client_protocol::Error> {
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
    Ok(response.config_options)
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{
        SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectGroup,
        SessionConfigSelectOption,
    };

    use super::{ModelSpec, category_values, default_model};
    use crate::ModelSpecError;

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
