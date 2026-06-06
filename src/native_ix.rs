//! Native swap instruction builders — zero Metis/Jupiter involvement.
//!
//! Each builder reads pool state directly from `PoolStateStore`, derives tick
//! array / oracle PDAs, and returns a ready-to-sign `Instruction`.
//!
//! Supported DEXes:
//!   • Raydium CLMM      (swap_v2 — Token-2022 compatible)
//!   • Raydium CPMM      (swap_base_input)
//!   • Orca Whirlpool    (swap)
//!   • Meteora DAMM v2   (swap — cp-amm)
//!   • Meteora DLMM      (swap — bin-based, remaining_accounts = BinArrays)
//!   • Raydium AMM v4    (SwapBaseIn — requires Serum market in store)
//!
//! Unsupported DEXes return `Err` so the no-metis executor can log and skip.

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
pub const RAYDIUM_AMM_V4_PROGRAM: Pubkey =
    Pubkey::from_str_const("675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8");
pub const METEORA_DAMM_V2_PROGRAM: Pubkey =
    Pubkey::from_str_const("cpamdpZCGKUy5JxQXB4dcpGPiikHawvSWAd6mEn1sGG");
pub const METEORA_DLMM_PROGRAM: Pubkey =
    Pubkey::from_str_const("LBUZKhRxPF3XUpBCjp4YzTKgLccjZhTSDM9YuVaPwxo");
pub const PUMPSWAP_PROGRAM: Pubkey =
    Pubkey::from_str_const("pAMMBay6oceH9fJKBRHGP5D4bD4sWpmSwMn52FMfXEA");
pub const SYSTEM_PROGRAM: Pubkey =
    Pubkey::from_str_const("11111111111111111111111111111111");
pub const ASSOCIATED_TOKEN_PROGRAM: Pubkey =
    Pubkey::from_str_const("ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJe1bRS");

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

// ── Raydium AMM V4 layout ─────────────────────────────────────────────────────
//
// AmmInfo (no Anchor discriminator, raw LE layout):
//   [0..128]   header fields (status, nonce, decimals, flags, …)
//   [128..192] Fees (64 bytes)
//   [192..320] OutPutData (128 bytes)
//   [320..352] token_coin  (coin vault Pubkey)
//   [352..384] token_pc    (pc vault Pubkey)
//   [384..416] coin_mint_address
//   [416..448] pc_mint_address
//   [448..480] lp_mint_address
//   [480..512] open_orders  (Serum open-orders account)
//   [512..544] market       (Serum market)
//   [544..576] serum_dex    (Serum program id)
//
// Serum/OpenBook market layout (5-byte prefix + raw fields):
//   [5..13]   accountFlags (u64)
//   [13..45]  ownAddress   (Pubkey)
//   [45..53]  vaultSignerNonce (u64)
//   [53..85]  baseMint     (Pubkey)
//   [85..117] quoteMint    (Pubkey)
//   [117..149] baseVault   (Pubkey)  — serum coin vault
//   [165..197] quoteVault  (Pubkey)  — serum pc vault
//   [253..285] eventQueue  (Pubkey)
//   [285..317] bids        (Pubkey)
//   [317..349] asks        (Pubkey)

const AMM_V4_COIN_VAULT_OFF: usize = 320;
const AMM_V4_PC_VAULT_OFF: usize = 352;
const AMM_V4_OPEN_ORDERS_OFF: usize = 480;
const AMM_V4_MARKET_OFF: usize = 512;
const AMM_V4_SERUM_DEX_OFF: usize = 544;
const AMM_V4_MIN_LEN: usize = 576;

const SERUM_VAULT_SIGNER_NONCE_OFF: usize = 45;
const SERUM_COIN_VAULT_OFF: usize = 117;
const SERUM_PC_VAULT_OFF: usize = 165;
const SERUM_EVENT_QUEUE_OFF: usize = 253;
const SERUM_BIDS_OFF: usize = 285;
const SERUM_ASKS_OFF: usize = 317;
const SERUM_MARKET_MIN_LEN: usize = 349;

