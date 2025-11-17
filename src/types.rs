use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::{Json, response::IntoResponse};
use reqwest::{Client as ReqwestClient, StatusCode};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::json;
use serenity::all::{ApplicationId, ChannelId, GuildId, UserId};
use tokio::sync::{RwLock, broadcast};

// VOICE CHANNEL MAPPING AND EVENT HANDLER (CORE)

/// Map the Voice channel group by guild, then channel, to users
#[derive(Default)]
pub struct VoiceChannelMap {
    pub map: HashMap<GuildId, HashMap<ChannelId, HashSet<UserId>>>,
}

pub type UserVoiceMap = HashMap<UserId, (GuildId, ChannelId)>;

// Authentication Related for Discord Client OAuth
#[derive(Deserialize)]
pub struct TokenRequest {
    pub code: String,
}

#[derive(Serialize)]
pub struct TokenResponse {
    pub access_token: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct DiscordTokenResponse {
    pub access_token: String,
    pub token_type: Option<String>,
    pub expires_in: Option<u64>,
    pub refresh_token: Option<String>,
    pub scope: Option<String>,
}

pub type UserIdStr = Arc<str>;

#[derive(Clone, Debug)]
pub struct Room {
    pub room_id: RoomId,
    pub name: Arc<str>,
    pub owner_id: UserIdStr,
    pub created_at: String,
    pub max_capacity: usize,
    /// For members, it can use the format from user id discord, or general user id
    pub members: HashSet<UserIdStr>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RoomSerialize {
    pub room_id: String,
    pub name: String,
    pub owner_id: String,
    pub created_at: String,
    pub max_capacity: usize,
    /// For members, it can use the format from user id discord, or general user id
    pub members: HashSet<String>,
}

impl From<&Room> for RoomSerialize {
    fn from(internal: &Room) -> Self {
        RoomSerialize {
            room_id: internal.room_id.to_string(),
            name: internal.name.to_string(),
            owner_id: internal.owner_id.to_string(),
            created_at: internal.created_at.clone(),
            max_capacity: internal.max_capacity,
            // Clones each Arc<str> into an owned String
            members: internal.members.iter().map(|s| s.to_string()).collect(),
        }
    }
}

impl AsRef<Room> for Room {
    fn as_ref(&self) -> &Room {
        self
    }
}

#[derive(Default, Debug)]
pub struct AppRoomState {
    /// Holds all active rooms, keyed by room_id
    pub room_map: HashMap<RoomId, Room>,
    /// Holds a reverse mapping to find a user's room quickly (UserIdStr -> RoomId)
    pub user_map: HashMap<UserIdStr, RoomId>,
}

pub type SharedRoomState = Arc<RwLock<AppRoomState>>;

pub type RoomId = Arc<str>;

/**
 * The state for our Axum web server.
 * We'll add the Reqwest client here for making outside requests.
 */
#[derive(Clone)]
pub struct AppState {
    pub http_client: ReqwestClient,
    pub user_voice_map: Arc<RwLock<UserVoiceMap>>,
    pub room_state: SharedRoomState,
    pub room_broadcasters: RoomBroadcaster,
}

pub type RoomBroadcaster = Arc<RwLock<HashMap<RoomId, broadcast::Sender<ServerMessage>>>>;
pub struct Data {
    pub application_id: ApplicationId,
    pub voice_map: Arc<RwLock<VoiceChannelMap>>,
    pub user_voice_map: Arc<RwLock<UserVoiceMap>>,
    pub room_state: SharedRoomState,
    pub room_broadcasters: RoomBroadcaster,
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type PoiseContext<'a> = poise::Context<'a, Data, Error>;

// ======= Room Manager Ws Payloads ========

/// This struct is used to parse the first message from the client
/// over the WebSocket connection.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum AuthInfoType {
    Discord {
        user_id: String,
        guild_id: String,
        channel_id: String,
    },
    General {
        user_id: String,
    },
}

/// Message the client can send to the server *after* authentication
#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ClientMessage {
    #[serde(rename = "connect")]
    Connect { auth: AuthInfoType, room_id: String },
    #[serde(rename = "create_room")]
    CreateRoom {auth: AuthInfoType, max_capacity: usize, name: String},
    #[serde(rename = "leave_room")]
    LeaveRoom { room_id: String },
    #[serde(rename = "ping")]
    Ping,
}

#[derive(Serialize, Debug, Clone)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(rename = "room_state")]
    RoomState(RoomSerialize),
    #[serde(rename = "pong")]
    Pong,
}

// Global API Type

#[derive(Debug, Clone)]
pub struct ApiError {
    pub status_code: StatusCode,
    pub message: String,
}

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        let body = Json(json!({
            "error": true,
            "code": self.status_code.as_u16(),
            "message": self.message
        }));

        (self.status_code, body).into_response()
    }
}
