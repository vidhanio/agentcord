use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicU64, Ordering},
};

use agent_client_protocol::schema::v1::{
    CreateElicitationRequest, CreateElicitationResponse, ElicitationAcceptAction,
    ElicitationAction, ElicitationContentValue, ElicitationFormMode, ElicitationMode,
    ElicitationPropertySchema, ElicitationSchema, ElicitationUrlMode, EnumOption,
};
use serenity::{
    all::{
        ButtonStyle, ComponentInteraction, Context, CreateButton, CreateInputText,
        CreateInteractionResponse, CreateInteractionResponseMessage, CreateLabel, CreateMessage,
        CreateModal, CreateModalComponent, CreateSelectMenu, CreateSelectMenuKind,
        CreateSelectMenuOption, GenericChannelId, InputTextStyle, LabelComponent, MessageId,
        ModalComponent, ModalInteractionData,
    },
    collector::CollectComponentInteractions,
    futures::StreamExt,
};

use crate::{Bot, render::split_message};

/// Message budget leaving room for elicitation formatting.
const MESSAGE_LIMIT: usize = 1900;
/// Maximum number of inputs supported by a Discord modal.
const MODAL_FIELDS_LIMIT: usize = 5;
/// Maximum Discord modal-title length.
const MODAL_TITLE_LIMIT: usize = 45;
/// Maximum Discord modal-field label length.
const FIELD_LABEL_LIMIT: usize = 45;
/// Prefix used for elicitation status messages.
const FOOTER: &str = "📋 **elicitation** — ";

/// Process-local nonce source for unique interaction component ids.
static ELICITATION_COUNTER: AtomicU64 = AtomicU64::new(1);

/// Presents an `elicitation/create` request to the allowed user and maps their
/// response back onto the protocol's accept/decline/cancel actions.
pub async fn handle(
    bot: &Bot,
    ctx: Context,
    thread: GenericChannelId,
    agent_name: &str,
    request: CreateElicitationRequest,
) -> CreateElicitationResponse {
    if matches!(
        request.scope(),
        agent_client_protocol::schema::v1::ElicitationScope::Request(_)
    ) {
        // Request-scoped elicitations have no session thread to surface in.
        return declined();
    }
    let agent_name = sanitize(agent_name);
    let message = truncate(&request.message, 500);
    match request.mode {
        ElicitationMode::Form(form) => {
            present_form(bot, ctx, thread, &agent_name, &message, &form).await
        }
        ElicitationMode::Url(url) => {
            present_url(bot, ctx, thread, &agent_name, &message, &url).await
        }
        // Unknown elicitation modes must not be rendered as known ones.
        _ => declined(),
    }
}

