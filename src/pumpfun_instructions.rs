use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    sysvar,
};
use std::str::FromStr;
use crate::jito_client::TokenProgramType;

pub const PUMP_FUN_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";
pub const GLOBAL_CONFIG: &str = "4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf";
pub const FEE_RECIPIENT: &str = "CebN5WGQ4jvEPvsVU4EoHEpgzq1VV7AbicfhtW4xC9iM";
pub const EVENT_AUTHORITY: &str = "Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1";
pub const GLOBAL_VOLUME_ACCUMULATOR: &str = "Hq2wp8uJ9jCPsYgNHex8RtqdvMPfVGoYwjvF1ATiwn2Y";
pub const FEE_CONFIG: &str = "8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt";
pub const FEE_PROGRAM: &str = "pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ";

pub const BUY_DISCRIMINATOR: [u8; 8] = [0x66, 0x06, 0x3d, 0x12, 0x01, 0xda, 0xeb, 0xea];
pub const SELL_DISCRIMINATOR: [u8; 8] = [0x33, 0xe6, 0x85, 0xa4, 0x01, 0x7f, 0x83, 0xad];

pub fn derive_user_volume_accumulator(user: &Pubkey) -> Pubkey {
    let program_id = Pubkey::from_str(PUMP_FUN_PROGRAM).unwrap();
    let seeds = &[b"user_volume_accumulator", user.as_ref()];
    Pubkey::find_program_address(seeds, &program_id).0
}

pub fn derive_bonding_curve(mint: &Pubkey) -> Pubkey {
    let program_id = Pubkey::from_str(PUMP_FUN_PROGRAM).unwrap();
    let seeds = &[b"bonding-curve", mint.as_ref()];
    Pubkey::find_program_address(seeds, &program_id).0
}

// ✅ BUY: همان کد قدیمی که کار می‌کرد (16 accounts)
// ✅ fee_recipient از victim transaction استخراج می‌شود
pub fn create_buy_instruction(
    buyer: &Pubkey,
    mint: &Pubkey,
    bonding_curve: &Pubkey,
    bonding_curve_token_account: &Pubkey,
    user_token_account: &Pubkey,
    token_amount: u64,
    max_sol_cost: u64,
    token_program_id: &Pubkey,
    creator_vault: &Pubkey,
    fee_recipient: &Pubkey,  // ✅ اضافه شد: از victim tx
) -> Result<Instruction> {
    let global_config = Pubkey::from_str(GLOBAL_CONFIG)?;
    let event_authority = Pubkey::from_str(EVENT_AUTHORITY)?;
    let program_id = Pubkey::from_str(PUMP_FUN_PROGRAM)?;
    let global_volume = Pubkey::from_str(GLOBAL_VOLUME_ACCUMULATOR)?;
    let fee_config = Pubkey::from_str(FEE_CONFIG)?;
    let fee_program = Pubkey::from_str(FEE_PROGRAM)?;

    let user_volume_accumulator = derive_user_volume_accumulator(buyer);

    let accounts = vec![
        AccountMeta::new_readonly(global_config, false),
        AccountMeta::new(*fee_recipient, false),  // ✅ از victim tx
        AccountMeta::new_readonly(*mint, false),
        AccountMeta::new(*bonding_curve, false),
        AccountMeta::new(*bonding_curve_token_account, false),
        AccountMeta::new(*user_token_account, false),
        AccountMeta::new(*buyer, true),
        AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
        AccountMeta::new_readonly(*token_program_id, false),
        AccountMeta::new(*creator_vault, false),
        AccountMeta::new_readonly(event_authority, false),
        AccountMeta::new_readonly(program_id, false),
        AccountMeta::new(global_volume, false),
        AccountMeta::new(user_volume_accumulator, false),
        AccountMeta::new_readonly(fee_config, false),
        AccountMeta::new_readonly(fee_program, false),
    ];

    let mut data = Vec::new();
    data.extend_from_slice(&BUY_DISCRIMINATOR);
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&max_sol_cost.to_le_bytes());

    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}

// ✅ SELL: 14 accounts طبق تصویر Solscan
// ✅ fee_recipient از victim transaction استخراج می‌شود
pub fn create_sell_instruction(
    seller: &Pubkey,
    mint: &Pubkey,
    bonding_curve: &Pubkey,
    bonding_curve_token_account: &Pubkey,
    user_token_account: &Pubkey,
    token_amount: u64,
    min_sol_output: u64,
    token_program_id: &Pubkey,
    creator_vault: &Pubkey,
    fee_recipient: &Pubkey,  // ✅ اضافه شد: از victim tx
) -> Result<Instruction> {
    let global_config = Pubkey::from_str(GLOBAL_CONFIG)?;
    let event_authority = Pubkey::from_str(EVENT_AUTHORITY)?;
    let program_id = Pubkey::from_str(PUMP_FUN_PROGRAM)?;
    let fee_config = Pubkey::from_str(FEE_CONFIG)?;
    let fee_program = Pubkey::from_str(FEE_PROGRAM)?;

    // ✅ SELL: فقط 14 accounts (بدون volume tracking و associated token program)
    let accounts = vec![
        AccountMeta::new_readonly(global_config, false),           // #1 - Global
        AccountMeta::new(*fee_recipient, false),                   // #2 - Fee Recipient (از victim tx)
        AccountMeta::new_readonly(*mint, false),                   // #3 - Mint
        AccountMeta::new(*bonding_curve, false),                   // #4 - Bonding Curve
        AccountMeta::new(*bonding_curve_token_account, false),     // #5 - Associated Bonding Curve
        AccountMeta::new(*user_token_account, false),              // #6 - Associated User
        AccountMeta::new(*seller, true),                           // #7 - User
        AccountMeta::new_readonly(solana_sdk::system_program::id(), false), // #8 - System Program
        AccountMeta::new(*creator_vault, false),                   // #9 - Creator Vault
        AccountMeta::new_readonly(*token_program_id, false),       // #10 - Token Program
        AccountMeta::new_readonly(event_authority, false),         // #11 - Event Authority
        AccountMeta::new_readonly(program_id, false),              // #12 - Program
        AccountMeta::new_readonly(fee_config, false),              // #13 - Fee Config
        AccountMeta::new_readonly(fee_program, false),             // #14 - Fee Program
    ];

    let mut data = Vec::new();
    data.extend_from_slice(&SELL_DISCRIMINATOR);
    data.extend_from_slice(&token_amount.to_le_bytes());
    data.extend_from_slice(&min_sol_output.to_le_bytes());

    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}
