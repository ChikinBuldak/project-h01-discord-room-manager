use std::sync::Arc;

use axum::{
    Json,
    extract::{
        State, WebSocketUpgrade,
        ws::{Message as WsMessage, WebSocket},
    },
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::json;
use serenity::all::{ChannelId, GuildId, UserId};
use tokio::sync::broadcast;

use crate::types::{
    AppState, ClientMessage, Room, RoomId, RoomSerialize, ServerMessage, ServerSuccessCode,
    UserIdStr, WsAuthRequest,
};

#[derive(Debug, Serialize)]
pub struct GetAllRoomsPayload {
    pub room_id: String,
    pub name: String,
    pub owner_id: String, // This is Arc<str>
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

pub async fn get_all_rooms(State(state): State<Arc<AppState>>) -> Json<Vec<GetAllRoomsPayload>> {
    let room_state = state.room_state.read().await;
    let rooms: Vec<GetAllRoomsPayload> = room_state
        .room_map
        .values()
        .map(|room| GetAllRoomsPayload::from(room))
        .collect();

    Json(rooms)
}

pub async fn ws_handler(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> Response {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

async fn handle_socket(mut ws: WebSocket, state: Arc<AppState>) {
    println!("New WebSocket connection established. Awaiting validation...");

    let user_id: UserIdStr = match validate_connection(&mut ws, &state).await {
        Some(id) => id,
        None => {
            println!("...Validation failed. Closing connection.");
            let _ = ws.close().await;
            return;
        }
    };

    println!("...User {} validated. Connection active.", user_id);

    let mut current_room_id: Option<RoomId> = None;
    let mut current_broadcaster_sub: Option<broadcast::Receiver<ServerMessage>> = None;

    {
        let room_state = state.room_state.read().await;
        if let Some(room_id) = room_state.user_map.get(&user_id) {
            if let Some(room) = room_state.room_map.get(room_id) {
                println!("User {} is reconnecting to room '{}'", &user_id, room.name);
                current_room_id = Some(room_id.clone());

                // try get existing broadcaster
                let broadcasters = state.room_broadcasters.read().await;
                if let Some(broadcaster) = broadcasters.get(room_id) {
                    current_broadcaster_sub = Some(broadcaster.subscribe());
                } else {
                    // This is a recovery case. The room exists but broadcaster doesn't.
                    // Let's create it.
                    drop(broadcasters);
                    let mut broadcasters_write = state.room_broadcasters.write().await;
                    let (tx, rx) = broadcast::channel(16);
                    broadcasters_write.insert(room_id.clone(), tx);
                    current_broadcaster_sub = Some(rx);
                    println!("...Recreated missing broadcaster for room {}", room.name);
                }
                send_message(
                    &mut ws,
                    ServerMessage::Success {
                        message: "Reconnected to room".to_string(),
                        code: ServerSuccessCode::Authenticated,
                    },
                )
                .await;

                let serializable_room = RoomSerialize::from(room);
                send_message(&mut ws, ServerMessage::RoomState(serializable_room)).await;
            } else {
                println!(
                    "User {} was in room {}, but it no longer exists. Cleaning up stale map entry.",
                    &user_id, room_id
                );
                drop(room_state);
                let mut room_state_write = state.room_state.write().await;
                room_state_write.user_map.remove(&user_id);
            }
        }
    }

    // If not reconnecting,  send a generic success
    if current_room_id.is_none() {
        send_message(
            &mut ws,
            ServerMessage::Success {
                message: "You are now connected. Create or join a room.".to_owned(),
                code: ServerSuccessCode::Authenticated,
            },
        )
        .await;
    }

    // Wait for client messages (Create, Join, Ping, etc)
    loop {
        tokio::select! {
            // 1. Listen for messages from the client (ws.next())
            Some(message) = ws.next() => {
                let message = match message {
                    Ok(WsMessage::Text(t)) => t,
                    Ok(WsMessage::Close(_)) => {
                        println!("Client closed connection.");
                        break; // Break select! loop to trigger cleanup
                    }
                    Err(e) => {
                        println!("WebSocket error: {}", e);
                        break; // Break select! loop
                    }
                    _ => continue, // Ignore non-text/close messages
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

                // Handle the client message
                match client_message {
                    ClientMessage::CreateRoom(req) => {
                        let room_id_str: RoomId = req.room_id.clone().into();
                        if req.user_id != *user_id {
                            send_message(&mut ws, ServerMessage::Error { message: "User ID Mismatch".to_string() }).await;
                            continue;
                        }

                        let mut room_state = state.room_state.write().await;

                        if room_state.user_map.contains_key(&user_id) {
                            send_message(&mut ws, ServerMessage::Error { message: "You are already in a room".to_string() }).await;
                            continue;
                        }
                        if room_state.room_map.contains_key(&room_id_str) {
                            send_message(&mut ws, ServerMessage::Error { message: "Room already exists".to_string() }).await;
                            continue;
                        }

                        let new_room = Room {
                            room_id: room_id_str.clone(),
                            name: req.name.into(),
                            owner_id: user_id.clone(),
                            created_at: req.created_at,
                            max_capacity: req.max_capacity,
                            members: [user_id.clone()].iter().cloned().collect(),
                        };

                        let serializable_room = RoomSerialize::from(&new_room);

                        room_state.room_map.insert(room_id_str.clone(), new_room);
                        room_state.user_map.insert(user_id.clone(), room_id_str.clone());

                        let (tx, rx) = broadcast::channel(16);
                        let mut broadcasters = state.room_broadcasters.write().await;
                        broadcasters.insert(room_id_str.clone(), tx.clone());

                        current_room_id = Some(room_id_str);
                        current_broadcaster_sub = Some(rx);

                        println!("User {} created room {} with ID {}", user_id, serializable_room.name, serializable_room.room_id);

                        send_message(&mut ws, ServerMessage::Success { message: "Room created".to_string(), code: ServerSuccessCode::RoomCreated }).await;

                        let state_message = ServerMessage::RoomState(serializable_room.clone());
                        send_message(&mut ws, state_message.clone()).await;
                        // Broadcast the new room state
                        tx.send(state_message).ok();
                    }
                    ClientMessage::JoinRoom { room_id } => {
                        let mut room_state = state.room_state.write().await;
                        let room_id_str: RoomId = room_id.clone().into();

                        if room_state.user_map.contains_key(&user_id) {
                            send_message(&mut ws, ServerMessage::Error { message: "You are already in a room".to_string() }).await;
                            continue;
                        };

                        let room_name: Arc<str>;
                        let mut broadcaster: Option<broadcast::Sender<ServerMessage>> = None;
                        let mut room_state_message: Option<ServerMessage> = None;

                        {
                            let Some(room) = room_state.room_map.get_mut(&room_id_str) else {
                                send_message(&mut ws, ServerMessage::Error { message: "Room not found".to_string() }).await;
                                continue;
                            };

                            room_name = Arc::clone(&room.name);

                            if room.members.len() >= room.max_capacity {
                                send_message(&mut ws, ServerMessage::Error { message: "Room is full".to_string() }).await;
                                continue;
                            }

                            room.members.insert(user_id.clone());

                            room_state_message = Some(ServerMessage::RoomState(RoomSerialize::from(room.as_ref())));

                            // --- Broadcast Logic ---
                            let broadcasters = state.room_broadcasters.read().await;
                            if let Some(tx) = broadcasters.get(&room_id_str) {
                                current_broadcaster_sub = Some(tx.subscribe());
                                broadcaster = Some(tx.clone());
                            } else {
                                // Room exists but no broadcaster, recover
                                drop(broadcasters);
                                let mut broadcasters_write = state.room_broadcasters.write().await;
                                let (tx, rx) = broadcast::channel(16);
                                broadcasters_write.insert(room_id_str.clone(), tx.clone());
                                current_broadcaster_sub = Some(rx);
                                broadcaster = Some(tx);
                                println!("...Recreated missing broadcaster for room {}", room_name);
                            }
                            // --- End Broadcast Logic ---
                        }

                        room_state.user_map.insert(user_id.clone(), room_id_str.clone());
                        current_room_id = Some(room_id_str.clone());

                        println!("User {} joined room '{}'", user_id, room_name);
                        send_message(&mut ws, ServerMessage::Success { message: "Room joined.".to_string(), code: ServerSuccessCode::UserJoined }).await;

                        // Broadcast the updated room state
                        if let (Some(state_msg), Some(tx)) = (room_state_message, broadcaster) {
                             // 1. Send the state DIRECTLY to the joining client.
                            send_message(&mut ws, state_msg.clone()).await;
                            println!("Send state to joining client");

                            // 2. Broadcast the state to all OTHER clients.
                            tx.send(state_msg).ok();
                            println!("Broadcast state to other clients");
                        }
                    }
                    ClientMessage::LeaveRoom { room_id } => {
                        println!("User {} is gracefully leaving room {}", user_id, room_id);
                        // Set the room_id to be cleaned up
                        current_room_id = Some(room_id.into());
                        // Break the loop to trigger the cleanup logic
                        break;
                    }
                    ClientMessage::Ping => {
                        send_message(&mut ws, ServerMessage::Pong).await;
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
                                     println!("Relaying room state for room {} to user {}", room_state_data.room_id, user_id);
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
                        println!("Broadcast receiver for user {} lagged by {} messages.", user_id, n);
                        // Just continue, the receiver is still usable
                    },
                    Err(broadcast::error::RecvError::Closed) => {
                        println!("Broadcast channel for user {} was closed. Removing subscription.", user_id);
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
    println!("Connection for user {} closing...", user_id);
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
            room_state.room_map.remove(&room_id_str);
            broadcasters.remove(&room_id_str);
            println!(
                "...room {} was empty and has been deleted (along with its broadcaster).",
                room_id_str
            );
        }
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

async fn validate_connection(socket: &mut WebSocket, state: &Arc<AppState>) -> Option<UserIdStr> {
    let auth_req_text = match socket.next().await {
        Some(Ok(WsMessage::Text(text))) => text,
        _ => {
            println!("Client disconnected before sending auth message.");
            return None;
        }
    };

    let auth_req: WsAuthRequest = match serde_json::from_str(&auth_req_text) {
        Ok(req) => req,
        Err(e) => {
            send_message(
                socket,
                ServerMessage::Error {
                    message: format!("Invalid auth message format: {}", e),
                },
            )
            .await;
            return None;
        }
    };

    match auth_req {
        WsAuthRequest::Discord {
            user_id,
            guild_id,
            channel_id,
        } => {
            let (user_id, guild_id, channel_id) = match (
                user_id.parse::<u64>().ok().map(UserId::from),
                guild_id.parse::<u64>().ok().map(GuildId::from),
                channel_id.parse::<u64>().ok().map(ChannelId::from),
            ) {
                (Some(u), Some(g), Some(c)) => (u, g, c),
                _ => {
                    send_message(
                        socket,
                        ServerMessage::Error {
                            message: "Invalid user, guild or channel ID".to_string(),
                        },
                    )
                    .await;
                    return None;
                }
            };

            let is_valid = {
                let user_voice_map = state.user_voice_map.read().await;
                match user_voice_map.get(&user_id) {
                    Some(&(g_id, c_id)) => g_id == guild_id && c_id == channel_id,
                    None => false,
                }
            };

            if is_valid {
                Some(user_id.to_string().into())
            } else {
                send_message(
                    socket,
                    ServerMessage::Error {
                        message: "Invalid voice channel".to_string(),
                    },
                )
                .await;
                None
            }
        }
        WsAuthRequest::General { user_id } => Some(user_id.into()),
    }
}
