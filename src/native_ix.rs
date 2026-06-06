//! Native swap instruction builders — zero Metis/Jupiter involvement.
//!
//! Each builder reads pool state directly from `PoolStateStore`, derives tick
//! array / oracle PDAs, and returns a ready-to-sign `Instruction`.
//!
//! Supported DEXes:
//!   • Raydium CLMM   (swap_v2 — Token-2022 compatible)
//!   • Orca Whirlpool (swap)
//!
//! Unsupported DEXes return `Err(NativeIxError::UnsupportedDex)` so the
//! no-metis executor can log and skip rather than panic.

use anyhow::{bail, Result};
use solana_sdk::{
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
};

use crate::pool_state_store::PoolStateStore;

// ── Program IDs ───────────────────────────────────────────────────────────────

pub const SPL_TOKEN: Pubkey =
    Pubkey::from_str_const("TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA");
pub const TOKEN_2022: Pubkey =
    Pubkey::from_str_const("TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb");
pub const MEMO_PROGRAM: Pubkey =
    Pubkey::from_str_const("MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr");
pub const RAYDIUM_CLMM_PROGRAM: Pubkey =
    Pubkey::from_str_const("CAMMCzo5YL8w4VFF8KVHrK22GGUsp5VTaW7grrKgrWqK");
pub const ORCA_WHIRLPOOL_PROGRAM: Pubkey =
    Pubkey::from_str_const("whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc");
pub const RAYDIUM_CPMM_PROGRAM: Pubkey =
    Pubkey::from_str_const("CPMMoo8L3F4NbTegBCKVNunggL7H1ZpdTHKxQB5qKP1C");

// ── Anchor discriminator helper ───────────────────────────────────────────────

/// Compute an Anchor instruction discriminator: sha256("global:<name>")[0..8].
fn anchor_disc(name: &str) -> [u8; 8] {
    let preimage = format!("global:{name}");
    let h = solana_sdk::hash::hash(preimage.as_bytes());
    h.to_bytes()[..8].try_into().unwrap()
}

// ── Raydium CLMM layout ───────────────────────────────────────────────────────
//
// PoolState (Anchor, LE) offsets used here:
//   8       bump (u8)
//   9..41   amm_config (Pubkey)
//   73..105 token_mint_0 (Pubkey)
//   137..169 token_vault_0 (Pubkey)
//   169..201 token_vault_1 (Pubkey)
//   201..233 observation_key (Pubkey)
//   235..237 tick_spacing (u16)
//   269..273 tick_current (i32)

const CLMM_AMM_CONFIG_OFF: usize = 9;
const CLMM_MINT_0_OFF: usize = 73;
const CLMM_VAULT_0_OFF: usize = 137;
const CLMM_VAULT_1_OFF: usize = 169;
const CLMM_OBS_OFF: usize = 201;
const CLMM_TICK_SPACING_OFF: usize = 235;
const CLMM_TICK_CURRENT_OFF: usize = 269;
const CLMM_MIN_LEN: usize = 273;

/// Raydium CLMM: ticks per TickArray.
const CLMM_TICKS_PER_ARRAY: i32 = 60;

const MIN_SQRT_PRICE: u128 = 4_295_048_016;
const MAX_SQRT_PRICE: u128 = 79_226_673_515_401_279_992_447_579_055;

fn clmm_tick_array_pda(pool: &Pubkey, start: i32) -> Pubkey {
    Pubkey::find_program_address(
        &[b"tick_array", pool.as_ref(), &start.to_be_bytes()],
        &RAYDIUM_CLMM_PROGRAM,
    )
    .0
}

