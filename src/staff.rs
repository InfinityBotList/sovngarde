use poise::serenity_prelude as serenity;

use std::fmt::Write as _; 
// import without risk of name clashing
use serenity::id::UserId;

use crate::checks;

type Error = crate::Error;
type Context<'a> = crate::Context<'a>;

/// Staff base command
#[poise::command(
    prefix_command,
    slash_command,
    guild_cooldown = 10,
    subcommands("staff_list", "staff_recalc", "staff_add", "staff_del")
)]
pub async fn staff(ctx: Context<'_>) -> Result<(), Error> {
    ctx.say("Available options are ``staff list``, ``staff recalc`` (dev/admin only), ``staff add`` (dev/admin only)").await?;
    Ok(())
}

#[poise::command(rename = "list", track_edits, prefix_command, slash_command, check = "checks::staff_server")]
pub async fn staff_list(ctx: Context<'_>) -> Result<(), Error> {
    // Get list of users with staff flag set to true
    let data = ctx.data();

    let staffs = sqlx::query!(
        "SELECT user_id, username, admin, ibldev FROM users WHERE staff = true ORDER BY user_id ASC"
    )
    .fetch_all(&data.pool)
    .await?;

    let mut staff_list = "**Staff List**\n".to_string();
    let mut not_in_staff_server = "**Not in staff server (based on cache, may be inaccurate)**\n".to_string();

    let guild = ctx.guild().unwrap();

    for staff in staffs.iter() {
        // Convert ID to u64
        let user_id = staff.user_id.parse::<u64>()?;

        let cache_user = ctx.discord().cache.member(guild.id, UserId(user_id));

        let user = match cache_user {
            Some(user) => user.user,
            None => {
                // User not found in cache, fetch from API
                let user = UserId(user_id).to_user(&ctx.discord().http).await?;

                write!(not_in_staff_server, "{} ({})", user.id.0, user.name)?;
                user
            }
        };

        writeln!(staff_list, "{} ({}) [admin={}, ibldev={}]", staff.user_id, user.name, staff.admin, staff.ibldev)?;
    }

    ctx.say(staff_list + "\n" + &not_in_staff_server).await?;

    Ok(())
}

/// Recalculates the list of staff and developers based on their roles
#[poise::command(rename = "recalc", track_edits, prefix_command, slash_command, check = "checks::is_admin_dev")]
pub async fn staff_recalc(ctx: Context<'_>) -> Result<(), Error> {
    if !checks::staff_server(ctx).await? {
        return Ok(())
    }

    // Ask if the user truly wishes to continue
    let mut msg = ctx.send(|m| {
        m.content(r#"
Continuing will change the PostgreSQL database and recalculate the list of staff/admins/developers based on their roles. This is dangerous but sometimes needed for the manager bot to work correctly!

**Current recalculation system**

``Staff Manager`` = admin flag
``Head Developer`` = iblhdev flag
``Developer`` | ``Head Developer`` = ibldev flag
``Website Moderator`` = staff flag

**This only affects v4 and later**

During beta testing, this is available to admins and devs, but once second final migration happens, it will only be available for Toxic Dev
"#)
        .components(|c| {
            c.create_action_row(|r| {
                r.create_button(|b| {
                    b.custom_id("continue")
                    .label("Continue")
                })
                .create_button(|b| {
                    b.custom_id("cancel")
                        .label("Cancel")
                        .style(serenity::ButtonStyle::Danger)
                })
            })
        })
    })
    .await?
    .message()
    .await?;

    let interaction = msg
        .await_component_interaction(ctx.discord())
        .author_id(ctx.author().id)
        .await;
        msg.edit(ctx.discord(), |b| b.components(|b| b)).await?; // remove buttons after button press

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
    } else {
        ctx.say("Recalculating staff list").await?;

        // Get all members on staff server
        let guild = ctx.guild().unwrap();

        let dev_role = poise::serenity_prelude::RoleId(std::env::var("DEV_ROLE")?.parse::<u64>()?);
        let head_dev_role = poise::serenity_prelude::RoleId(std::env::var("HEAD_DEV_ROLE")?.parse::<u64>()?);
        let staff_man_role = poise::serenity_prelude::RoleId(std::env::var("STAFF_MAN_ROLE")?.parse::<u64>()?);
        let web_mod_role = poise::serenity_prelude::RoleId(std::env::var("WEB_MOD_ROLE")?.parse::<u64>()?);

        // First unset all staff
        sqlx::query!("UPDATE users SET staff = false, ibldev = false, admin = true")
            .execute(&ctx.data().pool)
            .await?;

        for member in guild.members.values() {
            if member.roles.contains(&dev_role) {
                sqlx::query!("UPDATE users SET staff = true, ibldev = true WHERE user_id = $1", member.user.id.0.to_string())
                    .execute(&ctx.data().pool)
                    .await?;
            } else if member.roles.contains(&head_dev_role) {
                sqlx::query!("UPDATE users SET staff = true, ibldev = true, iblhdev = true WHERE user_id = $1", member.user.id.0.to_string())
                    .execute(&ctx.data().pool)
                    .await?;
            } else if member.roles.contains(&staff_man_role) {
                sqlx::query!("UPDATE users SET staff = true, admin = true WHERE user_id = $1", member.user.id.0.to_string())
                    .execute(&ctx.data().pool)
                    .await?;
            } else if member.roles.contains(&web_mod_role) {
                sqlx::query!("UPDATE users SET staff = true WHERE user_id = $1", member.user.id.0.to_string())
                    .execute(&ctx.data().pool)
                    .await?;
            }
        }

        ctx.say("Recalculated staff list").await?;
    }

    Ok(())
}