/// Presents a supported elicitation schema as a Discord modal.
async fn present_form(
    bot: &Bot,
    ctx: Context,
    thread: GenericChannelId,
    agent_name: &str,
    message: &str,
    form: &ElicitationFormMode,
) -> CreateElicitationResponse {
    let Some(spec) = modal_spec(&form.requested_schema) else {
        return reject_schema(ctx, thread, agent_name, message, &form.requested_schema).await;
    };
    let nonce = ELICITATION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let form_id = format!("agentcord:elicit:{nonce}:form");
    let decline_id = format!("agentcord:elicit:{nonce}:decline");
    let modal_id = format!("agentcord:elicit:{nonce}:modal");
    let Ok(posted) = thread
        .send_message(
            &ctx.http,
            form_message(agent_name, message)
                .button(
                    CreateButton::new(form_id.clone())
                        .label("Fill form")
                        .style(ButtonStyle::Primary),
                )
                .button(
                    CreateButton::new(decline_id.clone())
                        .label("Decline")
                        .style(ButtonStyle::Danger),
                ),
        )
        .await
    else {
        return cancelled();
    };

    let Some(interaction) = allowed_interaction(bot, &ctx, posted.id).await else {
        timeout_edit(&ctx, thread, posted.id).await;
        return declined();
    };
    if interaction.data.custom_id.as_str() == decline_id {
        let _ = interaction
            .create_response(
                &ctx.http,
                CreateInteractionResponse::UpdateMessage(
                    CreateInteractionResponseMessage::new()
                        .content(format!("{FOOTER}declined by the user"))
                        .components(vec![]),
                ),
            )
            .await;
        return declined();
    }
    if interaction
        .create_response(
            &ctx.http,
            CreateInteractionResponse::Modal(build_modal(&modal_id, &spec)),
        )
        .await
        .is_err()
    {
        return cancelled();
    }
    let submit = serenity::collector::ModalInteractionCollector::new(&ctx)
        .filter(move |submit| submit.data.custom_id.as_str() == modal_id)
        .timeout(bot.config.timeouts.modal)
        .await;
    if let Some(submit) = submit {
        return accept_modal(&ctx, thread, posted.id, &spec, &submit.data).await;
    }
    let _ = thread
        .edit_message(
            &ctx.http,
            posted.id,
            serenity::all::EditMessage::new()
                .content(format!("{FOOTER}dismissed by the user"))
                .components(vec![]),
        )
        .await;
    cancelled()
}

/// Validates submitted modal values and accepts the elicitation.
async fn accept_modal(
    ctx: &Context,
    thread: GenericChannelId,
    message: MessageId,
    spec: &ModalSpec,
    data: &ModalInteractionData,
) -> CreateElicitationResponse {
    let values = parse_modal(data);
    let mut content = BTreeMap::new();
    for field in &spec.fields {
        let Some(value) = values.get(&field.custom_id) else {
            continue;
        };
        if value.trim().is_empty() {
            continue;
        }
        match &field.input {
            FieldInput::Select { .. } => {
                content.insert(
                    field.custom_id.clone(),
                    ElicitationContentValue::from(value.as_str()),
                );
            }
            FieldInput::Text { placeholder, .. } => match placeholder {
                Placeholder::Integer => match value.parse::<i64>() {
                    Ok(parsed) => {
                        content.insert(
                            field.custom_id.clone(),
                            ElicitationContentValue::from(parsed),
                        );
                    }
                    Err(_) => {
                        return invalid_field(ctx, thread, message, &field.label, "an integer")
                            .await;
                    }
                },
                Placeholder::Number => match value.parse::<f64>() {
                    Ok(parsed) => {
                        content.insert(
                            field.custom_id.clone(),
                            ElicitationContentValue::from(parsed),
                        );
                    }
                    Err(_) => {
                        return invalid_field(ctx, thread, message, &field.label, "a number").await;
                    }
                },
                Placeholder::None => {
                    content.insert(
                        field.custom_id.clone(),
                        ElicitationContentValue::from(value.as_str()),
                    );
                }
            },
        }
    }
    let _ = thread
        .edit_message(
            &ctx.http,
            message,
            serenity::all::EditMessage::new()
                .content(format!("{FOOTER}submitted"))
                .components(vec![]),
        )
        .await;
    CreateElicitationResponse::new(ElicitationAction::Accept(
        ElicitationAcceptAction::new().content(content),
    ))
}

/// Explains a field-validation failure and cancels the elicitation.
async fn invalid_field(
    ctx: &Context,
    thread: GenericChannelId,
    message: MessageId,
    label: &str,
    expected: &str,
) -> CreateElicitationResponse {
    let _ = thread
        .edit_message(
            &ctx.http,
            message,
            serenity::all::EditMessage::new()
                .content(format!("{FOOTER}cancelled: `{label}` must be {expected}"))
                .components(vec![]),
        )
        .await;
    cancelled()
}

