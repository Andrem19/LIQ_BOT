use std::time::Duration;
use crate::types::{PoolConfig, Range};
use crate::types::{LiqPosition, Role};
use once_cell::sync::Lazy;
use tokio::sync::Mutex;

/// Единственный lock, защищающий любые операции,
/// которые *могут изменить* баланс кошелька
pub static WALLET_MUTEX: Lazy<Mutex<()>> = Lazy::new(|| Mutex::new(()));

pub const WSOL: &str = "So11111111111111111111111111111111111111112"; // 9
pub const USDC: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"; // 6
pub const USDT: &str = "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB"; //6
pub const RAY:  &str = "4k3Dyjzvzp8eMZWUXbBCjEvwSkkk59S5iCNLY3QrkX6R"; // 6
pub const WETH: &str = "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs"; // 8
pub const WBTC: &str = "3NZ9JMVBmGAqocybic2c7LQCJScmgsAZ6vQqTDzcqmJh"; //8

pub const RPC_URL: &str = "https://api.mainnet-beta.solana.com";
pub const KEYPAIR_FILENAME: &str = "/home/jupiter/.config/solana/mainnet-id.json";
pub const OVR: f64 = 1.03;
pub const PCT_LIST_1: [f64; 4] = [0.001, 0.003, 0.006, 0.004];
pub const PCT_LIST_2: [f64; 4] = [0.002, 0.002, 0.007, 0.007];
pub const PCT_NUMBER: u16 = 1;
pub fn weights_1() -> Vec<f64> {
    vec![25.0, 20.0, 55.0]
}

/// Второй вектор весов
pub fn weights_2() -> Vec<f64> {
    vec![80.0, 20.0]
}

pub const WEIGHTS_NUMBER: u16 = 1;
pub const AMOUNT: f64 = 200.0;
pub const POOL_NUMBER: u16 = 2;
pub const INFO_INTERVAL: u16 = 5;
pub const COMPRESS: bool = false;
pub const RANGE: Range = Range::Three;


