use crate::checks;
use crate::impls::actions::add_action_log;
use crate::Context;
use crate::Error;
use poise::serenity_prelude::ButtonStyle;
use poise::serenity_prelude::CreateActionRow;
use poise::serenity_prelude::CreateButton;
use poise::serenity_prelude::CreateEmbed;
use poise::serenity_prelude::CreateInteractionResponseMessage;
use poise::serenity_prelude::CreateMessage;
use poise::CreateReply;

use poise::serenity_prelude as serenity;

/// Onboarding base command
#[poise::command(
    category = "Admin",
    prefix_command,
    slash_command,
    guild_cooldown = 10,
    subcommands("approveonboard", "denyonboard", "resetonboard",)
)]
pub async fn onboardman(ctx: Context<'_>) -> Result<(), Error> {
    ctx.say("Some available options are ``onboardman approve`` etc.")
        .await?;
    Ok(())
}

/// Allows managers to onboard users
#[poise::command(
    rename = "approve",
    category = "Admin",
    track_edits,
    prefix_command,
    slash_command,
    check = "checks::is_hdev_hadmin",
    check = "checks::staff_server"
)]
pub async fn approveonboard(
    ctx: Context<'_>,
    #[description = "The staff id"] member: serenity::User,
) -> Result<(), Error> {
    let data = ctx.data();

    // Check onboard state of user
    let onboard_state = sqlx::query!(
        "SELECT staff_onboard_state FROM users WHERE user_id = $1",
        member.id.to_string()
    )
    .fetch_one(&data.pool)
    .await?;

    if onboard_state.staff_onboard_state
        != crate::onboarding::OnboardState::PendingManagerReview.as_str()
        && onboard_state.staff_onboard_state != crate::onboarding::OnboardState::Denied.as_str()
    {
        return Err(format!(
            "User is not pending manager review and currently has state of: {}",
            onboard_state.staff_onboard_state
        )
        .into());
    }

    // Update onboard state of user
    sqlx::query!(
        "UPDATE users SET staff_onboard_state = $1 WHERE user_id = $2",
        crate::onboarding::OnboardState::Completed.as_str(),
        member.id.to_string()
    )
    .execute(&data.pool)
    .await?;

    // DM user that they have been approved
    let _ = member.dm(
        &ctx.discord().http,
        CreateMessage::new()
        .content("Your onboarding request has been approved. You may now begin approving/denying bots") 
    ).await?;

    ctx.say("Onboarding request approved!").await?;

    Ok(())
}

/// Denies onboarding requests
#[poise::command(
    rename = "deny",
    category = "Admin",
    track_edits,
    prefix_command,
    slash_command,
    check = "checks::is_hdev_hadmin",
    check = "checks::staff_server"
)]
pub async fn denyonboard(
    ctx: crate::Context<'_>,
    #[description = "The staff id"] user: serenity::User,
) -> Result<(), Error> {
    let data = ctx.data();

    // Check onboard state of user
    let onboard_state = sqlx::query!(
        "SELECT staff_onboard_state FROM users WHERE user_id = $1",
        user.id.to_string()
    )
    .fetch_one(&data.pool)
    .await?;

    if onboard_state.staff_onboard_state
        != crate::onboarding::OnboardState::PendingManagerReview.as_str()
    {
        return Err(format!(
            "User is not pending manager review and currently has state of: {}",
            onboard_state.staff_onboard_state
        )
        .into());
    }

    // Update onboard state of user
    sqlx::query!(
        "UPDATE users SET staff_onboard_state = $1 WHERE user_id = $2",
        crate::onboarding::OnboardState::Denied.as_str(),
        user.id.to_string()
    )
    .execute(&data.pool)
    .await?;

    // DM user that they have been denied
    let _ = user.dm(&ctx.discord().http, CreateMessage::new().content("Your onboarding request has been denied. Please contact a manager for more information")).await?;

    ctx.say("Onboarding request denied!").await?;

    Ok(())
}