/// Adds a new staff member
#[poise::command(rename = "add", track_edits, prefix_command, slash_command, check = "checks::is_admin_dev")]
pub async fn staff_add(
    ctx: Context<'_>, 
    #[description = "The user ID of the user to add"]
    member: serenity::Member) -> Result<(), Error> {
    if !checks::staff_server(ctx).await? {
        return Ok(())
    }

    let web_mod_role = poise::serenity_prelude::RoleId(std::env::var("WEB_MOD_ROLE")?.parse::<u64>()?);

    if !member.roles.contains(&web_mod_role) {
        return Err(format!("{} is not a web moderator", member.user.name).into());
    }

    sqlx::query!("UPDATE users SET staff = true WHERE user_id = $1", member.user.id.0.to_string())
        .execute(&ctx.data().pool)
        .await?;
    
    ctx.say(&format!("Added {} to the staff list (if they weren't already staff)", member.user.name)).await?;

    Ok(())
}

/// Removes a staff member
#[poise::command(rename = "del", track_edits, prefix_command, slash_command, check = "checks::is_admin_dev")]
pub async fn staff_del(
    ctx: Context<'_>, 
    #[description = "The user ID of the user to remove staff from"]
    member: serenity::Member) -> Result<(), Error> {
    if !checks::staff_server(ctx).await? {
        return Ok(())
    }

    let staff_man_role = poise::serenity_prelude::RoleId(std::env::var("STAFF_MAN_ROLE")?.parse::<u64>()?);
    let owner_role = poise::serenity_prelude::RoleId(std::env::var("OWNER_ROLE")?.parse::<u64>()?);


    if member.user.id == ctx.author().id {
        // Don't error, just let them know how stupid they are
        ctx.say("Removing yourselves from staff eh? Well I'll do it since you asked so nicely :heart:").await?;
    } else if member.roles.contains(&staff_man_role) {
        return Err(format!("{} is a staff manager and as such is protected!", member.user.name).into());
    } else if member.roles.contains(&owner_role) {
        return Err(format!("{} is a owner and as such is protected!", member.user.name).into());
    }

    sqlx::query!("UPDATE users SET staff = false, ibldev = false, admin = false WHERE user_id = $1", member.user.id.0.to_string())
        .execute(&ctx.data().pool)
        .await?;
    
    member.guild_id.kick_with_reason(&ctx.discord().http, member.user.id, "Removed from staff list").await?;

    ctx.say("Removed from staff list").await?;

    Ok(())
}