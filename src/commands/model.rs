use agent_client_protocol::schema::v1::{
    SessionConfigId, SessionConfigKind, SessionConfigOptionCategory, SessionConfigOptionValue,
};
use poise::serenity_prelude as serenity;
use serenity::all::{AutocompleteChoice, CreateAutocompleteResponse};

use crate::{Bot, BotError, BotResult};

/// View or set the model and thinking level of this thread's ACP session
#[poise::command(slash_command, check = "super::allowed")]
pub async fn model(
    ctx: poise::ApplicationContext<'_, Bot, BotError>,
    #[description = "model to switch to"]
    #[autocomplete = "model_choices"]
    model: Option<String>,
    #[description = "thinking level to switch to"]
    #[autocomplete = "thinking_choices"]
    thinking: Option<String>,
) -> Result<(), BotError> {
    let bot = (*ctx.data()).clone();
    let thread = ctx.channel_id();
    if model.is_none() && thinking.is_none() {
        return status(ctx, &bot, thread).await;
    }
    ctx.defer_ephemeral().await?;
    bot.ensure_session(thread).await?;

    let mut changes: Vec<(&str, SessionConfigId, SessionConfigOptionValue, String)> = Vec::new();
    if let Some(model) = model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        let (id, value, label) = resolve_value(
            &bot,
            thread,
            &SessionConfigOptionCategory::Model,
            "model",
            model,
        )?;
        changes.push(("model", id, value, label));
    }
    if let Some(thinking) = thinking.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
        let (id, value, label) = resolve_value(
            &bot,
            thread,
            &SessionConfigOptionCategory::ThoughtLevel,
            "thinking",
            thinking,
        )?;
        changes.push(("thought level", id, value, label));
    }
    for (_name, id, value, _label) in &changes {
        bot.set_config_option(thread, id.clone(), value.clone())
            .await?;
    }
    let summary = changes
        .into_iter()
        .map(|(name, _, _, label)| format!("{name} set to **{label}**"))
        .collect::<Vec<_>>()
        .join(" · ");
    ctx.send(poise::CreateReply::new().content(summary).ephemeral(true))
        .await?;
    Ok(())
}

async fn status(
    ctx: poise::ApplicationContext<'_, Bot, BotError>,
    bot: &Bot,
    thread: serenity::all::GenericChannelId,
) -> Result<(), BotError> {
    ctx.defer_ephemeral().await?;
    let content = bot.session_ui(thread).map_or_else(
        || "this session is not running, so its current model is unknown".into(),
        |ui| {
            let model = ui
                .config_label(&SessionConfigOptionCategory::Model)
                .unwrap_or_else(|| "not exposed".into());
            let thinking = ui
                .config_label(&SessionConfigOptionCategory::ThoughtLevel)
                .unwrap_or_else(|| "not exposed".into());
            format!("model: **{model}** · thinking: **{thinking}**")
        },
    );
    ctx.send(poise::CreateReply::new().content(content).ephemeral(true))
        .await?;
    Ok(())
}

/// Resolves a typed value against the first option of a configuration
/// category, accepting either a value id or a human-readable option name.
fn resolve_value(
    bot: &Bot,
    thread: serenity::all::GenericChannelId,
    category: &SessionConfigOptionCategory,
    category_label: &str,
    input: &str,
) -> BotResult<(SessionConfigId, SessionConfigOptionValue, String)> {
    let ui = bot.session_ui(thread).ok_or_else(|| {
        BotError::Other("the ACP session is not running, so its options are unknown".into())
    })?;
    let option = ui
        .config_options
        .iter()
        .find(|option| option.category.as_ref() == Some(category))
        .ok_or_else(|| {
            BotError::Other(format!(
                "this agent does not expose a {category_label} option"
            ))
        })?;
    let config_id = option.id.clone();
    match &option.kind {
        SessionConfigKind::Select(select) => {
            let candidates = crate::acp::select_options(select);
            let needle = input.to_lowercase();
            let matched = candidates
                .iter()
                .find(|candidate| candidate.value.to_string() == input)
                .or_else(|| {
                    candidates
                        .iter()
                        .find(|candidate| candidate.name.to_lowercase() == needle)
                })
                .ok_or_else(|| {
                    BotError::Other(format!(
                        "unknown {category_label} `{input}`; pick one of the suggestions"
                    ))
                })?;
            Ok((
                config_id,
                SessionConfigOptionValue::value_id(matched.value.clone()),
                matched.name.clone(),
            ))
        }
        SessionConfigKind::Boolean(_) => {
            let value = match input.to_lowercase().as_str() {
                "true" | "on" | "yes" => true,
                "false" | "off" | "no" => false,
                _ => {
                    return Err(BotError::Other(
                        "this option is a toggle; use `true` or `false`".into(),
                    ));
                }
            };
            Ok((
                config_id,
                SessionConfigOptionValue::boolean(value),
                if value { "on" } else { "off" }.into(),
            ))
        }
        _ => Err(BotError::Other(format!(
            "this agent's {category_label} option is not selectable"
        ))),
    }
}

fn category_choices<'a>(
    ui: Option<crate::acp::SessionUiState>,
    category: &SessionConfigOptionCategory,
    partial: &'a str,
) -> CreateAutocompleteResponse<'a> {
    let Some(option) = ui.and_then(|ui| {
        ui.config_options
            .into_iter()
            .find(|option| option.category.as_ref() == Some(category))
    }) else {
        return CreateAutocompleteResponse::new();
    };
    let needle = partial.to_lowercase();
    let choices: Vec<AutocompleteChoice<'static>> = match &option.kind {
        SessionConfigKind::Select(select) => crate::acp::select_options(select)
            .into_iter()
            .filter(|candidate| {
                candidate.name.to_lowercase().contains(&needle)
                    || candidate.value.to_string().to_lowercase().contains(&needle)
            })
            .take(super::CHOICE_LIMIT)
            .map(|candidate| {
                AutocompleteChoice::new(
                    super::truncate(&candidate.name, 100),
                    candidate.value.to_string(),
                )
            })
            .collect(),
        SessionConfigKind::Boolean(_) => ["true", "false"]
            .into_iter()
            .filter(|value| value.contains(&needle))
            .map(|value| AutocompleteChoice::new(value, value))
            .collect(),
        _ => Vec::new(),
    };
    CreateAutocompleteResponse::new().set_choices(choices)
}

#[expect(
    clippy::unused_async,
    reason = "poise autocomplete callbacks must be async functions"
)]
async fn model_choices<'a>(
    ctx: poise::Context<'a, Bot, BotError>,
    partial: &'a str,
) -> CreateAutocompleteResponse<'a> {
    let poise::Context::Application(application) = ctx else {
        return CreateAutocompleteResponse::new();
    };
    let ui = application.data().session_ui(application.channel_id());
    category_choices(ui, &SessionConfigOptionCategory::Model, partial)
}

#[expect(
    clippy::unused_async,
    reason = "poise autocomplete callbacks must be async functions"
)]
async fn thinking_choices<'a>(
    ctx: poise::Context<'a, Bot, BotError>,
    partial: &'a str,
) -> CreateAutocompleteResponse<'a> {
    let poise::Context::Application(application) = ctx else {
        return CreateAutocompleteResponse::new();
    };
    let ui = application.data().session_ui(application.channel_id());
    category_choices(ui, &SessionConfigOptionCategory::ThoughtLevel, partial)
}