/// Presents a secure URL elicitation and waits for accept or decline.
async fn present_url(
    bot: &Bot,
    ctx: Context,
    thread: GenericChannelId,
    agent_name: &str,
    message: &str,
    url_mode: &ElicitationUrlMode,
) -> CreateElicitationResponse {
    let url = url_mode.url.clone();
    if !url.starts_with("https://") {
        let _ = thread
            .say(
                &ctx.http,
                format!(
                    "{agent_name} requested an elicitation at a non-HTTPS URL, which agentcord declines"
                ),
            )
            .await;
        return declined();
    }
    let nonce = ELICITATION_COUNTER.fetch_add(1, Ordering::Relaxed);
    let consent_id = format!("agentcord:elicit:{nonce}:consent");
    let decline_id = format!("agentcord:elicit:{nonce}:decline");
    let Ok(posted) = thread
        .send_message(
            &ctx.http,
            form_message(
                agent_name,
                &format!("{message}\nURL: {url}\nOnly consent if you recognize this address."),
            )
            .button(CreateButton::new_link(url.clone()).label("Open URL"))
            .button(
                CreateButton::new(consent_id.clone())
                    .label("I opened it")
                    .style(ButtonStyle::Success),
            )
            .button(
                CreateButton::new(decline_id.clone())
                    .label("Decline")
                    .style(ButtonStyle::Danger),
            ),
        )
        .await
    else {
        return cancelled();
    };

    let Some(interaction) = allowed_interaction(bot, &ctx, posted.id).await else {
        timeout_edit(&ctx, thread, posted.id).await;
        return declined();
    };
    if interaction.data.custom_id.as_str() == consent_id {
        let _ = interaction
            .create_response(
                &ctx.http,
                CreateInteractionResponse::UpdateMessage(
                    CreateInteractionResponseMessage::new()
                        .content(format!("{FOOTER}consented"))
                        .components(vec![]),
                ),
            )
            .await;
        return CreateElicitationResponse::new(ElicitationAction::Accept(
            ElicitationAcceptAction::new(),
        ));
    }
    let _ = interaction
        .create_response(
            &ctx.http,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content(format!("{FOOTER}declined by the user"))
                    .components(vec![]),
            ),
        )
        .await;
    declined()
}

/// Waits for the next component interaction from the allowed user, answering
/// everyone else with an ephemeral notice. Returns `None` on timeout.
async fn allowed_interaction(
    bot: &Bot,
    ctx: &Context,
    message: MessageId,
) -> Option<ComponentInteraction> {
    let mut interactions = message
        .collect_component_interactions(ctx)
        .timeout(bot.config.timeouts.permission)
        .stream();
    while let Some(interaction) = interactions.next().await {
        if !bot.is_allowed(interaction.user.id) {
            let _ = interaction
                .create_response(
                    &ctx.http,
                    CreateInteractionResponse::Message(
                        CreateInteractionResponseMessage::new()
                            .content("you are not allowed to answer this request")
                            .ephemeral(true),
                    ),
                )
                .await;
            continue;
        }
        return Some(interaction);
    }
    None
}

/// Marks an elicitation message as timed out on a best-effort basis.
async fn timeout_edit(ctx: &Context, thread: GenericChannelId, message: MessageId) {
    let _ = thread
        .edit_message(
            &ctx.http,
            message,
            serenity::all::EditMessage::new()
                .content(format!("{FOOTER}timed out and was declined"))
                .components(vec![]),
        )
        .await;
}

/// Reports why an unsupported form schema was declined.
async fn reject_schema(
    ctx: Context,
    thread: GenericChannelId,
    agent_name: &str,
    message: &str,
    schema: &ElicitationSchema,
) -> CreateElicitationResponse {
    let reason = if schema.properties.len() > MODAL_FIELDS_LIMIT {
        format!(
            "the form has {} fields, but Discord modals support at most {MODAL_FIELDS_LIMIT}",
            schema.properties.len()
        )
    } else {
        "the form contains field types agentcord cannot render".into()
    };
    let chunks = split_message(
        &format!(
            "📋 **{agent_name} needs input** — {message}\ncannot present this form: {reason}\n```json\n{}\n```",
            serde_json::to_string_pretty(schema).unwrap_or_default()
        ),
        MESSAGE_LIMIT,
    );
    for chunk in &chunks {
        let _ = thread.say(&ctx.http, chunk).await;
    }
    declined()
}

