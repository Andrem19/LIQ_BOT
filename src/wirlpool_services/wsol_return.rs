//! sweep.rs – утилита «подмести» кошелёк.
//! • Закрывает все нулевые ATA (любой формат, в т. ч. с extensions).
//! • Закрывает пустые mint-счёты (если authority = wallet).

use anyhow::{anyhow, Context, Result};
use arrayref::{array_refs, array_ref};
use solana_client::{
    rpc_client::RpcClient,
    rpc_request::TokenAccountsFilter,
};
use solana_program::{
    program_pack::Pack,
    program_option::COption,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    instruction::Instruction,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    system_program,
    transaction::Transaction,
};
use spl_token::{
    instruction as token_ix,
    native_mint,
    state::{Account as TokenAccount, Mint as TokenMint},
    ID as TOKEN_PID,
};

/// Подмести кошелёк: закрыть нулевые ATA + пустые mint-счёты.
pub fn sweep_wallet(rpc: RpcClient, keypair_path: &str) -> Result<()> {
    //------------------------------------------------------------------ 0. RPC

    let kp: Keypair = read_keypair_file(keypair_path)
        .map_err(|e| anyhow!("read_keypair_file: {e}"))?;
    let wallet = kp.pubkey();

    //------------------------------------------------------------------ 1. ATA
    let ata_list = rpc.get_token_accounts_by_owner(
        &wallet,
        TokenAccountsFilter::ProgramId(TOKEN_PID),
    )?;

    // каждый счёт обрабатываем отдельно → «битые» не мешают остальным
    for keyed in ata_list {
        let ata_pk: Pubkey = keyed.pubkey.parse()?;
        let raw = rpc.get_account(&ata_pk)?;

        // владельцем должен быть SPL-Token
        if raw.owner != TOKEN_PID {
            continue;
        }

        // баланс через RPC (понимает extensions)
        let balance: u64 = rpc
            .get_token_account_balance(&ata_pk)?
            .amount
            .parse()
            .unwrap_or(1);

        if balance != 0 {
            continue;
        }

        //----------------------------------------------------------------
        // определяем mint «вручную» — первые 32 байта данных счёта
        //----------------------------------------------------------------
        if raw.data.len() < 32 {
            continue;                // мусорный аккаунт – пропускаем
        }
        let mint_bytes = array_ref![raw.data, 0, 32];
        let mint_pk = Pubkey::new_from_array(*mint_bytes);

        //------------------------------------------------------------ ix-ы
        let mut ix: Vec<Instruction> = Vec::new();
        if mint_pk == native_mint::id() {
            ix.push(token_ix::sync_native(&TOKEN_PID, &ata_pk)?);
        }
        ix.push(token_ix::close_account(
            &TOKEN_PID,
            &ata_pk,
            &wallet,
            &wallet,
            &[],
        )?);

        send_once(&rpc, &kp, ix, &ata_pk)?;
    }

    //------------------------------------------------------------------ 2. mint
    // идём по ВСЕМ аккаунтам владельца (ProgramId == System) – это
    // дёшево и позволяет найти «осиротевшие» mint-счёты
    //------------------------------------------------------------------
    let mut mint_ix: Vec<Instruction> = Vec::new();
    let mint_cand = rpc.get_token_accounts_by_owner(
        &wallet,
        TokenAccountsFilter::ProgramId(TOKEN_PID),
    )?;

    for keyed in mint_cand {
        let pk: Pubkey = keyed.pubkey.parse()?;
        let raw = rpc.get_account(&pk)?;

        // нужен именно mint-счёт
        if raw.owner != TOKEN_PID || raw.data.len() != <TokenMint as Pack>::LEN {
            continue;
        }

        let mint = TokenMint::unpack_from_slice(&raw.data)?;
        let authority_is_me = match mint.mint_authority {
            COption::Some(a) if a == wallet => true,
            _ => false,
        };

        if mint.supply == 0 && authority_is_me {
            mint_ix.push(token_ix::close_account(
                &TOKEN_PID,
                &pk,
                &wallet,
                &wallet,
                &[],
            )?);
        }
    }

    if !mint_ix.is_empty() {
        send_once(&rpc, &kp, mint_ix, &native_mint::id())?;
    }

    println!("✅  sweep_wallet завершён.");
    Ok(())
}

/// Helper: подписать и отправить одну транзакцию.
/// Ошибки не фейлят процесс — счёт просто пропускается.
fn send_once(rpc: &RpcClient, payer: &Keypair, ix: Vec<Instruction>, dbg_acc: &Pubkey) -> Result<()> {
    let bh = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(&ix, Some(&payer.pubkey()), &[payer], bh);

    match rpc.send_and_confirm_transaction_with_spinner(&tx) {
        Ok(sig) => {
            println!("  • closed {dbg_acc}  ✅  ({sig})");
            Ok(())
        }
        Err(e) => {
            println!("  • skip   {dbg_acc}  ⚠️  ({e})");
            Ok(()) // продолжаем обрабатывать остальные
        }
    }
}

