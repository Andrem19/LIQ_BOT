// src/params.rs

use std::time::Duration;

/// RPC URL для Mainnet-Beta
pub const RPC_URL: &str = "https://api.mainnet-beta.solana.com";

/// Файл keypair в ~/.config/solana/
pub const KEYPAIR_FILENAME: &str = "/home/jupiter/.config/solana/mainnet-id.json";

/// Интервал проверки цены (секунды)
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// Интервал отправки отчётов в Telegram (секунды)
pub const REPORT_INTERVAL: Duration = Duration::from_secs(30 * 60); // 30 минут

/// Полуширина диапазона (0.25% → ширина 0.5%)
pub const RANGE_HALF: f64 = 0.0025;

/// Конфигурация одного пула Concentrated Liquidity
#[derive(Clone)]
pub struct PoolConfig {
    pub program: &'static str,
    /// Удобное имя для лога и отчётов
    pub name: &'static str,
    /// Адрес программы CLMM-пула
    pub pool_address: &'static str,
    pub position_address: &'static str,
    /// Mint-адрес USDC
    pub mint_A: &'static str,
    /// Mint-адрес WSOL
    pub mint_B: &'static str,
    /// Начальная сумма в USDC для первой заливки
    pub decimal_A: u16,
    pub decimal_B: u16,
    pub initial_amount_usdc: f64,
}

/// Список пулов, с которыми бот будет работать
pub const POOLS: &[PoolConfig] = &[
    PoolConfig {
        program: "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
        name: "WSOL/USDC",
        pool_address: "4HppGTweoGQ8ZZ6UcCgwJKfi5mJD9Dqwy6htCpnbfBLW",
        position_address: "6yzQy77GCF14iELYU67ihiDgRvARe4yV5NAkb9qpq3pt",
        mint_A: "So11111111111111111111111111111111111111112",
        mint_B: "EPjFWdd5AufqSS3ZQSijzCetra7T1Y9NPpF7Rmgv8dZ",
        decimal_A: 9,
        decimal_B: 6,
        initial_amount_usdc: 180.0,
    },
    // Добавляйте сюда новые пулы по той же схеме
];
