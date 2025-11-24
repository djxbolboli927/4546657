use anyhow::Result;
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    system_program,
};
use std::str::FromStr;
use crate::spl_utils::{
    get_associated_token_address,
    get_associated_token_address_2022,
    TOKEN_PROGRAM_ID,
    TOKEN_2022_PROGRAM_ID,
};
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

// ═══════════════════════════════════════════════════════════
// ✅ PDA Derivation Functions
// ═══════════════════════════════════════════════════════════

/// ✅ محاسبه User Volume Accumulator PDA
/// Seeds: ["user", user_pubkey]
pub fn derive_user_volume_accumulator(user: &Pubkey) -> Pubkey {
    let program_id = Pubkey::from_str(PUMP_FUN_PROGRAM).unwrap();
    let seeds = &[b"user", user.as_ref()];
    Pubkey::find_program_address(seeds, &program_id).0
}

/// ✅ محاسبه Bonding Curve PDA
/// Seeds: ["bonding-curve", mint]
pub fn derive_bonding_curve(mint: &Pubkey) -> Pubkey {
    let program_id = Pubkey::from_str(PUMP_FUN_PROGRAM).unwrap();
    let seeds = &[b"bonding-curve", mint.as_ref()];
    Pubkey::find_program_address(seeds, &program_id).0
}

pub fn create_buy_instruction(
    buyer: &Pubkey,
    mint: &Pubkey,
    bonding_curve: &Pubkey,
    creator_vault: &Pubkey,
    user_token_account: &Pubkey,
    token_amount: u64,
    max_sol_cost: u64,
    token_program_type: TokenProgramType,  // ✅ NEW: پارامتر جدید
) -> Result<Instruction> {
    // ✅ انتخاب تابع مناسب بر اساس Token Program type
    let bonding_curve_token_account = match token_program_type {
        TokenProgramType::Token2022Program => {
            get_associated_token_address_2022(bonding_curve, mint)
        }
        TokenProgramType::TokenProgram => {
            get_associated_token_address(bonding_curve, mint)
        }
    };

    let token_program_id = match token_program_type {
        TokenProgramType::Token2022Program => TOKEN_2022_PROGRAM_ID,
        TokenProgramType::TokenProgram => TOKEN_PROGRAM_ID,
    };

    let user_volume_accumulator = derive_user_volume_accumulator(buyer);

    let global_config = Pubkey::from_str(GLOBAL_CONFIG)?;
    let fee_recipient = Pubkey::from_str(FEE_RECIPIENT)?;
    let event_authority = Pubkey::from_str(EVENT_AUTHORITY)?;
    let program_id = Pubkey::from_str(PUMP_FUN_PROGRAM)?;
    let global_volume = Pubkey::from_str(GLOBAL_VOLUME_ACCUMULATOR)?;
    let fee_config = Pubkey::from_str(FEE_CONFIG)?;
    let fee_program = Pubkey::from_str(FEE_PROGRAM)?;

    let accounts = vec![
        AccountMeta::new_readonly(global_config, false),
        AccountMeta::new(fee_recipient, false),
        AccountMeta::new_readonly(*mint, false),
        AccountMeta::new(*bonding_curve, false),
        AccountMeta::new(bonding_curve_token_account, false),
        AccountMeta::new(*user_token_account, false),
        AccountMeta::new(*buyer, true),
        AccountMeta::new_readonly(system_program::ID, false),
        AccountMeta::new_readonly(token_program_id, false),  // ✅ Dynamic Token Program!
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

pub fn create_sell_instruction(
    seller: &Pubkey,
    mint: &Pubkey,
    bonding_curve: &Pubkey,
    creator_vault: &Pubkey,
    user_token_account: &Pubkey,
    token_amount: u64,
    min_sol_output: u64,
    token_program_type: TokenProgramType,  // ✅ NEW: پارامتر جدید
) -> Result<Instruction> {
    // ✅ انتخاب تابع مناسب بر اساس Token Program type
    let bonding_curve_token_account = match token_program_type {
        TokenProgramType::Token2022Program => {
            get_associated_token_address_2022(bonding_curve, mint)
        }
        TokenProgramType::TokenProgram => {
            get_associated_token_address(bonding_curve, mint)
        }
    };

    let token_program_id = match token_program_type {
        TokenProgramType::Token2022Program => TOKEN_2022_PROGRAM_ID,
        TokenProgramType::TokenProgram => TOKEN_PROGRAM_ID,
    };

    let global_config = Pubkey::from_str(GLOBAL_CONFIG)?;
    let fee_recipient = Pubkey::from_str(FEE_RECIPIENT)?;
    let event_authority = Pubkey::from_str(EVENT_AUTHORITY)?;
    let program_id = Pubkey::from_str(PUMP_FUN_PROGRAM)?;
    let fee_config = Pubkey::from_str(FEE_CONFIG)?;
    let fee_program = Pubkey::from_str(FEE_PROGRAM)?;

    let accounts = vec![
        AccountMeta::new_readonly(global_config, false),
        AccountMeta::new(fee_recipient, false),
        AccountMeta::new_readonly(*mint, false),
        AccountMeta::new(*bonding_curve, false),
        AccountMeta::new(bonding_curve_token_account, false),
        AccountMeta::new(*user_token_account, false),
        AccountMeta::new(*seller, true),
        AccountMeta::new_readonly(system_program::ID, false),
        AccountMeta::new(*creator_vault, false),
        AccountMeta::new_readonly(token_program_id, false),  // ✅ Dynamic Token Program!
        AccountMeta::new_readonly(event_authority, false),
        AccountMeta::new_readonly(program_id, false),
        AccountMeta::new_readonly(fee_config, false),
        AccountMeta::new_readonly(fee_program, false),
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