/// Build a Raydium AMM V4 `SwapBaseIn` (instruction id = 9) instruction.
///
/// Requires both the pool account AND the Serum market account to be present
/// in the store (both are subscribed via mix.json for AMM V4 pools).
pub fn build_raydium_amm_v4(
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
        .ok_or_else(|| anyhow::anyhow!("RaydiumAmmV4 pool {pool} not in store"))?;
    if pool_data.len() < AMM_V4_MIN_LEN {
        bail!("RaydiumAmmV4 pool {pool} data too short ({})", pool_data.len());
    }

    let read_pk = |off: usize| -> Pubkey {
        Pubkey::from(<[u8; 32]>::try_from(&pool_data[off..off + 32]).unwrap())
    };

    let coin_vault = read_pk(AMM_V4_COIN_VAULT_OFF);
    let pc_vault = read_pk(AMM_V4_PC_VAULT_OFF);
    let open_orders = read_pk(AMM_V4_OPEN_ORDERS_OFF);
    let market = read_pk(AMM_V4_MARKET_OFF);
    let serum_dex = read_pk(AMM_V4_SERUM_DEX_OFF);

    let user_source_ata = spl_associated_token_account::get_associated_token_address(user, mint_in);
    let user_dest_ata = spl_associated_token_account::get_associated_token_address(user, mint_out);

    // AMM authority is a program-level PDA shared across all AMM V4 pools.
    let (amm_authority, _) = Pubkey::find_program_address(
        &[b"amm authority"],
        &RAYDIUM_AMM_V4_PROGRAM,
    );

    // Read the Serum market account to get bids/asks/event_queue/vaults/nonce.
    let market_data = store
        .accounts
        .get(&market)
        .map(|r| r.data.clone())
        .ok_or_else(|| anyhow::anyhow!("RaydiumAmmV4: Serum market {market} not in store"))?;
    if market_data.len() < SERUM_MARKET_MIN_LEN {
        bail!("RaydiumAmmV4: Serum market {market} data too short ({})", market_data.len());
    }

    let read_market_pk = |off: usize| -> Pubkey {
        Pubkey::from(<[u8; 32]>::try_from(&market_data[off..off + 32]).unwrap())
    };
    let read_u64_le = |off: usize| -> u64 {
        u64::from_le_bytes(market_data[off..off + 8].try_into().unwrap())
    };

    let vault_signer_nonce = read_u64_le(SERUM_VAULT_SIGNER_NONCE_OFF);
    let serum_coin_vault = read_market_pk(SERUM_COIN_VAULT_OFF);
    let serum_pc_vault = read_market_pk(SERUM_PC_VAULT_OFF);
    let serum_event_queue = read_market_pk(SERUM_EVENT_QUEUE_OFF);
    let serum_bids = read_market_pk(SERUM_BIDS_OFF);
    let serum_asks = read_market_pk(SERUM_ASKS_OFF);

    let vault_signer = Pubkey::create_program_address(
        &[market.as_ref(), &vault_signer_nonce.to_le_bytes()],
        &serum_dex,
    )
    .map_err(|e| anyhow::anyhow!("RaydiumAmmV4: vault_signer derivation failed: {e:?}"))?;

    // SwapBaseIn: instruction byte = 9, then amount_in (u64), min_out (u64).
    let mut data = Vec::with_capacity(17);
    data.push(9u8);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());

    let accounts = vec![
        AccountMeta::new(*pool, false),                     // amm
        AccountMeta::new_readonly(amm_authority, false),    // amm_authority
        AccountMeta::new(open_orders, false),               // amm_open_orders
        AccountMeta::new(coin_vault, false),                // amm_coin_vault
        AccountMeta::new(pc_vault, false),                  // amm_pc_vault
        AccountMeta::new_readonly(serum_dex, false),        // serum_program
        AccountMeta::new(market, false),                    // serum_market
        AccountMeta::new(serum_bids, false),                // serum_bids
        AccountMeta::new(serum_asks, false),                // serum_asks
        AccountMeta::new(serum_event_queue, false),         // serum_event_queue
        AccountMeta::new(serum_coin_vault, false),          // serum_coin_vault
        AccountMeta::new(serum_pc_vault, false),            // serum_pc_vault
        AccountMeta::new_readonly(vault_signer, false),     // serum_vault_signer
        AccountMeta::new(user_source_ata, false),           // user_source_token
        AccountMeta::new(user_dest_ata, false),             // user_dest_token
        AccountMeta::new_readonly(*user, true),             // user_owner (signer)
        AccountMeta::new_readonly(SPL_TOKEN, false),        // token_program
    ];

    Ok(Instruction {
        program_id: RAYDIUM_AMM_V4_PROGRAM,
        accounts,
        data,
    })
}

