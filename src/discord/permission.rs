//! Discord interactions for ACP permission requests.

use std::time::Duration;

use agent_client_protocol::schema::v1::{
    PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
    RequestPermissionResponse, SelectedPermissionOutcome,
};
use serenity::{
    all::{
        ButtonStyle, Context, CreateActionRow, CreateButton, CreateComponent,
        CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
        GenericChannelId, UserId,
    },
    collector::CollectComponentInteractions,
    futures::StreamExt,
};
use tracing::{debug, info, warn};

/// Maximum number of permission options Discord accepts in one message.
const MAX_OPTIONS: usize = 25;
/// Maximum number of buttons Discord permits in one action row.
const BUTTONS_PER_ROW: usize = 5;
/// Maximum number of action rows Discord permits in one message.
const MAX_ACTION_ROWS: usize = 5;

/// Presents one ACP permission request in Discord and waits for the allowed
/// user.
pub async fn ask(
    context: Context,
    channel: GenericChannelId,
    allowed_user: UserId,
    timeout: Duration,
    request: RequestPermissionRequest,
) -> RequestPermissionResponse {
    if request.options.is_empty() || request.options.len() > MAX_OPTIONS {
        warn!(
            channel = ?channel,
            options = request.options.len(),
            "rejecting invalid permission request..."
        );
        let message_content = format!(
            "permission request cancelled: acp supplied {} choices, but discord supports 1–25",
            request.options.len()
        );
        report(
            &context,
            channel,
            message_content,
            "invalid permission request",
        )
        .await;
        return cancelled();
    }

    let components = permission_components(&request);
    if components.len() > MAX_ACTION_ROWS {
        warn!(
            channel = ?channel,
            options = request.options.len(),
            rows = components.len(),
            "rejecting permission request that exceeds discord component limits..."
        );
        report(
            &context,
            channel,
            "permission request cancelled: discord cannot display all permission choices",
            "oversized permission request",
        )
        .await;
        return cancelled();
    }

    let title = request
        .tool_call
        .fields
        .title
        .as_deref()
        .unwrap_or("the requested tool call")
        .replace(['@', '`'], "");
    let builder = CreateMessage::new()
        .content(format!("🔐 **permission requested** for **{title}**"))
        .components(components);
    info!(
        channel = ?channel,
        options = request.options.len(),
        timeout_ms = timeout.as_millis(),
        "sending permission request..."
    );
    let mut message = match channel.send_message(&context.http, builder).await {
        Ok(message) => {
            info!(
                channel = ?channel,
                message = ?message.id,
                "sent permission request"
            );
            message
        }
        Err(error) => {
            warn!(?error, ?channel, "failed to send permission request");
            return cancelled();
        }
    };

    wait_for_response(
        &context,
        channel,
        allowed_user,
        timeout,
        request,
        &mut message,
    )
    .await
}

/// Reports a permission-related message to the Discord channel.
async fn report(
    context: &Context,
    channel: GenericChannelId,
    message: impl Into<String>,
    event: &str,
) {
    debug!(?channel, event, "reporting permission event...");
    match channel.say(&context.http, message.into()).await {
        Ok(_) => debug!(?channel, event, "reported permission event"),
        Err(error) => warn!(?error, ?channel, event, "failed to report permission event"),
    }
}

/// Waits for a valid button interaction and disables the prompt afterwards.
async fn wait_for_response(
    context: &Context,
    channel: GenericChannelId,
    allowed_user: UserId,
    timeout: Duration,
    request: RequestPermissionRequest,
    message: &mut serenity::all::Message,
) -> RequestPermissionResponse {
    debug!(
        channel = ?channel,
        message = ?message.id,
        timeout_ms = timeout.as_millis(),
        "waiting for permission response..."
    );
    let mut interactions = message
        .id
        .collect_component_interactions(context)
        .timeout(timeout)
        .stream();
    while let Some(interaction) = interactions.next().await {
        if interaction.user.id != allowed_user {
            warn!(
                channel = ?channel,
                user = %interaction.user.id,
                "rejecting unauthorized permission interaction..."
            );
            reject_unauthorized(context, channel, &interaction).await;
            continue;
        }

        let index = interaction
            .data
            .custom_id
            .strip_prefix("agentcord:permission:")
            .and_then(|value| value.parse::<usize>().ok());
        let Some(option) = index.and_then(|index| request.options.get(index)) else {
            warn!(
                custom_id = %interaction.data.custom_id,
                ?channel,
                "ignoring malformed permission interaction"
            );
            continue;
        };

        info!(
            channel = ?channel,
            user = %interaction.user.id,
            option = index,
            "processing permission response..."
        );
        acknowledge(context, channel, &interaction, &option.name).await;
        return selected(option.option_id.clone());
    }

    info!(
        channel = ?channel,
        message = ?message.id,
        "marking permission request as timed out..."
    );
    mark_timed_out(context, channel, message).await;
    cancelled()
}

