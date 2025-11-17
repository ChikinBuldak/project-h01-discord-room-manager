use std::{sync::Arc, time::Duration};

use axum::{
    Json,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message as WsMessage, WebSocket},
    },
    response::Response,
};
use chrono::Utc;
use futures_util::{StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use serenity::all::{ChannelId, GuildId, UserId};
use tokio::{sync::broadcast, time::{Instant, sleep}};

use crate::{
    types::{
        AppState, AuthInfoType, ClientMessage, Room, RoomId, RoomSerialize,
        ServerMessage, UserIdStr,
    },
    utils::generate_room_id,
};

/// How long an empty room can exist before being cleaned up.
const ROOM_TTL: Duration = Duration::from_secs(5);
/// How long a client can be silent before being disconnected.
const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(30);

// ===================================
// ============= PAYLOADS ============
// ===================================

#[derive(Debug, Serialize)]
/// Payload for getting all rooms available on the server
pub struct GetAllRoomsPayload {
    pub room_id: String,
    pub name: String,
    pub owner_id: String,
    pub created_at: String,
    pub max_capacity: usize,
    pub number_of_members: usize,
}

impl From<&Room> for GetAllRoomsPayload {
    fn from(room: &Room) -> Self {
        let owner_id = room.owner_id.to_string();
        let created_at = room.created_at.to_string();
        let max_capacity = room.max_capacity;
        let numbers_of_members = room.members.len();
        let room_name = room.name.to_string();

        Self {
            room_id: room.room_id.to_string(),
            name: room_name,
            owner_id,
            created_at,
            max_capacity,
            number_of_members: numbers_of_members,
        }
    }
}

// ================================================
// ============= REST API HANDLERS=================
// ================================================

/// Get All rooms Available on the server
pub async fn get_all_rooms(State(state): State<Arc<AppState>>) -> Json<Vec<GetAllRoomsPayload>> {
    let room_state = state.room_state.read().await;
    let rooms: Vec<GetAllRoomsPayload> = room_state
        .room_map
        .values()
        .map(|room| GetAllRoomsPayload::from(room))
        .collect();

    Json(rooms)
}

// ===============================================================
// ============= WAITING ROOM WEBSOCKET HANDLERS =================
// ===============================================================

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(mut ws: WebSocket, state: Arc<AppState>) {
    println!("New WebSocket connection established. Awaiting validation...");

    let mut user_id: Option<UserIdStr> = None;
    let mut current_room_id: Option<RoomId> = None;
    let mut current_broadcaster_sub: Option<broadcast::Receiver<ServerMessage>> = None;

    let mut last_heartbeat = Instant::now();

    // Wait for client messages (Create, Join, Ping, etc)
    loop {
        tokio::select! {
            _ = sleep(HEARTBEAT_TIMEOUT) => {
                if Instant::now().duration_since(last_heartbeat) >= HEARTBEAT_TIMEOUT {
                    println!("Client heartbeat timed out. Closing connection.");
                    break;
                }
            }
            // Listen for messages from the client (ws.next())
            Some(message) = ws.next() => {
                last_heartbeat = Instant::now();

                let message = match message {
                    Ok(WsMessage::Text(t)) => t,
                    Ok(WsMessage::Close(_)) => {
                        println!("Client closed connection.");
                        break;
                    }
                    Ok(WsMessage::Ping(_)) => {
                        // The framework will automatically send a Pong back.
                        // We just reset our timer.
                        println!("Received WebSocket Ping");
                        continue;
                    }
                    Ok(WsMessage::Pong(_)) => {
                        // We received a pong (likely in response to a server-sent ping)
                        println!("Received WebSocket Pong");
                        continue;
                    }
                    Err(e) => {
                        println!("WebSocket error: {}", e);
                        break;
                    }
                    _ => continue,
                };

                let client_message = match serde_json::from_str(&message) {
                    Ok(m) => m,
                    Err(e) => {
                        send_message(
                            &mut ws,
                            ServerMessage::Error {
                                message: format!("Invalid message: {e}"),
                            },
                        )
                        .await;
                        continue;
                    }
                };

                // Logic for authenticated user
                if user_id.is_some() {
                     match client_message {
                        ClientMessage::LeaveRoom { room_id } => {
                            let Some(uid) = &user_id else { continue; };
                            println!("User {} is gracefully leaving room {}", uid, room_id);
                            current_room_id = Some(room_id.into());
                            break;
                        }
                        ClientMessage::Ping => {
                            send_message(&mut ws, ServerMessage::Pong).await;
                        },
                        // Disallow joining/creating another room
                        _ => {
                            send_message(&mut ws, ServerMessage::Error { message: "Already in a room.".to_string() }).await;
                        }
                    }
                    continue;
                }

                // Handle the client message
                match client_message {
                    ClientMessage::Connect {auth, room_id} => {
                        println!("Received Connect request for room '{}'", room_id);

                        // Validate AUTH
                        let validated_user_id = match validate_auth_info(auth, &state).await {
                            Ok(id) => id,
                            Err(msg) => {
                                send_message(
                                    &mut ws,
                                    ServerMessage::Error {
                                        message: msg,
                                    },
                                )
                                .await;
                                break;
                            }
                        };

                        println!("...User {} validated via Connect. Attempting to join room {}...", validated_user_id, room_id);

                        let room_id_arc: Arc<str> = room_id.into();
                        let mut room_state = state.room_state.write().await;

                        if room_state.user_map.contains_key(&validated_user_id) {
                            send_message(
                                &mut ws,
                                ServerMessage::Error {
                                    message: "User is already in a room.".to_string(),
                                },
                            )
                            .await;
                            break;
                        }

                        let room = match room_state.room_map.get_mut(&room_id_arc) {
                            Some(room) => {
                                if room.members.len() >= room.max_capacity {
                                    send_message(&mut ws, ServerMessage::Error { message: "Room is full".to_string()}).await;
                                    break;
                                }

                                // Add user to room
                                room.members.insert(validated_user_id.clone());
                                room.clone()
                            },
                            None => {
                                send_message(
                                    &mut ws,
                                    ServerMessage::Error {
                                        message: "Room not found".to_string(),
                                    },
                                )
                                .await;
                                break;
                            }
                        };

                        room_state.user_map.insert(validated_user_id.clone(), room_id_arc.clone());
                        user_id = Some(validated_user_id.clone());
                        current_room_id = Some(room_id_arc.clone());

                        drop(room_state);
                        // Get broadcaster
                        let mut broadcasters = state.room_broadcasters.write().await;
                        let sub = match broadcasters.get(&room_id_arc) {
                            Some(tx) => tx.subscribe(),
                            None => {
                                println!("..Recreated missing broadcaster for room {}", room_id_arc);
                                let (tx, rx) = broadcast::channel(16);
                                broadcasters.insert(room_id_arc.clone(), tx.clone());
                                rx
                            }
                        };

                        current_broadcaster_sub = Some(sub);

                        let broadcaster_tx = broadcasters.get(&room_id_arc).cloned();
                        drop(broadcasters);

                        let serializable_room = RoomSerialize::from(&room);
                        send_message(&mut ws, ServerMessage::RoomState(serializable_room.clone())).await;

                        // Broadcast new state
                        if let Some(tx) = broadcaster_tx {
                            println!("Broadcasting room state update for room {}", room.room_id);
                            tx.send(ServerMessage::RoomState(serializable_room)).ok();
                        }

                        println!("User {} successfully joined room {}", validated_user_id, room.name);
                    },
                    ClientMessage::LeaveRoom { room_id } => {
                        let Some(uid) = &user_id else {
                            send_message(
                                &mut ws,
                                ServerMessage::Error {
                                    message: "Not connected. Send a Connect message first.".to_string(),
                                },
                            )
                            .await;
                            continue;
                        };
                        println!("User {} is gracefully leaving room {}", uid, room_id);
                        current_room_id = Some(room_id.into());
                        // Break the loop to trigger the cleanup logic
                        break;
                    }
                    ClientMessage::CreateRoom {auth, max_capacity, name} => {
                        println!("Received CreateRoom request for '{}'", name);
                            let validated_user_id = match validate_auth_info(auth.clone(), &state).await {
                                Ok(id) => id,
                                Err(msg) => {
                                    send_message(&mut ws, ServerMessage::Error { message: msg }).await;
                                    break;
                                }
                            };

                            let (room_id_str, user_id_str): (String, Arc<str>) = match auth {
                                AuthInfoType::Discord { guild_id, channel_id, .. } => 
                                    (format!("discord-{}_{}", guild_id, channel_id), validated_user_id.clone()),
                                AuthInfoType::General { .. } => 
                                    (format!("general-{}", generate_room_id()), validated_user_id.clone()),
                            };
                            let room_id_arc: Arc<str> = room_id_str.into();
                            let created_at = Utc::now().to_rfc3339();

                            let mut room_state = state.room_state.write().await;
                            
                            if room_state.user_map.contains_key(&validated_user_id) {
                                send_message(&mut ws, ServerMessage::Error { message: "User is already in a room.".to_string() }).await;
                                break;
                            }
                            if room_state.room_map.contains_key(&room_id_arc) {
                                 send_message(&mut ws, ServerMessage::Error { message: "Room already exists.".to_string() }).await;
                                break;
                            }
                            
                            let new_room = Room {
                                room_id: room_id_arc.clone(),
                                name: name.clone().into(),
                                owner_id: user_id_str.clone(),
                                created_at: created_at.clone(),
                                max_capacity,
                                members: [user_id_str.clone()].iter().cloned().collect(),
                            };

                            // 5. Update state
                            room_state.room_map.insert(room_id_arc.clone(), new_room.clone());
                            room_state.user_map.insert(user_id_str.clone(), room_id_arc.clone());
                            
                            // 6. Set session variables
                            user_id = Some(user_id_str.clone());
                            current_room_id = Some(room_id_arc.clone());
                            
                            // 7. Create broadcaster
                            drop(room_state); // Drop write lock
                            let mut broadcasters = state.room_broadcasters.write().await;
                            let (tx, rx) = broadcast::channel(16);
                            broadcasters.insert(room_id_arc.clone(), tx);
                            current_broadcaster_sub = Some(rx);
                            drop(broadcasters);

                            // 8. Send success (RoomState)
                            let serializable_room = RoomSerialize::from(&new_room);
                            send_message(&mut ws, ServerMessage::RoomState(serializable_room)).await;

                            println!("User {} created and joined room {} ({})", validated_user_id, name, room_id_arc);
                        
                    }
                    ClientMessage::Ping => {
                        if user_id.is_none() {
                                send_message(
                                &mut ws,
                                ServerMessage::Error {
                                    message: "Not connected. Send a Connect message first.".to_string(),
                                },
                            )
                            .await;
                            continue;
                        }
                        send_message(&mut ws, ServerMessage::Pong).await;
                    },
                    _ => {
                        if user_id.is_none() {
                             send_message(
                                &mut ws,
                                ServerMessage::Error {
                                    message: "Not connected. Send a Connect message first.".to_string(),
                                },
                            )
                            .await;
                            continue;
                        }

                        println!("Received unhandled authenticated message type.");
                    }
                }
            },

            // 2. Listen for messages from the broadcast channel
            result = async { current_broadcaster_sub.as_mut().unwrap()
                                .recv().await }, if current_broadcaster_sub.is_some() => {
                // Got a message from the broadcaster. Send it to the client.
                // We clone the message to avoid move errors if we need it later.
                match result {
                    Ok(msg) => {
                        match msg {
                            ServerMessage::RoomState(room_state_data) => {
                                // Check if this message is for the room we are currently in
                                if current_room_id.as_deref() == Some(&room_state_data.room_id) {
                                    println!("Relaying room state for room {} to user {}", room_state_data.room_id, user_id.as_deref().unwrap_or("UNKNOWN"));
                                    send_message(&mut ws, ServerMessage::RoomState(room_state_data)).await;
                            }
                            }
                            // We could broadcast other message types here too if needed
                            _ => {
                                // For now, only broadcast RoomState.
                            }
                        }
                    },
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        println!("Broadcast receiver for user {} lagged by {} messages.", user_id.as_deref().unwrap_or("UNKNOWN"), n);
                        // Just continue, the receiver is still usable
                    },
                    Err(broadcast::error::RecvError::Closed) => {
                        println!("Broadcast channel for user {} was closed. Removing subscription.", user_id.as_deref().unwrap_or("UNKNOWN"));
                        // The broadcaster was dropped, probably room deleted.
                        // Set sub to None so we don't try to receive again.
                        current_broadcaster_sub = None;
                    }
                }
            },

            // Either ws.next() or rx.recv() returned None/Err, so we break.
            else => {
                break;
            }
        }
    }

    // Cleanup
    if let Some(user_id) = user_id {

        if let Some(room_id_str) = current_room_id {
            // This is an Arc<str>
            println!("...user was in room {}. Cleaning up.", room_id_str);
            let mut room_state = state.room_state.write().await;
            let mut broadcasters = state.room_broadcasters.write().await;
    
            let mut room_is_empty = false;
            let mut broadcast_update = false;
    
            room_state.user_map.remove(&user_id);
    
            if let Some(room) = room_state.room_map.get_mut(&room_id_str) {
                {
                    if room.members.remove(&user_id) {
                        // Only broadcast if the member was successfully removed
                        broadcast_update = true;
                    }
                }
    
                let room = room.as_ref();
    
                println!(
                    "...user {} removed from room {}. {} members left.",
                    user_id,
                    room_id_str,
                    room.members.len()
                );
    
                if room.members.is_empty() {
                    room_is_empty = true;
                }
    
                if broadcast_update {
                    if let Some(tx) = broadcasters.get(&room_id_str) {
                        let serialized_room = RoomSerialize::from(room);
                        tx.send(ServerMessage::RoomState(serialized_room)).ok();
                    }
                }
            }
    
            if room_is_empty {
                println!(
                    "...room {} is now empty. It will be cleaned up by the TTL task.",
                    room_id_str
                );
            }
        }
    } else {
        println!("Connection closed (user never connected to a room).");
    }
}