// ── Meteora DAMM v2 layout ────────────────────────────────────────────────────
//
// Pool (cp-amm, Anchor, LE) key offsets:
//   [168..200] token_a_mint  (Pubkey)
//   [200..232] token_b_mint  (Pubkey)
//   [232..264] token_a_vault (Pubkey)
//   [264..296] token_b_vault (Pubkey)

const DAMM_V2_TOKEN_A_MINT_OFF: usize = 168;
const DAMM_V2_TOKEN_B_MINT_OFF: usize = 200;
const DAMM_V2_TOKEN_A_VAULT_OFF: usize = 232;
const DAMM_V2_TOKEN_B_VAULT_OFF: usize = 264;
const DAMM_V2_MIN_LEN: usize = 296;

/// Build a Meteora DAMM v2 (cp-amm) `swap` instruction.
///
/// Pool authority and event authority are program PDAs shared across all pools.
/// The token programs are inferred from the vault account owners in the store
/// (defaults to SPL Token if the vault isn't present yet).
pub fn build_meteora_damm_v2(
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
        .ok_or_else(|| anyhow::anyhow!("MeteoraDammV2 pool {pool} not in store"))?;
    if pool_data.len() < DAMM_V2_MIN_LEN {
        bail!("MeteoraDammV2 pool {pool} data too short ({})", pool_data.len());
    }

    let read_pk = |off: usize| -> Pubkey {
        Pubkey::from(<[u8; 32]>::try_from(&pool_data[off..off + 32]).unwrap())
    };

    let token_a_mint = read_pk(DAMM_V2_TOKEN_A_MINT_OFF);
    let token_b_mint = read_pk(DAMM_V2_TOKEN_B_MINT_OFF);
    let token_a_vault = read_pk(DAMM_V2_TOKEN_A_VAULT_OFF);
    let token_b_vault = read_pk(DAMM_V2_TOKEN_B_VAULT_OFF);

    let a_to_b = *mint_in == token_a_mint;
    let expected_out = if a_to_b { &token_b_mint } else { &token_a_mint };
    if mint_out != expected_out {
        bail!("MeteoraDammV2 {pool}: mint_out mismatch (expected {expected_out}, got {mint_out})");
    }

    // Infer token programs from vault owners (defaults to SPL Token).
    let vault_token_program = |vault: &Pubkey| -> Pubkey {
        store
            .accounts
            .get(vault)
            .map(|r| if r.owner == TOKEN_2022 { TOKEN_2022 } else { SPL_TOKEN })
            .unwrap_or(SPL_TOKEN)
    };
    let token_a_program = vault_token_program(&token_a_vault);
    let token_b_program = vault_token_program(&token_b_vault);

    let user_in_ata = spl_associated_token_account::get_associated_token_address(user, mint_in);
    let user_out_ata = spl_associated_token_account::get_associated_token_address(user, mint_out);

    let (pool_authority, _) = Pubkey::find_program_address(&[b"authority"], &METEORA_DAMM_V2_PROGRAM);
    let (event_authority, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &METEORA_DAMM_V2_PROGRAM);

    // When no referral: pass the output vault as the referral_token_account.
    // It has the correct mint for whichever token the fee is collected in.
    let referral_token_account = if a_to_b { token_b_vault } else { token_a_vault };

    let disc = anchor_disc("swap");
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&disc);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());

    let accounts = vec![
        AccountMeta::new_readonly(pool_authority, false),       // pool_authority
        AccountMeta::new(*pool, false),                         // pool (writable)
        AccountMeta::new(user_in_ata, false),                   // input_token_account
        AccountMeta::new(user_out_ata, false),                  // output_token_account
        AccountMeta::new(token_a_vault, false),                 // token_a_vault
        AccountMeta::new(token_b_vault, false),                 // token_b_vault
        AccountMeta::new_readonly(token_a_mint, false),         // token_a_mint
        AccountMeta::new_readonly(token_b_mint, false),         // token_b_mint
        AccountMeta::new(*user, true),                          // payer (writable, signer)
        AccountMeta::new_readonly(token_a_program, false),      // token_a_program
        AccountMeta::new_readonly(token_b_program, false),      // token_b_program
        AccountMeta::new(referral_token_account, false),        // referral_token_account
        AccountMeta::new_readonly(event_authority, false),      // event_authority
        AccountMeta::new_readonly(METEORA_DAMM_V2_PROGRAM, false), // program
    ];

    Ok(Instruction {
        program_id: METEORA_DAMM_V2_PROGRAM,
        accounts,
        data,
    })
}

