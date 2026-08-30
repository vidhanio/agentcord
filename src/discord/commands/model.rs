use std::collections::BTreeSet;

use agent_client_protocol::schema::v1::SessionConfigOptionCategory;
use poise::serenity_prelude as serenity;
use serenity::all::{AutocompleteChoice, CreateAutocompleteResponse};

use crate::{Bot, BotError, acp::ModelSpec};

/// Changes the model and optional reasoning level for the current ACP session.
#[poise::command(slash_command, check = "super::allowed")]
pub async fn model(
    ctx: poise::ApplicationContext<'_, Bot, BotError>,
    #[description = "model (`model[:reasoning]`)"]
    #[autocomplete = "model_choices"]
    model: String,
) -> Result<(), BotError> {
    let bot = ctx.data().clone();
    let model = ModelSpec::parse(&model)?;
    ctx.defer_ephemeral().await?;
    bot.set_model(ctx.channel_id(), model.clone()).await?;
    ctx.send(
        poise::CreateReply::new()
            .content(format!("model `{model}` selected"))
            .ephemeral(true),
    )
    .await?;
    Ok(())
}

/// Suggests known model and reasoning combinations for the current session.
#[expect(
    clippy::unused_async,
    reason = "Poise autocomplete callbacks must be async"
)]
pub(super) async fn model_choices<'a>(
    ctx: poise::Context<'a, Bot, BotError>,
    partial: &'a str,
) -> CreateAutocompleteResponse<'a> {
    let poise::Context::Application(application) = ctx else {
        return CreateAutocompleteResponse::new();
    };
    let needle = partial.to_lowercase();
    let mut values = BTreeSet::new();
    if let Some(ui) = application.data().session_ui(application.channel_id()) {
        values.extend(combinations(&ui.config_options));
    }

    let choices: Vec<AutocompleteChoice<'static>> = values
        .into_iter()
        .filter(|value| value.to_lowercase().contains(&needle))
        .take(super::CHOICE_LIMIT)
        .map(|value| AutocompleteChoice::new(super::truncate(&value, 100), value))
        .collect();
    CreateAutocompleteResponse::new().set_choices(choices)
}

/// Builds canonical model strings from ACP model and optional reasoning
/// selectors.
pub(super) fn combinations(
    options: &[agent_client_protocol::schema::v1::SessionConfigOption],
) -> Vec<String> {
    let model_values =
        crate::acp::category_values(options, &SessionConfigOptionCategory::Model, &["model"]);
    let reasoning_values = crate::acp::category_values(
        options,
        &SessionConfigOptionCategory::ThoughtLevel,
        &["reasoning", "thought_level", "thinking"],
    );
    let Some(model_values) = model_values else {
        return Vec::new();
    };
    model_values
        .into_iter()
        .flat_map(|model| {
            if let Some(reasoning_values) = reasoning_values
                .as_ref()
                .filter(|values| !values.is_empty())
            {
                reasoning_values
                    .iter()
                    .map(move |reasoning| format!("{model}:{reasoning}"))
                    .collect::<Vec<_>>()
            } else {
                vec![model]
            }
        })
        .filter(|value| value.parse::<crate::acp::ModelSpec>().is_ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{
        SessionConfigOption, SessionConfigOptionCategory, SessionConfigSelectOption,
    };

    use super::combinations;

    /// Includes an ACP model when no reasoning selector is advertised.
    #[test]
    fn combinations_support_model_only_agents() {
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

        assert_eq!(combinations(&options), vec!["claude-sonnet-4"]);
    }
}