/// Helper to send a ServerMessage
async fn send_message(socket: &mut WebSocket, msg: ServerMessage) {
    let json = serde_json::to_string(&msg).unwrap_or_else(|e| {
        return json!({"type": "error", "message": e.to_string()}).to_string();
    });
    if socket.send(WsMessage::text(json)).await.is_err() {
        let error_type: String = match msg {
            ServerMessage::Error { message } => message,
            _ => "message".to_string(),
        };
        println!("Failed to send {} to client", error_type);
    }
}

/// Helper to validate auth info without sending messages
async fn validate_auth_info(
    auth: AuthInfoType,
    state: &Arc<AppState>,
) -> Result<UserIdStr, String> {
    match auth {
        AuthInfoType::Discord {
            user_id,
            guild_id,
            channel_id,
        } => {
            let (user_id_u64, guild_id_u64, channel_id_u64) = match (
                user_id.parse::<u64>().ok().map(UserId::from),
                guild_id.parse::<u64>().ok().map(GuildId::from),
                channel_id.parse::<u64>().ok().map(ChannelId::from),
            ) {
                (Some(u), Some(g), Some(c)) => (u, g, c),
                _ => {
                    return Err("Invalid user, guild or channel ID".to_string());
                }
            };

            let is_valid = {
                let user_voice_map = state.user_voice_map.read().await;
                match user_voice_map.get(&user_id_u64) {
                    Some(&(g_id, c_id)) => g_id == guild_id_u64 && c_id == channel_id_u64,
                    None => false,
                }
            };

            if is_valid {
                Ok(user_id.into()) // Return the String-based ID
            } else {
                Err("Invalid voice channel".to_string())
            }
        }
        AuthInfoType::General { user_id } => Ok(user_id.into()),
    }
}

