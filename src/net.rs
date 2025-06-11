// src/net.rs
use reqwest::Client;
use std::net::SocketAddr;

/// HTTP-клиент с жёсткой таблицей host → IP:порт (443).
/// Позволяет обойти отсутствие системного DNS без добавления новых фич.
pub fn http_client() -> Client {
    Client::builder()
        // Jupiter price API
        .resolve(
            "price.jup.ag",
            "104.18.19.96:443"
                .parse::<SocketAddr>()
                .expect("Invalid SocketAddr for price.jup.ag"),
        )
        // Jupiter quote API
        .resolve(
            "quote-api.jup.ag",
            "104.18.18.96:443"
                .parse::<SocketAddr>()
                .expect("Invalid SocketAddr for quote-api.jup.ag"),
        )
        // Telegram Bot API
        .resolve(
            "api.telegram.org",
            "149.154.167.220:443"
                .parse::<SocketAddr>()
                .expect("Invalid SocketAddr for api.telegram.org"),
        )
        // (Если пользуетесь Raydium)
        .resolve(
            "api-v3.raydium.io",
            "104.18.18.96:443"
                .parse::<SocketAddr>()
                .expect("Invalid SocketAddr for api-v3.raydium.io"),
        )
        .build()
        .expect("Failed to build HTTP client with fixed DNS")
}
