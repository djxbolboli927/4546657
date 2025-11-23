use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use anyhow::Result;

// Token Program ID (معمولی)
pub const TOKEN_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

// ✅ Token-2022 Program ID (برای PumpFun) - این همان است که PumpFun استفاده می‌کند!
pub const TOKEN_2022_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// محاسبه Associated Token Address (Token Program معمولی)
pub fn get_associated_token_address(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    let seeds = &[
        wallet.as_ref(),
        TOKEN_PROGRAM_ID.as_ref(),
        mint.as_ref(),
    ];

    Pubkey::find_program_address(seeds, &ASSOCIATED_TOKEN_PROGRAM_ID).0
}

/// ✅ محاسبه Associated Token Address (Token-2022 Program - برای PumpFun)
/// CRITICAL: PumpFun uses Token-2022, not standard Token Program!
pub fn get_associated_token_address_2022(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    let seeds = &[
        wallet.as_ref(),
        TOKEN_2022_PROGRAM_ID.as_ref(),
        mint.as_ref(),
    ];

    Pubkey::find_program_address(seeds, &ASSOCIATED_TOKEN_PROGRAM_ID).0
}

/// ساخت instruction برای create ATA - IDEMPOTENT VERSION (Token Program معمولی)
pub fn create_associated_token_account(
    payer: &Pubkey,
    wallet: &Pubkey,
    mint: &Pubkey,
) -> Instruction {
    let associated_token_address = get_associated_token_address(wallet, mint);

    Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(associated_token_address, false),
            AccountMeta::new_readonly(*wallet, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(TOKEN_PROGRAM_ID, false),
        ],
        data: vec![1],  // IDEMPOTENT: 1 byte = skip if exists
    }
}

/// ✅ ساخت instruction برای create ATA - Token-2022 (برای PumpFun)
/// CRITICAL FIX: This uses Token-2022 Program ID!
pub fn create_associated_token_account_2022(
    payer: &Pubkey,
    wallet: &Pubkey,
    mint: &Pubkey,
) -> Instruction {
    let associated_token_address = get_associated_token_address_2022(wallet, mint);

    Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(associated_token_address, false),
            AccountMeta::new_readonly(*wallet, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(TOKEN_2022_PROGRAM_ID, false),  // ✅ TOKEN-2022!
        ],
        data: vec![1],  // IDEMPOTENT: 1 byte = skip if exists
    }
}

/// ساخت instruction برای close account (Token Program معمولی)
pub fn close_account(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
) -> Result<Instruction> {
    Ok(Instruction {
        program_id: TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data: vec![9], // CloseAccount instruction
    })
}

/// ✅ ساخت instruction برای close account (Token-2022 - برای PumpFun)
/// CRITICAL FIX: This uses Token-2022 Program ID!
pub fn close_account_2022(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
) -> Result<Instruction> {
    Ok(Instruction {
        program_id: TOKEN_2022_PROGRAM_ID,  // ✅ TOKEN-2022!
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data: vec![9], // CloseAccount instruction
    })
}
