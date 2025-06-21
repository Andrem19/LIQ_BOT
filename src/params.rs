use std::time::Duration;
use crate::types::PoolConfig;
use crate::types::{LiqPosition, Role};
use once_cell::sync::Lazy;
use tokio::sync::Mutex;

/// Единственный lock, защищающий любые операции,
/// которые *могут изменить* баланс кошелька
pub static WALLET_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

pub const WSOL: &str = "So11111111111111111111111111111111111111112"; // 9
pub const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"; // 6
pub const RAY:  &str = "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R"; // 6
pub const WETH: &str = "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs"; // 8

pub const RPC_URL: &str = "https://api.mainnet-beta.solana.com";
pub const KEYPAIR_FILENAME: &str = "/home/jupiter/.config/solana/mainnet-id.json";
pub const OVR: f64 = 1.01;           // 0.3 % запас
pub const PCT_LIST_1: [f64; 4] = [0.002, 0.005, 0.014, 0.003];
pub const WEIGHTS: [f64; 3] = [45.0, 40.0, 15.0];
pub const TOTAL_USDC_SOLUSDC: f64 = 140.0;
pub const USDC_SOLRAY: f64 = 55.0;
pub const USDC_SOLWETH: f64 = 55.0;


pub const POOL: PoolConfig = PoolConfig {
    amount: 200.0,
    program: "whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc",
    name: "WSOL/USDC",
    pool_address: "",
    position_1: None,
    position_2: None,
    position_3: None,
    mint_a: "So11111111111111111111111111111111111111112",
    mint_b: "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
    decimal_a: 9,
    decimal_b: 6,
    sol_init: 0.0
};

