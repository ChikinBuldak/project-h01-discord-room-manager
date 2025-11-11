use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use reqwest::Client as ReqwestClient;
use serde::{Deserialize, Serialize};
use serenity::all::{ApplicationId, ChannelId, GuildId, UserId};
use tokio::sync::RwLock;

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

#[derive(Clone, Debug)] // REMOVED Serialize
pub struct Room {
    pub room_id: RoomId,
    pub name: Arc<str>,
    pub owner_id: UserIdStr, // This is Arc<str>
    pub created_at: String,
    pub max_capacity: usize,
    /// For members, it can use the format from user id discord, or general user id
    pub members: HashSet<UserIdStr>, // This is HashSet<Arc<str>>
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

#[derive(Default, Debug)]
pub struct AppRoomState {
    /// Holds all active rooms, keyed by room_id
    pub room_map: HashMap<RoomId, Room>,
    /// Holds a reverse mapping to find a user's room quickly (UserIdStr -> RoomId)
    pub user_map: HashMap<UserIdStr, RoomId>,
}

pub type SharedRoomState = Arc<RwLock<AppRoomState>>;

type GeneralUserId = String;
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
}

pub struct Data {
    pub application_id: ApplicationId,
    pub voice_map: Arc<RwLock<VoiceChannelMap>>,
    pub user_voice_map: Arc<RwLock<UserVoiceMap>>,
    pub room_state: SharedRoomState,
}

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type PoiseContext<'a> = poise::Context<'a, Data, Error>;

// ======= Room Manager Ws Payloads ========

/// This struct is used to parse the first message from the client
/// over the WebSocket connection.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum WsAuthRequest {
    Discord {
        user_id: String,
        guild_id: String,
        channel_id: String,
    },
    General {
        user_id: String,
    },
}

#[derive(Deserialize, Debug)]
pub struct WsCreateRoomRequest {
    pub user_id: GeneralUserId,
    pub room_id: String,
    pub name: String,
    pub created_at: String,
    pub max_capacity: usize,
}
/// Message the client can send to the server *after* authentication
#[derive(Deserialize, Debug)]
#[serde(tag = "type")]
pub enum ClientMessage {
    #[serde(rename = "create_room")]
    CreateRoom(WsCreateRoomRequest),
    #[serde(rename = "join_room")]
    JoinRoom { room_id: String },
    #[serde(rename = "leave_room")]
    LeaveRoom { room_id: String },
    #[serde(rename = "ping")]
    Ping,
}

#[derive(Serialize, Debug)]
#[serde(tag = "type")]
pub enum ServerMessage {
    #[serde(rename = "success")]
    Success { message: String },
    #[serde(rename = "error")]
    Error { message: String },
    #[serde(rename = "room_state")]
    RoomState(RoomSerialize),
    #[serde(rename = "pong")]
    Pong,
}
