//! The server as a program: it listens at `VIBETOGETHER_MCP_ADDRESS`, by default
//! 127.0.0.1:8792, serves MCP at `/mcp`, and takes a session token alone to mean
//! the relay at `AGENTWIRE_RELAY`, by default the public one.
//! It accepts requests for the hosts in `VIBETOGETHER_MCP_HOSTS`, separated by commas,
//! by default only loopback ones; behind a public domain, name it there.

use std::env;
use std::error::Error;

use tokio::net::TcpListener;
use vibetogether_mcp::{Room, service};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let address =
        env::var("VIBETOGETHER_MCP_ADDRESS").unwrap_or_else(|_| "127.0.0.1:8792".to_owned());
    let relay = env::var("AGENTWIRE_RELAY")
        .unwrap_or_else(|_| "https://agentwire.agentics.lwirth.com".to_owned());
    let hosts =
        env::var("VIBETOGETHER_MCP_HOSTS").unwrap_or_else(|_| "localhost,127.0.0.1,::1".to_owned());
    let hosts = hosts
        .split(',')
        .map(str::trim)
        .filter(|host| !host.is_empty());
    let router = axum::Router::new().nest_service("/mcp", service(Room::new(relay), hosts));
    let listener = TcpListener::bind(&address).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async { tokio::signal::ctrl_c().await.unwrap_or_default() })
        .await?;
    Ok(())
}
