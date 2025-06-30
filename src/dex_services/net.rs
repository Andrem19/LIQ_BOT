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
        .build()
        .expect("Failed to build HTTP client with fixed DNS")
}

pub fn smart_http_client() -> Client {
    // пытаемся резолвить по имени
    if std::net::ToSocketAddrs::to_socket_addrs(&("price.jup.ag", 443)).is_ok() {
        Client::new()
    } else {
        http_client()  // жёсткий вариант
    }
}