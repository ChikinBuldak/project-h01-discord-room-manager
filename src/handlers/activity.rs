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

use crate::types::{
    AppState, ClientMessage, Room, RoomId, RoomSerialize, ServerMessage, UserIdStr, WsAuthRequest,
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

    let mut current_room_id: Option<String> = None;

    {
        let room_state = state.room_state.read().await;
        if let Some(room_id) = room_state.user_map.get(&user_id) {
            if let Some(room) = room_state.room_map.get(room_id) {
                println!("User {} is reconnecting to room '{}'", &user_id, room.name);
                current_room_id = Some(room_id.to_string());
                send_message(
                    &mut ws,
                    ServerMessage::Success {
                        message: "Reconnected to room".to_string(),
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
            },
        )
        .await;
    }

    // Wait for client messages (Create, Join, Ping, etc)

    while let Some(message) = ws.next().await {
        let message = match message {
            Ok(WsMessage::Text(t)) => t,
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

        match client_message {
            ClientMessage::CreateRoom(req) => {
                let room_id_str: Arc<str> = req.room_id.clone().into();
                if req.user_id != *user_id {
                    send_message(
                        &mut ws,
                        ServerMessage::Error {
                            message: "User ID Mismatch".to_string(),
                        },
                    )
                    .await;
                    continue;
                }

                // Get write locks
                let mut room_state = state.room_state.write().await;

                if room_state.user_map.contains_key(&user_id) {
                    send_message(
                        &mut ws,
                        ServerMessage::Error {
                            message: "You are already in a room".to_string(),
                        },
                    )
                    .await;
                    continue;
                }
                if room_state.room_map.contains_key(&room_id_str) {
                    send_message(
                        &mut ws,
                        ServerMessage::Error {
                            message: "Room already exists".to_string(),
                        },
                    )
                    .await;
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
                room_state
                    .user_map
                    .insert(user_id.clone(), room_id_str.clone());
                current_room_id = Some(req.room_id);

                println!(
                    "User {} created room {} with ID {}",
                    user_id, serializable_room.name, serializable_room.room_id
                );
                send_message(
                    &mut ws,
                    ServerMessage::Success {
                        message: "Room created".to_string(),
                    },
                )
                .await;
                send_message(&mut ws, ServerMessage::RoomState(serializable_room)).await;
            }
            ClientMessage::JoinRoom { room_id } => {
                let mut room_state = state.room_state.write().await;
                let room_id_str: Arc<str> = room_id.clone().into();

                if room_state.user_map.contains_key(&user_id) {
                    send_message(
                        &mut ws,
                        ServerMessage::Error {
                            message: "You are already in a room".to_string(),
                        },
                    )
                    .await;
                    continue;
                };

                let room_name: Arc<str>;
                {
                    // ---- Scoped mutable borrow of room ----
                    let Some(room) = room_state.room_map.get_mut(&room_id_str) else {
                        send_message(
                            &mut ws,
                            ServerMessage::Error {
                                message: "Room not found".to_string(),
                            },
                        )
                        .await;
                        continue;
                    };

                    room_name = Arc::clone(&room.name);

                    if room.members.len() >= room.max_capacity {
                        send_message(
                            &mut ws,
                            ServerMessage::Error {
                                message: "Room is full".to_string(),
                            },
                        )
                        .await;
                        continue;
                    }

                    room.members.insert(user_id.clone());
                }

                let room_id: Arc<str> = room_id.into();
                room_state
                    .user_map
                    .insert(user_id.clone(), Arc::clone(&room_id));
                current_room_id = Some(room_id.to_string());

                println!("User {} joined room '{}'", user_id, room_name);
                send_message(
                    &mut ws,
                    ServerMessage::Success {
                        message: "Room joined.".to_string(),
                    },
                )
                .await;

                if let Some(room) = room_state.room_map.get(&room_id_str) {
                    let serialized_room = RoomSerialize::from(room);
                    send_message(&mut ws, ServerMessage::RoomState(serialized_room)).await;
                }
            }
            ClientMessage::LeaveRoom { room_id } => {
                // This is a "graceful" leave.
                println!("User {} is gracefully leaving room {}", user_id, room_id);
                // Set the room_id to be cleaned up
                current_room_id = Some(room_id);
                // Break the loop to trigger the cleanup logic
                break;
            }
            ClientMessage::Ping => {
                send_message(&mut ws, ServerMessage::Pong).await;
            }
        }
    }

    // Cleanup
    println!("Connection for user {} closing...", user_id);
    if let Some(room_id) = current_room_id {
        println!("...user was in room {}. Cleaning up.", room_id);
        let mut room_state = state.room_state.write().await;

        room_state.user_map.remove(&user_id);

        let room_id_str: Arc<str> = room_id.clone().into();
        if let Some(room) = room_state.room_map.get_mut(&room_id_str) {
            room.members.remove(&user_id);
            println!(
                "...user {} removed from room {}. {} members left.",
                user_id,
                room_id_str,
                room.members.len()
            );
            if room.members.is_empty() {
                room_state.room_map.remove(&room_id_str);
                println!("...room {} was empty and has been deleted.", room_id);
            }
        }
    }
}

/// Helper to send a ServerMessage
async fn send_message(socket: &mut WebSocket, msg: ServerMessage) {
    let json = serde_json::to_string(&msg).unwrap_or_else(|e| {
        return json!({"type": "error", "message": e.to_string()}).to_string();
    });
    if socket.send(WsMessage::text(json)).await.is_err() {
        println!("Failed to send message to client");
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