/// A Discord-compatible input derived from an ACP schema property.
enum FieldInput {
    /// A free-form short text input.
    Text {
        /// Hint describing the expected primitive type.
        placeholder: Placeholder,
        /// Optional initial field value.
        default: Option<String>,
    },
    /// A bounded string-selection input.
    Select {
        /// Display-label and submitted-value pairs.
        options: Vec<(String, String)>,
        /// Optional initially selected value.
        default: Option<String>,
    },
}

/// Placeholder semantics for text-backed primitive fields.
enum Placeholder {
    /// No type hint is needed.
    None,
    /// The value must parse as an integer.
    Integer,
    /// The value must parse as a finite or non-finite floating-point number.
    Number,
}

/// A validated Discord modal derived from an elicitation schema.
struct ModalSpec {
    /// Bounded modal title.
    title: String,
    /// Fields in schema presentation order.
    fields: Vec<ModalField>,
}

/// One validated field in an elicitation modal.
struct ModalField {
    /// Schema property name used as the component id.
    custom_id: String,
    /// Human-readable Discord label.
    label: String,
    /// Whether the schema requires a submitted value.
    required: bool,
    /// Discord component representation for the property.
    input: FieldInput,
}

/// Maps a restricted JSON schema onto Discord modal components, returning
/// `None` when the schema cannot be rendered faithfully.
fn modal_spec(schema: &ElicitationSchema) -> Option<ModalSpec> {
    if schema.properties.is_empty() || schema.properties.len() > MODAL_FIELDS_LIMIT {
        return None;
    }
    let mut fields = Vec::with_capacity(schema.properties.len());
    for (name, property) in &schema.properties {
        let required = schema
            .required
            .as_ref()
            .is_some_and(|required| required.iter().any(|entry| entry == name));
        let label = truncate(property.title().unwrap_or(name), FIELD_LABEL_LIMIT);
        let custom_id = name.clone();
        let input = match property {
            ElicitationPropertySchema::String(string) => string_options(string).map_or_else(
                || FieldInput::Text {
                    placeholder: Placeholder::None,
                    default: string.default.clone(),
                },
                |options| FieldInput::Select {
                    options,
                    default: string.default.clone(),
                },
            ),
            ElicitationPropertySchema::Integer(integer) => FieldInput::Text {
                placeholder: Placeholder::Integer,
                default: integer.default.map(|value| value.to_string()),
            },
            ElicitationPropertySchema::Number(number) => FieldInput::Text {
                placeholder: Placeholder::Number,
                default: number.default.map(|value| value.to_string()),
            },
            ElicitationPropertySchema::Boolean(boolean) => FieldInput::Select {
                options: vec![
                    ("true".into(), "true".into()),
                    ("false".into(), "false".into()),
                ],
                default: boolean.default.map(|value| value.to_string()),
            },
            _ => return None,
        };
        fields.push(ModalField {
            custom_id,
            label,
            required,
            input,
        });
    }
    Some(ModalSpec {
        title: truncate(
            &schema.title.clone().unwrap_or_else(|| "agent input".into()),
            MODAL_TITLE_LIMIT,
        ),
        fields,
    })
}

/// Extracts select-menu options from a string enum schema.
fn string_options(
    string: &agent_client_protocol::schema::v1::StringPropertySchema,
) -> Option<Vec<(String, String)>> {
    if let Some(values) = &string.enum_values {
        return Some(
            values
                .iter()
                .map(|value| (value.clone(), value.clone()))
                .collect(),
        );
    }
    string.one_of.as_ref().map(|options: &Vec<EnumOption>| {
        options
            .iter()
            .map(|option| (option.title.clone(), option.value.clone()))
            .collect()
    })
}

