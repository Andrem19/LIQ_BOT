// src/params.rs

use std::time::Duration;

/// RPC URL для Mainnet-Beta
pub const RPC_URL: &str = "https://api.mainnet-beta.solana.com";

/// Файл keypair в ~/.config/solana/
pub const KEYPAIR_FILENAME: &str = "mainnet-id.json";

/// Интервал проверки цены (секунды)
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// Интервал отправки отчётов в Telegram (секунды)
pub const REPORT_INTERVAL: Duration = Duration::from_secs(30 * 60); // 30 минут

/// Полуширина диапазона (0.25% → ширина 0.5%)
pub const RANGE_HALF: f64 = 0.0025;

/// Конфигурация одного пула Concentrated Liquidity
#[derive(Clone)]
pub struct PoolConfig {
    /// Удобное имя для лога и отчётов
    pub name: &'static str,
    /// Адрес программы CLMM-пула
    pub pool_address: &'static str,
    /// Mint-адрес USDC
    pub usdc_mint_address: &'static str,
    /// Mint-адрес WSOL
    pub wsol_mint_address: &'static str,
    /// Начальная сумма в USDC для первой заливки
    pub initial_amount_usdc: f64,
}

/// Список пулов, с которыми бот будет работать
pub const POOLS: &[PoolConfig] = &[
    PoolConfig {
        name: "WSOL/USDC",
        pool_address: "3ucNos4NbumPLZNWztqGHNFFgkHeRMBQAVemeeomsUxv",
        usdc_mint_address: "EPjFWdd5AufqSS3ZQSijzCetra7T1Y9NPpF7Rmgv8dZ",
        wsol_mint_address: "So11111111111111111111111111111111111111112",
        initial_amount_usdc: 40.0,
    },
    // Добавляйте сюда новые пулы по той же схеме
];
