mod handlers;
mod types;
mod utils;

use std::{
    collections::{HashMap, HashSet},
    env,
    net::SocketAddr,
    str::FromStr,
    sync::Arc,
};

use axum::{
    Router,
    http::HeaderValue,
    routing::{get, post},
};
use dotenv::dotenv;
use reqwest::{Client as HttpClient, Method, header};
use serenity::{
    Client,
    all::{ApplicationId, GatewayIntents, Ready, Settings},
};
use tokio::sync::RwLock;
use tower_http::cors::{Any, CorsLayer};

use crate::{
    handlers::{
        activity::{create_room, get_all_rooms, join_room, spawn_room_cleanup_task, ws_handler},
        auth::exchange_code_for_token,
        event::{ping, play, voice_state_update},
    },
    types::{AppRoomState, AppState, Data, Error, Room, UserVoiceMap, VoiceChannelMap},
};
use poise::serenity_prelude::FullEvent::VoiceStateUpdate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppEnv {
    Development,
    Production,
}

impl AppEnv {
    fn from_env() -> Self {
        match std::env::var("APP_ENV")
            .unwrap_or_else(|_| "development".into())
            .as_str()
        {
            "development" => AppEnv::Development,
            "production" => AppEnv::Production,
            _ => panic!("Invalid APP_ENV value"),
        }
    }
}

fn build_cors(env: AppEnv) -> CorsLayer {
    match (env) {
        AppEnv::Development => CorsLayer::new()
            .allow_origin(Any)
            .allow_methods(Any)
            .allow_headers(Any),
        AppEnv::Production => {
            // This is a placeholder. TODO: Implement production CORS configuration
            CorsLayer::new()
                .allow_origin("https://example.com".parse::<HeaderValue>().unwrap())
                .allow_methods([Method::GET, Method::POST])
                .allow_headers([header::CONTENT_TYPE, header::AUTHORIZATION])
        }
    }
}

async fn on_error(error: poise::FrameworkError<'_, Data, Error>) {
    // This is our custom error handler
    // They are many errors that can occur, so we only handle the ones we want to customize
    // and forward the rest to the default handler
    match error {
        poise::FrameworkError::Setup { error, .. } => panic!("Failed to start bot: {:?}", error),
        poise::FrameworkError::Command { error, ctx, .. } => {
            println!("Error in command `{}`: {:?}", ctx.command().name, error,);
        }
        error => {
            if let Err(e) = poise::builtins::on_error(error).await {
                println!("Error while handling error: {}", e)
            }
        }
    }
}