/// Build a Raydium CLMM `swap_v2` instruction.
///
/// Direction is inferred from pool state: if `mint_in == token_mint_0`
/// then `zero_for_one = true`.
pub fn build_raydium_clmm(
    pool: &Pubkey,
    user: &Pubkey,
    mint_in: &Pubkey,
    mint_out: &Pubkey,
    amount_in: u64,
    min_out: u64,
    store: &PoolStateStore,
) -> Result<Instruction> {
    let pool_data = store
        .accounts
        .get(pool)
        .map(|r| r.data.clone())
        .ok_or_else(|| anyhow::anyhow!("RaydiumClmm pool {pool} not in store"))?;

    if pool_data.len() < CLMM_MIN_LEN {
        bail!("RaydiumClmm pool {pool} data too short ({})", pool_data.len());
    }

    let read_pk = |off: usize| -> Pubkey {
        Pubkey::from(<[u8; 32]>::try_from(&pool_data[off..off + 32]).unwrap())
    };

    let amm_config = read_pk(CLMM_AMM_CONFIG_OFF);
    let token_mint_0 = read_pk(CLMM_MINT_0_OFF);
    let vault_0 = read_pk(CLMM_VAULT_0_OFF);
    let vault_1 = read_pk(CLMM_VAULT_1_OFF);
    let obs_key = read_pk(CLMM_OBS_OFF);
    let tick_spacing = u16::from_le_bytes(
        pool_data[CLMM_TICK_SPACING_OFF..CLMM_TICK_SPACING_OFF + 2]
            .try_into()?,
    );
    let tick_current = i32::from_le_bytes(
        pool_data[CLMM_TICK_CURRENT_OFF..CLMM_TICK_CURRENT_OFF + 4]
            .try_into()?,
    );

    let zero_for_one = *mint_in == token_mint_0;
    let (input_vault, output_vault) = if zero_for_one {
        (vault_0, vault_1)
    } else {
        (vault_1, vault_0)
    };

    // User's associated token accounts for the swap pair.
    let user_in_ata =
        spl_associated_token_account::get_associated_token_address(user, mint_in);
    let user_out_ata =
        spl_associated_token_account::get_associated_token_address(user, mint_out);

    // Three tick arrays in the swap direction (current + 2 next).
    let tpa = tick_spacing as i32 * CLMM_TICKS_PER_ARRAY;
    let s0 = tick_current.div_euclid(tpa) * tpa;
    let (s1, s2) = if zero_for_one {
        (s0 - tpa, s0 - 2 * tpa)
    } else {
        (s0 + tpa, s0 + 2 * tpa)
    };
    let ta0 = clmm_tick_array_pda(pool, s0);
    let ta1 = clmm_tick_array_pda(pool, s1);
    let ta2 = clmm_tick_array_pda(pool, s2);

    let sqrt_price_limit: u128 = if zero_for_one {
        MIN_SQRT_PRICE + 1
    } else {
        MAX_SQRT_PRICE - 1
    };

    // Instruction data: discriminator + amount(u64) + min_out(u64) + sqrt_limit(u128) + is_base_input(bool)
    let disc = anchor_disc("swap_v2");
    let mut data = Vec::with_capacity(41);
    data.extend_from_slice(&disc);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
    data.push(1u8); // is_base_input = true

    let accounts = vec![
        AccountMeta::new(*user, true),             // payer
        AccountMeta::new_readonly(amm_config, false),
        AccountMeta::new(*pool, false),             // pool_state (writable)
        AccountMeta::new(user_in_ata, false),       // input_token_account
        AccountMeta::new(user_out_ata, false),      // output_token_account
        AccountMeta::new(input_vault, false),       // input_vault
        AccountMeta::new(output_vault, false),      // output_vault
        AccountMeta::new(obs_key, false),           // observation_state
        AccountMeta::new_readonly(SPL_TOKEN, false),
        AccountMeta::new_readonly(TOKEN_2022, false),
        AccountMeta::new_readonly(MEMO_PROGRAM, false),
        AccountMeta::new_readonly(*mint_in, false),  // input_vault_mint
        AccountMeta::new_readonly(*mint_out, false), // output_vault_mint
        AccountMeta::new(ta0, false),               // tick_array_0 (remaining)
        AccountMeta::new(ta1, false),               // tick_array_1 (remaining)
        AccountMeta::new(ta2, false),               // tick_array_2 (remaining)
    ];

    Ok(Instruction {
        program_id: RAYDIUM_CLMM_PROGRAM,
        accounts,
        data,
    })
}