/// Rejects an interaction from a user who cannot answer the request.
async fn reject_unauthorized(
    context: &Context,
    channel: GenericChannelId,
    interaction: &serenity::all::ComponentInteraction,
) {
    debug!(
        channel = ?channel,
        user = %interaction.user.id,
        "sending unauthorized interaction response..."
    );
    match interaction
        .create_response(
            &context.http,
            CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("you are not allowed to answer this request")
                    .ephemeral(true),
            ),
        )
        .await
    {
        Ok(()) => debug!(
            channel = ?channel,
            user = %interaction.user.id,
            "sent unauthorized interaction response"
        ),
        Err(error) => warn!(
            ?error,
            ?channel,
            "failed to reject unauthorized permission interaction"
        ),
    }
}

/// Acknowledges a selected permission and removes its controls.
async fn acknowledge(
    context: &Context,
    channel: GenericChannelId,
    interaction: &serenity::all::ComponentInteraction,
    option_name: &str,
) {
    info!(
        channel = ?channel,
        user = %interaction.user.id,
        "updating permission request with response..."
    );
    match interaction
        .create_response(
            &context.http,
            CreateInteractionResponse::UpdateMessage(
                CreateInteractionResponseMessage::new()
                    .content(format!("permission response: **{option_name}**"))
                    .components(Vec::<CreateComponent>::new()),
            ),
        )
        .await
    {
        Ok(()) => info!(
            channel = ?channel,
            user = %interaction.user.id,
            "updated permission request with response"
        ),
        Err(error) => warn!(
            ?error,
            ?channel,
            "failed to acknowledge permission response"
        ),
    }
}

/// Marks a permission prompt denied after its interaction timeout.
async fn mark_timed_out(
    context: &Context,
    channel: GenericChannelId,
    message: &mut serenity::all::Message,
) {
    info!(
        channel = ?channel,
        message = ?message.id,
        "disabling timed-out permission request..."
    );
    match message
        .edit(
            &context.http,
            serenity::all::EditMessage::new()
                .content("permission request timed out and was denied")
                .components(Vec::<CreateComponent>::new()),
        )
        .await
    {
        Ok(()) => info!(
            channel = ?channel,
            message = ?message.id,
            "disabled timed-out permission request"
        ),
        Err(error) => warn!(
            ?error,
            ?channel,
            "failed to mark permission request as timed out"
        ),
    }
}

/// Chooses a persistent approval when available for `approve_all` mode.
pub fn approve_all(request: &RequestPermissionRequest) -> RequestPermissionResponse {
    let option = request
        .options
        .iter()
        .find(|option| option.kind == PermissionOptionKind::AllowAlways)
        .or_else(|| {
            request
                .options
                .iter()
                .find(|option| option.kind == PermissionOptionKind::AllowOnce)
        });
    option.map_or_else(cancelled, |option| selected(option.option_id.clone()))
}

/// Returns a cancelled permission response.
pub fn cancelled() -> RequestPermissionResponse {
    RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
}

/// Builds separate green and red button rows while respecting Discord limits.
fn permission_components(request: &RequestPermissionRequest) -> Vec<CreateComponent<'static>> {
    let mut allow_buttons = Vec::new();
    let mut deny_buttons = Vec::new();
    for (index, option) in request.options.iter().enumerate() {
        let button = CreateButton::new(format!("agentcord:permission:{index}"))
            .label(truncate(&option.name.to_lowercase(), 80))
            .style(option_style(option.kind));
        if is_allow(option.kind) {
            allow_buttons.push(button);
        } else {
            deny_buttons.push(button);
        }
    }

    let mut components = Vec::new();
    append_rows(&mut components, allow_buttons);
    append_rows(&mut components, deny_buttons);
    components
}