// ===============================================================
// ================== BACKGROUND CLEANUP TASK ==================
// ===============================================================

/// A background task that periodically cleans up empty, expired rooms.
pub async fn spawn_room_cleanup_task(state: Arc<AppState>) {
    // Run this task forever
    loop {
        // Check every 60 seconds
        sleep(Duration::from_secs(60)).await;

        let mut rooms_to_delete = Vec::new();

        let room_state = state.room_state.read().await;
        let now_str = Utc::now().to_rfc3339();

        for (room_id, room) in room_state.room_map.iter() {
            // Find rooms that are empty
            if room.members.is_empty() {
                // Check if they are past their TTL
                let created_at = match chrono::DateTime::parse_from_rfc3339(&room.created_at) {
                    Ok(t) => t.with_timezone(&Utc),
                    Err(_) => {
                        println!("Error parsing timestamp for room {}. Skipping.", room_id);
                        continue;
                    }
                };

                let now = match chrono::DateTime::parse_from_rfc3339(&now_str) {
                    Ok(t) => t.with_timezone(&Utc),
                    Err(_) => continue,
                };
                
                if now.signed_duration_since(created_at).to_std().unwrap_or_default() > ROOM_TTL {
                    println!("Room {} is empty and expired. Scheduling for deletion.", room_id);
                    rooms_to_delete.push(room_id.clone());
                }
            }
        }
        drop(room_state);

        if rooms_to_delete.is_empty() {
            continue; // Nothing to do, go back to sleep
        }

        let mut room_state_write = state.room_state.write().await;
        let mut broadcasters_write = state.room_broadcasters.write().await;

        for room_id in rooms_to_delete {
            println!("...Deleting room {}", room_id);
            room_state_write.room_map.remove(&room_id);
            broadcasters_write.remove(&room_id);
            // Any users in user_map pointing to this room are already gone,
            // or they will be on their next connect attempt (room not found).
            // This is self-healing.
        }
    }
}
