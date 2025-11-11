use std::{env, sync::Arc};

use axum::{Json, extract::State};

use crate::types::{AppState, DiscordTokenResponse, TokenRequest, TokenResponse};
use reqwest::{ StatusCode};
/**
* Axum handler for POST /api/token
 * This re-implements your Node.js logic in Rust.
 */
pub async fn exchange_code_for_token(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<TokenRequest>,
) -> Result<Json<TokenResponse>, StatusCode> {
    println!(".proxy/api/token/ endpoint is consumed");
    // Get secrets from environment
    let client_id = env::var("VITE_DISCORD_CLIENT_ID")
        .expect("VITE_DISCORD_CLIENT_ID not set");
    let client_secret = env::var("DISCORD_CLIENT_SECRET")
        .expect("DISCORD_CLIENT_SECRET not set");

    // Build the form parameters
    let params = [
        ("client_id", &client_id),
        ("client_secret", &client_secret),
        ("grant_type", &"authorization_code".to_string()),
        ("code", &payload.code),
    ];

    // Exchange the code for an access_token
    let response = state.http_client
        .post("https://discord.com/api/oauth2/token")
        .form(&params)
        .send()
        .await;

    match response {
        Ok(resp) => {
            match resp.json::<DiscordTokenResponse>().await {
                Ok(token_data) => {
                    // Return the access_token to our client
                    Ok(Json(TokenResponse {
                        access_token: token_data.access_token,
                    }))
                },
                Err(e) => {
                    eprintln!("Failed to parse Discord response: {}", e);
                    Err(StatusCode::INTERNAL_SERVER_ERROR)
                }
            }
        },
        Err(e) => {
            eprintln!("Failed to request token from Discord: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}