/// Appends action rows of at most five buttons to a component list.
fn append_rows(
    components: &mut Vec<CreateComponent<'static>>,
    buttons: Vec<CreateButton<'static>>,
) {
    let mut row = Vec::with_capacity(BUTTONS_PER_ROW);
    for button in buttons {
        row.push(button);
        if row.len() == BUTTONS_PER_ROW {
            components.push(CreateComponent::ActionRow(CreateActionRow::buttons(row)));
            row = Vec::with_capacity(BUTTONS_PER_ROW);
        }
    }
    if !row.is_empty() {
        components.push(CreateComponent::ActionRow(CreateActionRow::buttons(row)));
    }
}

/// Chooses green for allow options and red for all reject options.
const fn option_style(kind: PermissionOptionKind) -> ButtonStyle {
    if is_allow(kind) {
        ButtonStyle::Success
    } else {
        ButtonStyle::Danger
    }
}

/// Identifies options that authorize the requested operation.
const fn is_allow(kind: PermissionOptionKind) -> bool {
    matches!(
        kind,
        PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways
    )
}

/// Builds a selected permission response for one option ID.
fn selected(
    option_id: impl Into<agent_client_protocol::schema::v1::PermissionOptionId>,
) -> RequestPermissionResponse {
    RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
        SelectedPermissionOutcome::new(option_id),
    ))
}

/// Truncates a Discord button label without splitting Unicode characters.
fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut truncated = value
        .chars()
        .take(limit.saturating_sub(1))
        .collect::<String>();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{
        PermissionOption, PermissionOptionKind, RequestPermissionOutcome, RequestPermissionRequest,
        ToolCallUpdate, ToolCallUpdateFields,
    };

    use super::{approve_all, cancelled, permission_components, truncate};

    /// Builds a permission request for response-policy tests.
    fn request(options: Vec<PermissionOption>) -> RequestPermissionRequest {
        RequestPermissionRequest::new(
            "session",
            ToolCallUpdate::new("tool", ToolCallUpdateFields::new()),
            options,
        )
    }

    /// Verifies approve-all prefers a persistent allow option.
    #[test]
    fn approve_all_prefers_allow_always() {
        let response = approve_all(&request(vec![
            PermissionOption::new("once", "allow once", PermissionOptionKind::AllowOnce),
            PermissionOption::new("always", "allow always", PermissionOptionKind::AllowAlways),
        ]));
        let RequestPermissionOutcome::Selected(selected) = response.outcome else {
            panic!("approve-all should select an allow option");
        };
        assert_eq!(selected.option_id.to_string(), "always");
    }

    /// Verifies approve-all cancels when no allow option exists.
    #[test]
    fn approve_all_cancels_without_allow_option() {
        let response = approve_all(&request(vec![PermissionOption::new(
            "reject",
            "reject",
            PermissionOptionKind::RejectOnce,
        )]));
        assert_eq!(response, cancelled());
    }

    /// Verifies allow and reject choices become separate action rows.
    #[test]
    fn permission_components_group_allow_and_reject_buttons() {
        let components = permission_components(&request(vec![
            PermissionOption::new("reject", "REJECT", PermissionOptionKind::RejectOnce),
            PermissionOption::new("allow", "ALLOW", PermissionOptionKind::AllowOnce),
        ]));
        assert_eq!(components.len(), 2);
        let json = serde_json::to_value(&components).expect("serializable permission components");
        assert_eq!(json[0]["components"][0]["style"], 3);
        assert_eq!(
            json[0]["components"][0]["custom_id"],
            "agentcord:permission:1"
        );
        assert_eq!(json[0]["components"][0]["label"], "allow");
        assert_eq!(json[1]["components"][0]["style"], 4);
        assert_eq!(
            json[1]["components"][0]["custom_id"],
            "agentcord:permission:0"
        );
        assert_eq!(json[1]["components"][0]["label"], "reject");
    }

    /// Verifies button labels are truncated at the Discord limit.
    #[test]
    fn truncate_preserves_unicode_boundaries() {
        assert_eq!(truncate("😀😀😀", 2), "😀…");
    }
}