// ── Meteora DLMM layout ───────────────────────────────────────────────────────
//
// LbPair (Anchor, LE) key offsets:
//   [76..80]   active_id    (i32)
//   [80..82]   bin_step     (u16)
//   [88..120]  token_x_mint (Pubkey)
//   [120..152] token_y_mint (Pubkey)
//   [152..184] reserve_x    (Pubkey)
//   [184..216] reserve_y    (Pubkey)
//
// Bin arrays are derived PDAs (seeds: ["bin_array", lb_pair, index_le8]).
// Active bin array index = active_id.div_euclid(70).

const DLMM_ACTIVE_ID_OFF: usize = 76;
const DLMM_BIN_STEP_OFF: usize = 80;
const DLMM_TOKEN_X_MINT_OFF: usize = 88;
const DLMM_TOKEN_Y_MINT_OFF: usize = 120;
const DLMM_RESERVE_X_OFF: usize = 152;
const DLMM_RESERVE_Y_OFF: usize = 184;
const DLMM_MIN_LEN: usize = 216;
const DLMM_BINS_PER_ARRAY: i32 = 70;

fn dlmm_bin_array_pda(lb_pair: &Pubkey, index: i32) -> Pubkey {
    let idx_le = (index as i64).to_le_bytes();
    Pubkey::find_program_address(
        &[b"bin_array", lb_pair.as_ref(), &idx_le],
        &METEORA_DLMM_PROGRAM,
    )
    .0
}

/// Build a Meteora DLMM `swap` instruction.
///
/// BinArray accounts are appended as remaining_accounts. We include the active
/// bin array plus two adjacent arrays in the swap direction to cover price
/// movement. Any missing bin arrays fall through to the RPC fallback in the
/// calibrator (they are derived PDAs that are typically subscribed in mix.json).
pub fn build_meteora_dlmm(
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
        .ok_or_else(|| anyhow::anyhow!("MeteoraDlmm pool {pool} not in store"))?;
    if pool_data.len() < DLMM_MIN_LEN {
        bail!("MeteoraDlmm pool {pool} data too short ({})", pool_data.len());
    }

    let read_pk = |off: usize| -> Pubkey {
        Pubkey::from(<[u8; 32]>::try_from(&pool_data[off..off + 32]).unwrap())
    };
    let active_id = i32::from_le_bytes(pool_data[DLMM_ACTIVE_ID_OFF..DLMM_ACTIVE_ID_OFF + 4].try_into()?);
    let token_x_mint = read_pk(DLMM_TOKEN_X_MINT_OFF);
    let token_y_mint = read_pk(DLMM_TOKEN_Y_MINT_OFF);
    let reserve_x = read_pk(DLMM_RESERVE_X_OFF);
    let reserve_y = read_pk(DLMM_RESERVE_Y_OFF);

    let swap_for_y = *mint_in == token_x_mint;
    let expected_out = if swap_for_y { &token_y_mint } else { &token_x_mint };
    if mint_out != expected_out {
        bail!("MeteoraDlmm {pool}: mint_out mismatch (expected {expected_out}, got {mint_out})");
    }

    let user_in_ata = spl_associated_token_account::get_associated_token_address(user, mint_in);
    let user_out_ata = spl_associated_token_account::get_associated_token_address(user, mint_out);

    let (oracle, _) = Pubkey::find_program_address(
        &[b"oracle", pool.as_ref()],
        &METEORA_DLMM_PROGRAM,
    );
    let (bitmap_extension, _) = Pubkey::find_program_address(
        &[b"bitmap_extension", pool.as_ref()],
        &METEORA_DLMM_PROGRAM,
    );
    let (event_authority, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &METEORA_DLMM_PROGRAM);

    // Infer token programs from reserve account owners.
    let reserve_token_program = |reserve: &Pubkey| -> Pubkey {
        store
            .accounts
            .get(reserve)
            .map(|r| if r.owner == TOKEN_2022 { TOKEN_2022 } else { SPL_TOKEN })
            .unwrap_or(SPL_TOKEN)
    };
    let token_x_program = reserve_token_program(&reserve_x);
    let token_y_program = reserve_token_program(&reserve_y);

    // Bin arrays: active + 2 in the swap direction.
    let active_index = active_id.div_euclid(DLMM_BINS_PER_ARRAY);
    let (i1, i2, i3) = if swap_for_y {
        (active_index, active_index - 1, active_index - 2)
    } else {
        (active_index, active_index + 1, active_index + 2)
    };
    let ba0 = dlmm_bin_array_pda(pool, i1);
    let ba1 = dlmm_bin_array_pda(pool, i2);
    let ba2 = dlmm_bin_array_pda(pool, i3);

    // host_fee_in: use the input reserve so the fee stays in-protocol.
    let host_fee_in = if swap_for_y { reserve_x } else { reserve_y };

    let disc = anchor_disc("swap");
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&disc);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());

    let mut accounts = vec![
        AccountMeta::new(*pool, false),                           // lb_pair (writable)
        AccountMeta::new_readonly(bitmap_extension, false),       // bin_array_bitmap_extension
        AccountMeta::new(reserve_x, false),                       // reserve_x (writable)
        AccountMeta::new(reserve_y, false),                       // reserve_y (writable)
        AccountMeta::new(user_in_ata, false),                     // user_token_in
        AccountMeta::new(user_out_ata, false),                    // user_token_out
        AccountMeta::new_readonly(token_x_mint, false),           // token_x_mint
        AccountMeta::new_readonly(token_y_mint, false),           // token_y_mint
        AccountMeta::new(oracle, false),                          // oracle (writable)
        AccountMeta::new(host_fee_in, false),                     // host_fee_in
        AccountMeta::new_readonly(*user, true),                   // user (signer)
        AccountMeta::new_readonly(token_x_program, false),        // token_x_program
        AccountMeta::new_readonly(token_y_program, false),        // token_y_program
        AccountMeta::new_readonly(event_authority, false),        // event_authority
        AccountMeta::new_readonly(METEORA_DLMM_PROGRAM, false),   // program
        // Remaining accounts: bin arrays
        AccountMeta::new(ba0, false),
        AccountMeta::new(ba1, false),
        AccountMeta::new(ba2, false),
    ];

    Ok(Instruction {
        program_id: METEORA_DLMM_PROGRAM,
        accounts,
        data,
    })
}