/// Builds Discord modal components from a validated modal specification.
fn build_modal<'a>(modal_id: &'a str, spec: &'a ModalSpec) -> CreateModal<'a> {
    let components: Vec<CreateModalComponent> = spec
        .fields
        .iter()
        .map(|field| match &field.input {
            FieldInput::Text {
                placeholder,
                default,
            } => {
                let mut input = CreateInputText::new(InputTextStyle::Short, &field.custom_id)
                    .required(field.required);
                input = match placeholder {
                    Placeholder::None => input,
                    Placeholder::Integer => input.placeholder("integer"),
                    Placeholder::Number => input.placeholder("number"),
                };
                if let Some(default) = default {
                    input = input.value(default.clone());
                }
                CreateModalComponent::Label(CreateLabel::input_text(&field.label, input))
            }
            FieldInput::Select { options, default } => {
                let menu_options = options
                    .iter()
                    .map(|(label, value)| {
                        CreateSelectMenuOption::new(label, value.clone())
                            .default_selection(default.as_deref() == Some(value))
                    })
                    .collect();
                let select = CreateSelectMenu::new(
                    &field.custom_id,
                    CreateSelectMenuKind::String {
                        options: menu_options,
                    },
                );
                CreateModalComponent::Label(CreateLabel::select_menu(&field.label, select))
            }
        })
        .collect();
    CreateModal::new(modal_id, &spec.title).components(components)
}

/// Extracts submitted modal field values by schema property name.
fn parse_modal(data: &ModalInteractionData) -> BTreeMap<String, String> {
    let mut values = BTreeMap::new();
    for component in &data.components {
        let ModalComponent::Label(label) = component else {
            continue;
        };
        match &label.component {
            LabelComponent::InputText(text) => {
                values.insert(text.custom_id.to_string(), text.value.to_string());
            }
            LabelComponent::SelectMenu(select) => {
                if let Some(value) = select.values.as_slice().first() {
                    values.insert(select.custom_id.to_string(), value.clone());
                }
            }
            _ => {}
        }
    }
    values
}

/// Builds the Discord message that introduces a form elicitation.
fn form_message(agent_name: &str, message: &str) -> CreateMessage<'static> {
    CreateMessage::new().content(format!("📋 **{agent_name} needs input** — {message}"))
}

/// Builds a declined elicitation response without user content.
fn declined() -> CreateElicitationResponse {
    CreateElicitationResponse::new(ElicitationAction::Decline)
}

/// The response sent when an elicitation cannot be surfaced to the user.
pub fn declined_response() -> CreateElicitationResponse {
    declined()
}

/// Builds a cancelled elicitation response without user content.
fn cancelled() -> CreateElicitationResponse {
    CreateElicitationResponse::new(ElicitationAction::Cancel)
}

/// Removes mention and code-span characters from agent-provided labels.
fn sanitize(value: &str) -> String {
    value.replace(['@', '`'], "")
}

/// Truncates elicitation labels without splitting Unicode characters.
fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut truncated = value.chars().take(limit - 1).collect::<String>();
    truncated.push('…');
    truncated
}

/// Provides a common title accessor across supported property variants.
trait PropertyTitle {
    /// Returns the property's optional human-readable title.
    fn title(&self) -> Option<&str>;
}

impl PropertyTitle for ElicitationPropertySchema {
    /// Reads a title only from property variants rendered by Agentcord.
    fn title(&self) -> Option<&str> {
        match self {
            Self::String(property) => property.title.as_deref(),
            Self::Integer(property) => property.title.as_deref(),
            Self::Number(property) => property.title.as_deref(),
            Self::Boolean(property) => property.title.as_deref(),
            _ => None,
        }
    }
}
