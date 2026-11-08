//! The server as a program: it listens at `VIBE_TOGETHER_MCP_ADDRESS`, by default
//! 127.0.0.1:8792, serves MCP at `/mcp`, and takes a session token alone to mean
//! the relay at `AGENTWIRE_RELAY`, by default the public one.

use std::env;
use std::error::Error;

use tokio::net::TcpListener;
use vibe_together_mcp::{Room, service};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let address =
        env::var("VIBE_TOGETHER_MCP_ADDRESS").unwrap_or_else(|_| "127.0.0.1:8792".to_owned());
    let relay = env::var("AGENTWIRE_RELAY")
        .unwrap_or_else(|_| "https://agentwire.agentics.lwirth.com".to_owned());
    let router = axum::Router::new().nest_service("/mcp", service(Room::new(relay)));
    let listener = TcpListener::bind(&address).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async { tokio::signal::ctrl_c().await.unwrap_or_default() })
        .await?;
    Ok(())
}
