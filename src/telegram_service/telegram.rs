// src/telegram_service/telegram.rs

use anyhow::{Result, bail};
use reqwest::Client;
use serde_json::json;
use tracing::debug;
use crate::wirlpool_services::net::http_client;

#[derive(Clone)]
pub struct Telegram {
    client: Client,
    token: String,
    chat_id: i64,
}

impl Telegram {
    pub fn new(token: &str, chat_id: &str) -> Self {
        let client = http_client();
        let token = token.to_string();
        let chat_id = chat_id.parse().expect("CHAT_ID должен быть integer");
        Telegram { client, token, chat_id }
    }

    /// Позволяет Engine-у выполнять getUpdates и т.п.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Токен нужен для построения URL при getUpdates
    pub fn token(&self) -> &str {
        &self.token
    }

    /// Идентификатор чата
    pub fn chat_id(&self) -> i64 {
        self.chat_id
    }

    /// Отправка простого текста
    pub async fn send(&self, text: &str) -> Result<()> {
        let url = format!("https://api.telegram.org/bot{}/sendMessage", self.token);
        let resp = self.client
            .post(&url)
            .json(&json!({ "chat_id": self.chat_id, "text": text }))
            .send()
            .await?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        debug!(target="notify", %url, %status, %body);
        if !status.is_success() {
            bail!("Telegram API error: {} - {}", status, body);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn send_pool_report(
        &self,
        pool_name: &str,
        current_price: f64,
        lower_price: f64,
        upper_price: f64,
        balance_usdc: u128,
        balance_wsol: u128,
        profit_usdc: f64,
    ) -> Result<()> {
        let text = format!(
            "*Pool:* {}\n\
             *Price:* {:.6}\n\
             *Range:* [{:.6} – {:.6}]\n\
             *USDC:* {:.6}\n\
             *WSOL:* {:.6}\n\
             *Profit (USDC):* {:.6}",
            pool_name,
            current_price,
            lower_price,
            upper_price,
            balance_usdc as f64 / 1e6,
            balance_wsol as f64 / 1e9,
            profit_usdc,
        );
        self.send(&text).await
    }
}