// ── Orca Whirlpool layout ─────────────────────────────────────────────────────
//
// Whirlpool account (Anchor, LE) offsets:
//   41..43  tick_spacing (u16)
//   81..85  tick_current_index (i32)
//   101..133 token_mint_a (Pubkey)
//   133..165 token_vault_a (Pubkey)
//   181..213 token_mint_b (Pubkey)
//   213..245 token_vault_b (Pubkey)

const WP_TICK_SPACING_OFF: usize = 41;
const WP_TICK_CURRENT_OFF: usize = 81;
const WP_MINT_A_OFF: usize = 101;
const WP_VAULT_A_OFF: usize = 133;
const WP_MINT_B_OFF: usize = 181;
const WP_VAULT_B_OFF: usize = 213;
const WP_MIN_LEN: usize = 245;

/// Orca Whirlpool: ticks per TickArray.
const WP_TICKS_PER_ARRAY: i32 = 88;

fn wp_tick_array_start(tick: i32, tick_spacing: u16) -> i32 {
    let tpa = tick_spacing as i32 * WP_TICKS_PER_ARRAY;
    tick.div_euclid(tpa) * tpa
}

fn wp_tick_array_pda(pool: &Pubkey, start: i32) -> Pubkey {
    // Whirlpool uses the decimal string of start_tick_index as seed bytes.
    let s = start.to_string();
    Pubkey::find_program_address(
        &[b"tick_array", pool.as_ref(), s.as_bytes()],
        &ORCA_WHIRLPOOL_PROGRAM,
    )
    .0
}

fn wp_oracle_pda(pool: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"oracle", pool.as_ref()], &ORCA_WHIRLPOOL_PROGRAM).0
}

/// Build an Orca Whirlpool legacy `swap` instruction.
///
/// Direction is inferred: if `mint_in == token_mint_a` then `a_to_b = true`.
pub fn build_orca_whirlpool(
    pool: &Pubkey,
    user: &Pubkey,
    mint_in: &Pubkey,
    mint_out: &Pubkey,
    amount_in: u64,
    min_out: u64,
    store: &PoolStateStore,
) -> Result<Instruction> {
    let pool_data = store
        .accounts
        .get(pool)
        .map(|r| r.data.clone())
        .ok_or_else(|| anyhow::anyhow!("OrcaWhirlpool {pool} not in store"))?;

    if pool_data.len() < WP_MIN_LEN {
        bail!("OrcaWhirlpool {pool} data too short ({})", pool_data.len());
    }

    let read_pk = |off: usize| -> Pubkey {
        Pubkey::from(<[u8; 32]>::try_from(&pool_data[off..off + 32]).unwrap())
    };

    let mint_a = read_pk(WP_MINT_A_OFF);
    let mint_b = read_pk(WP_MINT_B_OFF);
    let vault_a = read_pk(WP_VAULT_A_OFF);
    let vault_b = read_pk(WP_VAULT_B_OFF);
    let tick_spacing = u16::from_le_bytes(
        pool_data[WP_TICK_SPACING_OFF..WP_TICK_SPACING_OFF + 2].try_into()?,
    );
    let tick_current = i32::from_le_bytes(
        pool_data[WP_TICK_CURRENT_OFF..WP_TICK_CURRENT_OFF + 4].try_into()?,
    );

    let a_to_b = *mint_in == mint_a;

    // User ATAs — always use A/B names aligned with pool, not swap direction.
    let ata_a = spl_associated_token_account::get_associated_token_address(user, &mint_a);
    let ata_b = spl_associated_token_account::get_associated_token_address(user, &mint_b);

    // Verify mint alignment with swap direction.
    let expected_out = if a_to_b { &mint_b } else { &mint_a };
    if mint_out != expected_out {
        bail!(
            "OrcaWhirlpool {pool}: mint_out mismatch (expected {expected_out}, got {mint_out})"
        );
    }

    // Three tick arrays.
    let start0 = wp_tick_array_start(tick_current, tick_spacing);
    let tpa = tick_spacing as i32 * WP_TICKS_PER_ARRAY;
    let (start1, start2) = if a_to_b {
        (start0 - tpa, start0 - 2 * tpa)
    } else {
        (start0 + tpa, start0 + 2 * tpa)
    };
    let ta0 = wp_tick_array_pda(pool, start0);
    let ta1 = wp_tick_array_pda(pool, start1);
    let ta2 = wp_tick_array_pda(pool, start2);

    let oracle = wp_oracle_pda(pool);

    let sqrt_price_limit: u128 = if a_to_b {
        MIN_SQRT_PRICE + 1
    } else {
        MAX_SQRT_PRICE - 1
    };

    // Data: discriminator + amount(u64) + other_amount_threshold(u64)
    //       + sqrt_price_limit(u128) + amount_specified_is_input(bool) + a_to_b(bool)
    let disc = anchor_disc("swap");
    let mut data = Vec::with_capacity(42);
    data.extend_from_slice(&disc);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());
    data.extend_from_slice(&sqrt_price_limit.to_le_bytes());
    data.push(1u8);       // amount_specified_is_input = true
    data.push(a_to_b as u8);

    let accounts = vec![
        AccountMeta::new_readonly(SPL_TOKEN, false),      // token_program
        AccountMeta::new_readonly(*user, true),            // token_authority (signer)
        AccountMeta::new(*pool, false),                    // whirlpool (writable)
        AccountMeta::new(ata_a, false),                    // token_owner_account_a
        AccountMeta::new(vault_a, false),                  // token_vault_a
        AccountMeta::new(ata_b, false),                    // token_owner_account_b
        AccountMeta::new(vault_b, false),                  // token_vault_b
        AccountMeta::new(ta0, false),                      // tick_array_0
        AccountMeta::new(ta1, false),                      // tick_array_1
        AccountMeta::new(ta2, false),                      // tick_array_2
        AccountMeta::new(oracle, false),                   // oracle
    ];

    Ok(Instruction {
        program_id: ORCA_WHIRLPOOL_PROGRAM,
        accounts,
        data,
    })
}

