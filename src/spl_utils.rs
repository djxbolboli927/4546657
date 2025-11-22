use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use anyhow::Result;

// Token Program ID
pub const TOKEN_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// محاسبه Associated Token Address
pub fn get_associated_token_address(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    let seeds = &[
        wallet.as_ref(),
        TOKEN_PROGRAM_ID.as_ref(),
        mint.as_ref(),
    ];

    Pubkey::find_program_address(seeds, &ASSOCIATED_TOKEN_PROGRAM_ID).0
}

/// ساخت instruction برای create ATA - IDEMPOTENT VERSION
///
/// CRITICAL CHANGE: data = vec![1] instead of vec![]
///
/// This single byte makes the instruction idempotent:
/// - If ATA exists: instruction succeeds (skips creation)
/// - If ATA doesn't exist: instruction creates it normally
///
/// This is essential for retry-safe execution in high-frequency trading bots!
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
        data: vec![1],  // ✅ IDEMPOTENT: 1 byte = skip if exists
    }
}

/// ساخت instruction برای close account
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
