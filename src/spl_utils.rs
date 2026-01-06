use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use anyhow::Result;

// Token Program ID (معمولی)
pub const TOKEN_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

// ✅ Token-2022 Program ID (برای PumpFun)
pub const TOKEN_2022_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

// ═══════════════════════════════════════════════════════════════
// ✅ CRITICAL FIX: Use EXACT token_program_id from victim transaction!
// ═══════════════════════════════════════════════════════════════

/// ✅ محاسبه Associated Token Address با Token Program ID دلخواه
/// CRITICAL: استفاده از دقیقاً همان token_program_id که از victim tx گرفتیم!
pub fn get_associated_token_address_with_program_id(
    wallet: &Pubkey,
    mint: &Pubkey,
    token_program_id: &Pubkey,
) -> Pubkey {
    let seeds = &[
        wallet.as_ref(),
        token_program_id.as_ref(),
        mint.as_ref(),
    ];

    Pubkey::find_program_address(seeds, &ASSOCIATED_TOKEN_PROGRAM_ID).0
}

/// ✅ ساخت instruction برای create ATA با Token Program ID دلخواه
/// CRITICAL: استفاده از دقیقاً همان token_program_id که از victim tx گرفتیم!
pub fn create_associated_token_account_with_program_id(
    payer: &Pubkey,
    wallet: &Pubkey,
    mint: &Pubkey,
    token_program_id: &Pubkey,
) -> Instruction {
    let associated_token_address = get_associated_token_address_with_program_id(
        wallet,
        mint,
        token_program_id,
    );

    Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(associated_token_address, false),
            AccountMeta::new_readonly(*wallet, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(*token_program_id, false),  // ✅ از victim tx!
        ],
        data: vec![1],  // IDEMPOTENT: 1 byte = skip if exists
    }
}

/// ✅ ساخت instruction برای close account با Token Program ID دلخواه
/// CRITICAL: استفاده از دقیقاً همان token_program_id که از victim tx گرفتیم!
pub fn close_account_with_program_id(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
    token_program_id: &Pubkey,
) -> Result<Instruction> {
    Ok(Instruction {
        program_id: *token_program_id,  // ✅ از victim tx!
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data: vec![9], // CloseAccount instruction
    })
}

// ═══════════════════════════════════════════════════════════════
// توابع DEPRECATED - فقط برای سازگاری با کدهای قدیمی
// ═══════════════════════════════════════════════════════════════

/// محاسبه Associated Token Address (Token Program معمولی) - DEPRECATED
#[deprecated(note = "Use get_associated_token_address_with_program_id instead")]
pub fn get_associated_token_address(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    get_associated_token_address_with_program_id(wallet, mint, &TOKEN_PROGRAM_ID)
}

/// محاسبه Associated Token Address (Token-2022 Program) - DEPRECATED
#[deprecated(note = "Use get_associated_token_address_with_program_id instead")]
pub fn get_associated_token_address_2022(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    get_associated_token_address_with_program_id(wallet, mint, &TOKEN_2022_PROGRAM_ID)
}

/// ساخت instruction برای create ATA - Token Program معمولی - DEPRECATED
#[deprecated(note = "Use create_associated_token_account_with_program_id instead")]
pub fn create_associated_token_account(
    payer: &Pubkey,
    wallet: &Pubkey,
    mint: &Pubkey,
) -> Instruction {
    create_associated_token_account_with_program_id(payer, wallet, mint, &TOKEN_PROGRAM_ID)
}

/// ساخت instruction برای create ATA - Token-2022 - DEPRECATED
#[deprecated(note = "Use create_associated_token_account_with_program_id instead")]
pub fn create_associated_token_account_2022(
    payer: &Pubkey,
    wallet: &Pubkey,
    mint: &Pubkey,
) -> Instruction {
    create_associated_token_account_with_program_id(payer, wallet, mint, &TOKEN_2022_PROGRAM_ID)
}

/// ساخت instruction برای close account - Token Program معمولی - DEPRECATED
#[deprecated(note = "Use close_account_with_program_id instead")]
pub fn close_account(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
) -> Result<Instruction> {
    close_account_with_program_id(account, destination, owner, &TOKEN_PROGRAM_ID)
}

/// ساخت instruction برای close account - Token-2022 - DEPRECATED
#[deprecated(note = "Use close_account_with_program_id instead")]
pub fn close_account_2022(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
) -> Result<Instruction> {
    close_account_with_program_id(account, destination, owner, &TOKEN_2022_PROGRAM_ID)
}