// ── Raydium CPMM layout ───────────────────────────────────────────────────────
//
// PoolState (Anchor, LE) offsets after 8-byte discriminator:
//   8:   amm_config    (Pubkey, 32 bytes)
//   40:  pool_creator  (Pubkey, 32 bytes)
//   72:  token_0_vault (Pubkey, 32 bytes)
//   104: token_1_vault (Pubkey, 32 bytes)
//   136: lp_mint       (Pubkey, 32 bytes)
//   168: token_0_mint  (Pubkey, 32 bytes)
//   200: token_1_mint  (Pubkey, 32 bytes)
//   232: token_0_program (Pubkey, 32 bytes)
//   264: token_1_program (Pubkey, 32 bytes)
//   296: observation_key (Pubkey, 32 bytes)
//
// Min length: 328 bytes.

const CPMM_AMM_CONFIG_OFF: usize = 8;
const CPMM_VAULT_0_OFF: usize = 72;
const CPMM_VAULT_1_OFF: usize = 104;
const CPMM_MINT_0_OFF: usize = 168;
const CPMM_MINT_1_OFF: usize = 200;
const CPMM_TOKEN_0_PROGRAM_OFF: usize = 232;
const CPMM_TOKEN_1_PROGRAM_OFF: usize = 264;
const CPMM_OBS_OFF: usize = 296;
const CPMM_MIN_LEN: usize = 328;

