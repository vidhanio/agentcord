use std::time::Duration;

use agent_client_protocol::schema::v1::{
    RequestPermissionOutcome, RequestPermissionRequest, RequestPermissionResponse,
    SelectedPermissionOutcome,
};
use serenity::{
    all::{
        ButtonStyle, Context, CreateButton, CreateInteractionResponse,
        CreateInteractionResponseMessage, CreateMessage, GenericChannelId, UserId,
    },
    collector::CollectComponentInteractions,
    futures::StreamExt,
};

pub async fn ask(
    ctx: Context,
    channel: GenericChannelId,
    allowed_user: UserId,
    timeout: Duration,
    request: RequestPermissionRequest,
) -> RequestPermissionResponse {
    if request.options.is_empty() || request.options.len() > 25 {
        let _ = channel
            .say(
                &ctx.http,
                format!(
                    "permission request cancelled: ACP supplied {} choices, but Discord supports 1–25",
                    request.options.len()
                ),
            )
            .await;
        return cancelled();
    }
    let title = request
        .tool_call
        .fields
        .title
        .as_deref()
        .unwrap_or("the requested tool call");
    let mut builder = CreateMessage::new().content(format!(
        "🔐 **permission requested** for **{}**",
        title.replace(['@', '`'], "")
    ));
    for (index, option) in request.options.iter().enumerate() {
        let style = match option.kind {
            agent_client_protocol::schema::v1::PermissionOptionKind::AllowOnce
            | agent_client_protocol::schema::v1::PermissionOptionKind::AllowAlways => {
                ButtonStyle::Success
            }
            _ => ButtonStyle::Danger,
        };
        builder = builder.button(
            CreateButton::new(format!("agentcord:permission:{index}"))
                .label(truncate(&option.name, 80))
                .style(style),
        );
    }
    let Ok(mut message) = channel.send_message(&ctx.http, builder).await else {
        return cancelled();
    };

    let mut interactions = message
        .id
        .collect_component_interactions(&ctx)
        .timeout(timeout)
        .stream();
    while let Some(interaction) = interactions.next().await {
        if interaction.user.id != allowed_user {
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
        let index = interaction
            .data
            .custom_id
            .rsplit(':')
            .next()
            .and_then(|value| value.parse::<usize>().ok());
        let Some(option) = index.and_then(|index| request.options.get(index)) else {
            continue;
        };
        let _ = interaction
            .create_response(
                &ctx.http,
                CreateInteractionResponse::UpdateMessage(
                    CreateInteractionResponseMessage::new()
                        .content(format!("permission response: **{}**", option.name))
                        .components(vec![]),
                ),
            )
            .await;
        return RequestPermissionResponse::new(RequestPermissionOutcome::Selected(
            SelectedPermissionOutcome::new(option.option_id.clone()),
        ));
    }

    let _ = message
        .edit(
            &ctx.http,
            serenity::all::EditMessage::new()
                .content("permission request timed out and was denied")
                .components(vec![]),
        )
        .await;
    cancelled()
}

fn cancelled() -> RequestPermissionResponse {
    RequestPermissionResponse::new(RequestPermissionOutcome::Cancelled)
}

fn truncate(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut truncated = value.chars().take(limit - 1).collect::<String>();
    truncated.push('…');
    truncated
}