/// Resets a onboarding to force a new one
#[poise::command(
    rename = "reset",
    category = "Admin",
    track_edits,
    prefix_command,
    slash_command,
    check = "checks::is_hdev_hadmin",
    check = "checks::staff_server"
)]
pub async fn resetonboard(
    ctx: crate::Context<'_>,
    #[description = "The staff id"] user: serenity::User,
) -> Result<(), Error> {
    let data = ctx.data();

    let builder = CreateReply::new()
        .content("Are you sure you wish to reset this user's onboard state and force them to redo onboarding?")
        .components(
            vec![
                CreateActionRow::Buttons(
                    vec![
                        CreateButton::new("continue").label("Continue").style(serenity::ButtonStyle::Primary),
                        CreateButton::new("cancel").label("Cancel").style(serenity::ButtonStyle::Danger),
                    ]
                )
            ]
        );

    let mut msg = ctx.send(builder.clone()).await?.into_message().await?;

    let interaction = msg
        .await_component_interaction(ctx.discord())
        .author_id(ctx.author().id)
        .await;

    msg.edit(ctx.discord(), builder.to_prefix_edit().components(vec![]))
        .await?; // remove buttons after button press

    let pressed_button_id = match &interaction {
        Some(m) => &m.data.custom_id,
        None => {
            ctx.say("You didn't interact in time").await?;
            return Ok(());
        }
    };

    if pressed_button_id == "cancel" {
        ctx.say("Cancelled").await?;
        return Ok(());
    }

    // Update onboard state of user
    sqlx::query!(
        "UPDATE users SET staff_onboard_guild = NULL, staff_onboard_state = $1, staff_onboard_last_start_time = NOW() WHERE user_id = $2",
        crate::onboarding::OnboardState::Pending.as_str(),
        user.id.to_string()
    )
    .execute(&data.pool)
    .await?;

    // DM user that they have been force reset
    let _ = user.dm(&ctx.discord().http, CreateMessage::new().content("Your onboarding request has been force reset. Please contact a manager for more information. You will, in most cases, need to redo onboarding")).await?;

    ctx.say("Onboarding request reset!").await?;

    Ok(())
}

/// Unlocks RPC for a 10 minutes, is logged
#[poise::command(category = "Admin", track_edits, prefix_command, slash_command)]
pub async fn rpcunlock(
    ctx: crate::Context<'_>,
    #[description = "Purpose"] purpose: String,
) -> Result<(), Error> {
    let warn_embed = {
        CreateEmbed::new()
        .title(":warning: Warning")
        .description(
            format!("**You are about to unlock full access to the RPC API for 10 minutes on your account (required by some parts of our staff panel)**

While RPC is unlocked, any leaks or bugs have a higher change in leading to data being destroyed and mass-nukes to potentially occur although the API does try to protect against it using ratelimits etc.!

To continue, please click the `Unlock` button OR instead, (PREFERRED) just use bot commands instead (where permitted).

**Given Reason:** {}
            ", 
            purpose)
        )
        .color(0xFF0000)
    };

    let msg = ctx
        .send(
            CreateReply::new()
                .embed(warn_embed)
                .components(vec![CreateActionRow::Buttons(vec![
                    CreateButton::new("a:unlock")
                        .style(ButtonStyle::Primary)
                        .label("Unlock"),
                    CreateButton::new("a:cancel")
                        .style(ButtonStyle::Danger)
                        .label("Cancel"),
                ])]),
        )
        .await?
        .into_message()
        .await?;

    let interaction = msg
        .await_component_interaction(ctx.discord())
        .author_id(ctx.author().id)
        .await;

    if let Some(item) = interaction {
        let custom_id = &item.data.custom_id;

        if custom_id == "a:cancel" {
            item.delete_response(ctx.discord()).await?;
        } else if custom_id == "a:unlock" {
            add_action_log(
                &ctx.data().pool,
                &crate::config::CONFIG.test_bot.to_string(),
                &ctx.author().id.to_string(),
                &purpose,
                "rpc_unlock",
            )
            .await?;

            sqlx::query!(
                "UPDATE users SET staff_rpc_last_verify = NOW() WHERE user_id = $1",
                ctx.author().id.to_string()
            )
            .execute(&ctx.data().pool)
            .await?;

            item.create_response(&ctx.discord(), serenity::CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::default()
                .content("RPC unlocked")
            )).await?;
        }
    }

    Ok(())
}

/// Locks RPC
#[poise::command(category = "Admin", track_edits, prefix_command, slash_command)]
pub async fn rpclock(ctx: crate::Context<'_>) -> Result<(), Error> {
    sqlx::query!(
        "UPDATE users SET staff_rpc_last_verify = NOW() - interval '1 hour' WHERE user_id = $1",
        ctx.author().id.to_string()
    )
    .execute(&ctx.data().pool)
    .await?;

    ctx.say("RPC has been locked").await?;

    Ok(())
}