/// Build a Raydium CPMM `swap_base_input` instruction.
///
/// Direction is inferred from pool state: if `mint_in == token_0_mint`
/// then token_0 is input, otherwise token_1 is input.
pub fn build_raydium_cpmm(
    pool: &Pubkey,
    user: &Pubkey,
    mint_in: &Pubkey,
    mint_out: &Pubkey,
    amount_in: u64,
    min_out: u64,
    store: &PoolStateStore,
) -> Result<Instruction> {
    let pool_data = store
        .accounts
        .get(pool)
        .map(|r| r.data.clone())
        .ok_or_else(|| anyhow::anyhow!("RaydiumCpmm pool {pool} not in store"))?;

    if pool_data.len() < CPMM_MIN_LEN {
        bail!("RaydiumCpmm pool {pool} data too short ({})", pool_data.len());
    }

    let read_pk = |off: usize| -> Pubkey {
        Pubkey::from(<[u8; 32]>::try_from(&pool_data[off..off + 32]).unwrap())
    };

    let amm_config = read_pk(CPMM_AMM_CONFIG_OFF);
    let vault_0 = read_pk(CPMM_VAULT_0_OFF);
    let vault_1 = read_pk(CPMM_VAULT_1_OFF);
    let token_0_mint = read_pk(CPMM_MINT_0_OFF);
    let token_0_program = read_pk(CPMM_TOKEN_0_PROGRAM_OFF);
    let token_1_program = read_pk(CPMM_TOKEN_1_PROGRAM_OFF);
    let observation_key = read_pk(CPMM_OBS_OFF);

    let zero_for_one = *mint_in == token_0_mint;
    let (input_vault, output_vault, input_token_program, output_token_program) = if zero_for_one {
        (vault_0, vault_1, token_0_program, token_1_program)
    } else {
        (vault_1, vault_0, token_1_program, token_0_program)
    };

    // Authority PDA: seeds `[b"vault_and_lp_mint_auth_seed"]` with CPMM program.
    let (authority, _) = Pubkey::find_program_address(
        &[b"vault_and_lp_mint_auth_seed"],
        &RAYDIUM_CPMM_PROGRAM,
    );

    // User's associated token accounts for the swap pair.
    let user_in_ata =
        spl_associated_token_account::get_associated_token_address(user, mint_in);
    let user_out_ata =
        spl_associated_token_account::get_associated_token_address(user, mint_out);

    // Instruction data: discriminator + amount_in(u64) + min_out(u64)
    let disc = anchor_disc("swap_base_input");
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&disc);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());

    let accounts = vec![
        AccountMeta::new(*user, true),                              // 0. payer (writable, signer)
        AccountMeta::new_readonly(authority, false),                // 1. authority (readonly)
        AccountMeta::new_readonly(amm_config, false),               // 2. amm_config (readonly)
        AccountMeta::new(*pool, false),                             // 3. pool_state (writable)
        AccountMeta::new(user_in_ata, false),                       // 4. input_token_account (writable)
        AccountMeta::new(user_out_ata, false),                      // 5. output_token_account (writable)
        AccountMeta::new(input_vault, false),                       // 6. input_vault (writable)
        AccountMeta::new(output_vault, false),                      // 7. output_vault (writable)
        AccountMeta::new_readonly(input_token_program, false),      // 8. input_token_program (readonly)
        AccountMeta::new_readonly(output_token_program, false),     // 9. output_token_program (readonly)
        AccountMeta::new_readonly(*mint_in, false),                 // 10. input_token_mint (readonly)
        AccountMeta::new_readonly(*mint_out, false),                // 11. output_token_mint (readonly)
        AccountMeta::new(observation_key, false),                   // 12. observation_state (writable)
    ];

    Ok(Instruction {
        program_id: RAYDIUM_CPMM_PROGRAM,
        accounts,
        data,
    })
}

// ── Dispatch ─────────────────────────────────────────────────────────────────

/// Build a native swap instruction for the given DEX name.
///
/// Returns `Err` if:
/// - The DEX is not implemented (log `[no_metis_skip] reason=missing_native_ix_builder`)
/// - Pool account is missing from the store
/// - Pool data is corrupt or too short
pub fn build_swap(
    dex_name: &str,
    pool: &Pubkey,
    user: &Pubkey,
    mint_in: &Pubkey,
    mint_out: &Pubkey,
    amount_in: u64,
    min_out: u64,
    store: &PoolStateStore,
) -> Result<Instruction> {
    match dex_name {
        "RaydiumClmm" => {
            build_raydium_clmm(pool, user, mint_in, mint_out, amount_in, min_out, store)
        }
        "RaydiumCpmm" => {
            build_raydium_cpmm(pool, user, mint_in, mint_out, amount_in, min_out, store)
        }
        "OrcaWhirlpoolV1" => {
            build_orca_whirlpool(pool, user, mint_in, mint_out, amount_in, min_out, store)
        }
        other => bail!("missing_native_ix_builder dex={other}"),
    }
}