#[tokio::main]
async fn main() {
    if let Ok(_path) = dotenv() {
        println!("Loaded .env file");
    } else {
        println!("No .env file found, using system environment variables");
    }
    let token = env::var("DISCORD_BOT_TOKEN").expect("Expected a token in the environment");
    let base_url = env::var("BASE_URL").expect("Expected base URL in environment");
    let application_id = env::var("VITE_DISCORD_CLIENT_ID")
        .expect("Expected application ID in environment")
        .parse::<u64>()
        .expect("Application ID must be a valid u64");

    let intents = GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_MEMBERS
        | GatewayIntents::GUILD_VOICE_STATES
        | GatewayIntents::GUILD_PRESENCES;

    // Initialize shared data
    let shared_data = Data {
        application_id: ApplicationId::from(application_id),
        voice_map: Arc::new(RwLock::new(VoiceChannelMap::default())),
        user_voice_map: Arc::new(RwLock::new(UserVoiceMap::default())),
        room_state: Arc::new(RwLock::new(AppRoomState::default())),
        room_broadcasters: Arc::new(RwLock::new(HashMap::new())),
    };

    // Create dummy room
    {
        let dummy_room = Room {
            room_id: "general_lobby_1".into(),
            name: "Public Lobby".into(),
            owner_id: "system".into(),
            created_at: "2025-01-01T00:00:00Z".to_string(),
            max_capacity: 20,
            members: HashSet::new(),
        };

        // Get a write lock and insert the room
        // We can .await here because we are in an async fn
        let mut room_state_write = shared_data.room_state.write().await;
        room_state_write
            .room_map
            .insert(dummy_room.room_id.clone(), dummy_room);
        println!("Created dummy 'Public Lobby' room.");
    }

    // Clone data for the monitoring task
    let voice_map_monitor = shared_data.voice_map.clone();
    let user_voice_map_monitor = shared_data.user_voice_map.clone();
    let room_state_map_monitor = shared_data.room_state.clone();
    let room_broadcasters_monitor = shared_data.room_broadcasters.clone();
    // Poise Framework setup
    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                play(),
                ping(),
                // add other commands here
            ],
            on_error: |error| Box::pin(on_error(error)),
            event_handler: |ctx, event, _framework, data| {
                Box::pin(async move {
                    match event {
                        VoiceStateUpdate { old, new } => {
                            voice_state_update(ctx, old, new, data).await?;
                        },
                        serenity::all::FullEvent::Ready { data_about_bot } => {
                            println!("Connected as {}", data_about_bot.user.name);
                            scan_existing_voice_states(ctx, data).await;
                        }
                        _ => {}
                    }
                    Ok(())
                })
            },
            ..Default::default()
        })
        .setup(|ctx, _ready, framework| {
            Box::pin(async move {
                // Get the first available guild from the cache
                for guild_id in ctx.cache.guilds() {
                    println!("Registering commands to guild {}...", guild_id);
                    poise::builtins::register_in_guild(ctx, &framework.options().commands, guild_id).await?;
                    println!("Commands registered to guild {} successfully!", guild_id);
                }

                if ctx.cache.guilds().is_empty() {
                    println!("Bot is not in any guilds. Commands will not be registered until the bot joins a guild.");
                }

                Ok(shared_data)
            })
        })
        .build();

    let mut client = Client::builder(token, intents)
        .framework(framework)
        .cache_settings({
            let mut settings = Settings::default();
            // 2. Modify the specific fields you care about
            settings.cache_guilds = true;
            settings.cache_channels = true;
            settings.cache_users = true;

            settings
        })
        .await
        .expect("Error creating client");

    // Axum Setup
    let app_state = Arc::new(AppState {
        http_client: HttpClient::new(),
        user_voice_map: user_voice_map_monitor.clone(),
        room_state: room_state_map_monitor.clone(),
        room_broadcasters: room_broadcasters_monitor.clone(),
    });

    let addr = SocketAddr::from_str(base_url.as_str()).expect("Invalid base URL");

    let app_env = AppEnv::from_env();
    let cors_layer = build_cors(app_env);

    let app = Router::new()
        .route("/.proxy/api/token", post(exchange_code_for_token))
        .route("/.proxy/api/activity/ws", get(ws_handler))
        .route("/.proxy/api/activity/rooms", get(get_all_rooms))
        .route("/.proxy/api/activity/rooms", post(create_room))
        .route("/.proxy/api/activity/rooms/join", post(join_room))
        .with_state(app_state.clone())
        .layer(cors_layer);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect(&format!("Cannot bind to {}", addr.to_string()));

    // Start Discord client
    let discord_handle = tokio::spawn(async move {
        println!("Starting Discord client...");
        if let Err(why) = client.start().await {
            println!("client error: {:?}", why);
        }
    });

    // Voice state monitoring task
    let voice_monitor = tokio::spawn(async move {
        println!("Starting voice state monitor...");
        loop {
            {
                let map_read = voice_map_monitor.read().await;
                let user_map_read = user_voice_map_monitor.read().await;

                if !map_read.map.is_empty() || !user_map_read.is_empty() {
                    println!("Current VC Members:");

                    for (guild_id, channels) in &map_read.map {
                        for (channel_id, members) in channels {
                            println!(
                                "Guild {} | Channel {}: {:?}",
                                guild_id.get(),
                                channel_id.get(),
                                members.iter().map(|f| f.get()).collect::<Vec<_>>()
                            );
                        }
                    }

                    println!(
                        "User Voice Map: {:?}",
                        user_map_read.keys().map(|k| k.get()).collect::<Vec<_>>()
                    );
                } else {
                    // println!("No active voice channels");
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        }
    });

    let http_server = axum::serve(listener, app);

    // room cleaner task
    let room_cleanup_task = tokio::spawn(spawn_room_cleanup_task(app_state.clone()));

    tokio::select! {
        result = discord_handle => {
            println!("Discord client finished: {:?}", result);
        },
        result = http_server => {
            println!("HTTP server finished: {:?}", result);
        },
        _ = voice_monitor => {
            println!("Voice monitor finished");
        },
        result = room_cleanup_task => {
            println!("Room cleaner task finished: {:?}", result);
        }
    }
}

async fn scan_existing_voice_states(ctx: &serenity::all::Context, data: &Data) {
    let mut voice_map = data.voice_map.write().await;
    let mut user_voice_map = data.user_voice_map.write().await;
    // let mut room_state = data.room_state.write().await; // Use if needed

    println!("Scanning existing voice states...");

    for guild_id in ctx.cache.guilds() {
        if let Some(guild) = ctx.cache.guild(guild_id) {
            for (user_id, voice_state) in &guild.voice_states {
                if let Some(channel_id) = voice_state.channel_id {
                    println!("User ID: {}", user_id.get());
                    println!("Channel ID: {}", channel_id.get());
                    voice_map
                        .map
                        .entry(guild_id)
                        .or_default()
                        .entry(channel_id)
                        .or_default()
                        .insert(*user_id);
                    user_voice_map.insert(*user_id, (guild_id, channel_id));

                    println!(
                        "Detected user {} already in VC {} before startup",
                        user_id.get(),
                        channel_id.get()
                    );
                }
            }
        }
    }
}
