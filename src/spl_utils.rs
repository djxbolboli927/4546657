use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};
use anyhow::Result;

pub const TOKEN_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const ASSOCIATED_TOKEN_PROGRAM_ID: Pubkey = solana_sdk::pubkey!("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL");

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
            AccountMeta::new_readonly(*token_program_id, false),
        ],
        data: vec![1],
    }
}

pub fn close_account_with_program_id(
    account: &Pubkey,
    destination: &Pubkey,
    owner: &Pubkey,
    token_program_id: &Pubkey,
) -> Result<Instruction> {
    Ok(Instruction {
        program_id: *token_program_id,
        accounts: vec![
            AccountMeta::new(*account, false),
            AccountMeta::new(*destination, false),
            AccountMeta::new_readonly(*owner, true),
        ],
        data: vec![9],
    })
}

#[deprecated]
pub fn get_associated_token_address(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    get_associated_token_address_with_program_id(wallet, mint, &TOKEN_PROGRAM_ID)
}

#[deprecated]
pub fn get_associated_token_address_2022(wallet: &Pubkey, mint: &Pubkey) -> Pubkey {
    get_associated_token_address_with_program_id(wallet, mint, &TOKEN_2022_PROGRAM_ID)
}
