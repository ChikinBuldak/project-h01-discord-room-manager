use std::collections::hash_map::Entry;

use serenity::all::{CreateInvite, InviteTargetType};

use crate::types::{
    Data, Error, PoiseContext, Room, RoomBroadcaster, RoomSerialize, ServerMessage,
    SharedRoomState, UserIdStr, UserVoiceMap, VoiceChannelMap,
};
use poise::serenity_prelude as serenity;

pub async fn voice_state_update(
    _ctx: &serenity::Context,
    old: &Option<serenity::VoiceState>,
    new: &serenity::VoiceState,
    data: &Data,
) -> Result<(), Error> {
    let guild_id = match new.guild_id {
        Some(guild_id) => guild_id,
        None => return Ok(()), // Ignore DMs or non-guild voice states
    };

    let user_id = new.user_id;

    let mut voice_map = data.voice_map.write().await;
    let mut user_voice_map = data.user_voice_map.write().await;
    let room_state = data.room_state.clone();
    let room_broadcasters = data.room_broadcasters.clone();

    // Handle user leaving a channel (from old state or our tracking)
    if let Some(old_state) = old {
        if let Some(old_channel_id) = old_state.channel_id {
            remove_user_from_voice_channel(
                &mut voice_map,
                &mut user_voice_map,
                &room_state,
                &room_broadcasters,
                guild_id,
                old_channel_id,
                user_id,
            );
        }
    } else if let Some(&(prev_guild_id, prev_channel_id)) = user_voice_map.get(&user_id) {
        // If no old state but we have tracking data, remove from previous channel
        if prev_guild_id == guild_id {
            remove_user_from_voice_channel(
                &mut voice_map,
                &mut user_voice_map,
                &room_state,
                &room_broadcasters,
                guild_id,
                prev_channel_id,
                user_id,
            );
        }
    }

    // Handle user joining a new channel
    if let Some(new_channel_id) = new.channel_id {
        add_user_to_voice_channel(
            &mut voice_map,
            &mut user_voice_map,
            guild_id,
            new_channel_id,
            user_id,
        );
    }

    println!(
        "Voice state updated - User: {}, Guild: {}, Channel: {:?}",
        user_id, guild_id, new.channel_id
    );

    Ok(())
}

fn remove_user_from_voice_channel(
    voice_map: &mut VoiceChannelMap,
    user_voice_map: &mut UserVoiceMap,
    room_state: &SharedRoomState,
    room_broadcasters: &RoomBroadcaster,
    guild_id: serenity::GuildId,
    channel_id: serenity::ChannelId,
    user_id: serenity::UserId,
) {
    // Remove from voice_map
    if let Some(channels) = voice_map.map.get_mut(&guild_id) {
        if let Some(members) = channels.get_mut(&channel_id) {
            members.remove(&user_id);
            if members.is_empty() {
                channels.remove(&channel_id);
            }
        }
        if channels.is_empty() {
            voice_map.map.remove(&guild_id);
        }
    }

    // Remove from user_voice_map
    user_voice_map.remove(&user_id);
    println!(
        "Removed user {} from voice tracking in guild {}",
        user_id, guild_id
    );

    // Remove from activity map if present
    let room_state = room_state.clone();
    let room_broadcasters = room_broadcasters.clone();

    // We spawn a new task to handle this so we don't block
    // the event handler with lock-in-lock awaits.
    tokio::spawn(async move {
        let user_id_str: UserIdStr = user_id.to_string().into();

        let mut room_state = room_state.write().await;
        if let Some(room_id) = room_state.user_map.get(&user_id_str) {
            // Only clean up if it's a "discord" room.
            // General rooms are unaffected by VC status.
            if !room_id.starts_with("discord_") {
                return;
            }

            println!(
                "User {} left VC, finding discord_ room {} to clean up...",
                user_id, room_id
            );

            let room_id_clone = room_id.clone();

            // Remove from user_map
            room_state.user_map.remove(&user_id_str);

            match room_state.room_map.entry(room_id_clone) {
                Entry::Occupied(mut entry) => {
                    {
                        let room = entry.get_mut();
                        // Remove the user from the room's members list
                        room.members.remove(&user_id_str);
                    }

                    let room = entry.get();
                    println!(
                        "...removed user {} from room '{}'. {} members left.",
                        user_id,
                        room.name,
                        room.members.len()
                    );

                    // broadcast
                    let broadcasters = room_broadcasters.read().await;
                    if let Some(tx) = broadcasters.get(entry.key()) {
                        let serialized_room = RoomSerialize::from(room); // from &mut Room
                        tx.send(ServerMessage::RoomState(serialized_room)).ok();
                    }
                    drop(broadcasters);
                    if room.members.is_empty() {
                        let room = entry.remove();
                        println!("...room '{}' is empty, deleting.", room.name);
                        let mut broadcasters_write = room_broadcasters.write().await;
                        broadcasters_write.remove(&room.room_id);
                        println!("...deleting broadcaster for room '{}'", room.name);
                    }
                }
                Entry::Vacant(_) => {
                    println!("Room not found.");
                }
            }
        }
    });
}

fn add_user_to_voice_channel(
    voice_map: &mut crate::types::VoiceChannelMap,
    user_voice_map: &mut crate::types::UserVoiceMap,
    guild_id: serenity::GuildId,
    channel_id: serenity::ChannelId,
    user_id: serenity::UserId,
) {
    // Add to voice_map
    let channels = voice_map.map.entry(guild_id).or_default();
    let members = channels.entry(channel_id).or_default();
    members.insert(user_id);

    // Add to user_voice_map
    user_voice_map.insert(user_id, (guild_id, channel_id));
    println!(
        "Added user {} to channel {} in guild {}",
        user_id, channel_id, guild_id
    );
}

/// Launch the 2D fighting Game Activity in your current voice channel.
#[poise::command(slash_command)]
pub async fn play(ctx: PoiseContext<'_>) -> Result<(), Error> {
    let application_id = ctx.data().application_id;
    let guild_id = ctx
        .guild_id()
        .ok_or("This command can only be used in a server.")?;
    let author_id = ctx.author().id;

    // Clone HTTP client before any await
    let http = ctx.serenity_context().http.clone();

    // Check voice state using YOUR custom tracking data (not cache)
    let channel_id = {
        let user_voice_map = ctx.data().user_voice_map.read().await;
        user_voice_map
            .get(&author_id)
            .and_then(|&(user_guild_id, user_channel_id)| {
                if user_guild_id == guild_id {
                    Some(user_channel_id)
                } else {
                    None
                }
            })
    };

    let channel_id = match channel_id {
        Some(channel_id) => channel_id,
        None => {
            ctx.say("You must be in a voice channel to start an activity.")
                .await?;
            return Ok(());
        }
    };

    // Create the invite builder directly
    let invite_builder = CreateInvite::new()
        .target_application_id(application_id)
        .target_type(InviteTargetType::EmbeddedApplication)
        .max_uses(100)
        .max_age(86400);

    let invite = channel_id.create_invite(&http, invite_builder).await?;

    ctx.say(format!(
        "Join the activity: https://discord.gg/{}",
        invite.code
    ))
    .await?;

    Ok(())
}

#[poise::command(slash_command)]
pub async fn ping(ctx: PoiseContext<'_>) -> Result<(), Error> {
    ctx.say("Pong!").await?;
    Ok(())
}
