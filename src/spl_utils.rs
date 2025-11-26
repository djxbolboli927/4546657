use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use anyhow::Result;
use std::str::FromStr;

// ✅ Token Program ID (original)
pub const TOKEN_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");

// ✅ Token-2022 Program ID (NEW!)
pub const TOKEN_2022_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");

// ✅ Associated Token Program ID
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

/// ✅ محاسبه Associated Token Address با پشتیبانی Token2022
pub fn get_associated_token_address_with_program_id(wallet: &Pubkey, mint: &Pubkey, token_program_id: &Pubkey) -> Pubkey {
    let seeds = &[
        wallet.as_ref(),
        token_program_id.as_ref(),
        mint.as_ref(),
    ];

    Pubkey::find_program_address(seeds, &ASSOCIATED_TOKEN_PROGRAM_ID).0
}

/// محاسبه Associated Token Address (Token Program قدیمی)
pub fn get_associated_token_address(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    get_associated_token_address_with_program_id(wallet, mint, &TOKEN_PROGRAM_ID)
}

/// ✅ تشخیص Token Program ID از string
pub fn parse_token_program_id(program_id_str: &str) -> Result<Pubkey> {
    let pubkey = Pubkey::from_str(program_id_str)?;

    // Validate it's one of the known token programs
    if pubkey == TOKEN_PROGRAM_ID || pubkey == TOKEN_2022_PROGRAM_ID {
        Ok(pubkey)
    } else {
        Err(anyhow::anyhow!("Invalid token program ID: {}", program_id_str))
    }
}

/// ✅ ساخت instruction برای create ATA - IDEMPOTENT VERSION با پشتیبانی Token2022
pub fn create_associated_token_account_with_program_id(
    payer: &Pubkey,
    wallet: &Pubkey,
    mint: &Pubkey,
    token_program_id: &Pubkey,
) -> Instruction {
    let associated_token_address = get_associated_token_address_with_program_id(wallet, mint, token_program_id);

    Instruction {
        program_id: ASSOCIATED_TOKEN_PROGRAM_ID,
        accounts: vec![
            AccountMeta::new(*payer, true),
            AccountMeta::new(associated_token_address, false),
            AccountMeta::new_readonly(*wallet, false),
            AccountMeta::new_readonly(*mint, false),
            AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
            AccountMeta::new_readonly(*token_program_id, false),  // ✅ استفاده از token_program_id صحیح!
        ],
        data: vec![1],  // ✅ IDEMPOTENT: 1 byte = skip if exists
    }
}

/// ساخت instruction برای create ATA (Token Program قدیمی)
pub fn create_associated_token_account(
    payer: &Pubkey,
    wallet: &Pubkey,
    mint: &Pubkey,
) -> Instruction {
    create_associated_token_account_with_program_id(payer, wallet, mint, &TOKEN_PROGRAM_ID)
}

/// ✅ ساخت instruction برای close account با پشتیبانی Token2022
pub fn close_account_with_program_id(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
    token_program_id: &Pubkey,
) -> Result<Instruction> {
    Ok(Instruction {
        program_id: *token_program_id,  // ✅ استفاده از token_program_id صحیح!
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data: vec![9], // CloseAccount instruction
    })
}

/// ساخت instruction برای close account (Token Program قدیمی)
pub fn close_account(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
) -> Result<Instruction> {
    close_account_with_program_id(account, destination, owner, &TOKEN_PROGRAM_ID)
}