// ── PumpSwap layout ───────────────────────────────────────────────────────────
//
// Pool (Anchor, LE) key offsets (from pumpswap.rs):
//   [43..75]   base_mint  (Pubkey)
//   [75..107]  quote_mint (Pubkey)
//   [139..171] base_vault (Pubkey)
//   [171..203] quote_vault (Pubkey)
//
// GlobalConfig (Anchor, LE) — first protocol_fee_recipient at offset 64.
// The global_config PDA is ["global_config"] with PumpSwap program.

const PUMPSWAP_BASE_MINT_OFF: usize = 43;
const PUMPSWAP_QUOTE_MINT_OFF: usize = 75;
const PUMPSWAP_BASE_VAULT_OFF: usize = 139;
const PUMPSWAP_QUOTE_VAULT_OFF: usize = 171;
const PUMPSWAP_POOL_MIN_LEN: usize = 203;
const PUMPSWAP_GLOBAL_CONFIG_FEE_RECIPIENT_OFF: usize = 64; // first pubkey after admin

/// Build a PumpSwap `buy` or `sell` instruction depending on swap direction.
///
/// Requires the `global_config` PDA to be in the store so the fee recipient
/// can be read. If not in the store this returns Err (will log as no_builder).
pub fn build_pumpswap(
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
        .ok_or_else(|| anyhow::anyhow!("PumpSwap pool {pool} not in store"))?;
    if pool_data.len() < PUMPSWAP_POOL_MIN_LEN {
        bail!("PumpSwap pool {pool} data too short ({})", pool_data.len());
    }

    let read_pk = |off: usize| -> Pubkey {
        Pubkey::from(<[u8; 32]>::try_from(&pool_data[off..off + 32]).unwrap())
    };

    let base_mint = read_pk(PUMPSWAP_BASE_MINT_OFF);
    let quote_mint = read_pk(PUMPSWAP_QUOTE_MINT_OFF);
    let base_vault = read_pk(PUMPSWAP_BASE_VAULT_OFF);
    let quote_vault = read_pk(PUMPSWAP_QUOTE_VAULT_OFF);

    // Validate mints.
    let is_buy = *mint_in == quote_mint && *mint_out == base_mint;
    let is_sell = *mint_in == base_mint && *mint_out == quote_mint;
    if !is_buy && !is_sell {
        bail!("PumpSwap {pool}: mint mismatch (in={mint_in} out={mint_out} base={base_mint} quote={quote_mint})");
    }

    // Read global_config to get the fee recipient address.
    let (global_config, _) = Pubkey::find_program_address(&[b"global_config"], &PUMPSWAP_PROGRAM);
    let gc_data = store
        .accounts
        .get(&global_config)
        .map(|r| r.data.clone())
        .ok_or_else(|| anyhow::anyhow!("PumpSwap global_config not in store — cannot build swap"))?;
    if gc_data.len() < PUMPSWAP_GLOBAL_CONFIG_FEE_RECIPIENT_OFF + 32 {
        bail!("PumpSwap global_config data too short ({})", gc_data.len());
    }
    let fee_recipient = Pubkey::from(
        <[u8; 32]>::try_from(
            &gc_data[PUMPSWAP_GLOBAL_CONFIG_FEE_RECIPIENT_OFF
                ..PUMPSWAP_GLOBAL_CONFIG_FEE_RECIPIENT_OFF + 32],
        )
        .unwrap(),
    );

    let user_base_ata = spl_associated_token_account::get_associated_token_address(user, &base_mint);
    let user_quote_ata = spl_associated_token_account::get_associated_token_address(user, &quote_mint);
    let fee_recipient_ata =
        spl_associated_token_account::get_associated_token_address(&fee_recipient, &quote_mint);

    let (event_authority, _) =
        Pubkey::find_program_address(&[b"__event_authority"], &PUMPSWAP_PROGRAM);

    // buy = quote→base, sell = base→quote. Both use anchor_disc("buy"/"sell").
    let disc = if is_buy { anchor_disc("buy") } else { anchor_disc("sell") };
    let mut data = Vec::with_capacity(24);
    data.extend_from_slice(&disc);
    data.extend_from_slice(&amount_in.to_le_bytes());
    data.extend_from_slice(&min_out.to_le_bytes());

    let accounts = vec![
        AccountMeta::new_readonly(global_config, false),     // global_config
        AccountMeta::new(*pool, false),                      // pool (writable)
        AccountMeta::new_readonly(*user, true),              // user (signer)
        AccountMeta::new_readonly(base_mint, false),         // base_mint
        AccountMeta::new_readonly(quote_mint, false),        // quote_mint
        AccountMeta::new(user_base_ata, false),              // user_base_token_account
        AccountMeta::new(user_quote_ata, false),             // user_quote_token_account
        AccountMeta::new(base_vault, false),                 // pool_base_token_account
        AccountMeta::new(quote_vault, false),                // pool_quote_token_account
        AccountMeta::new_readonly(fee_recipient, false),     // protocol_fee_recipient
        AccountMeta::new(fee_recipient_ata, false),          // protocol_fee_recipient_token_account
        AccountMeta::new_readonly(SPL_TOKEN, false),         // base_token_program
        AccountMeta::new_readonly(SPL_TOKEN, false),         // quote_token_program
        AccountMeta::new_readonly(SYSTEM_PROGRAM, false),    // system_program
        AccountMeta::new_readonly(ASSOCIATED_TOKEN_PROGRAM, false), // associated_token_program
        AccountMeta::new_readonly(event_authority, false),   // event_authority
        AccountMeta::new_readonly(PUMPSWAP_PROGRAM, false),  // program
    ];

    Ok(Instruction {
        program_id: PUMPSWAP_PROGRAM,
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
        "MeteoraDammV2" => {
            build_meteora_damm_v2(pool, user, mint_in, mint_out, amount_in, min_out, store)
        }
        "MeteoraDlmm" => {
            build_meteora_dlmm(pool, user, mint_in, mint_out, amount_in, min_out, store)
        }
        "RaydiumAmmV4" => {
            build_raydium_amm_v4(pool, user, mint_in, mint_out, amount_in, min_out, store)
        }
        "PumpSwap" => {
            build_pumpswap(pool, user, mint_in, mint_out, amount_in, min_out, store)
        }
        other => bail!("missing_native_ix_builder dex={other}"),
    }
}
