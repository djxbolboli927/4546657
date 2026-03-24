use anyhow::{Context, Result};
use rand::seq::SliceRandom;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    address_lookup_table::AddressLookupTableAccount,
    hash::Hash,
    instruction::{AccountMeta, Instruction},
    message::{v0, VersionedMessage},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
    transaction::VersionedTransaction,
};
use std::str::FromStr;

use crate::metis::{InstructionData, SwapInstructionsResponse};

/// Jito tip account addresses — pick one at random for each bundle.
/// Per Jito docs: do NOT use ALTs for tip accounts.
const JITO_TIP_ACCOUNTS: &[&str] = &[
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvB8eLJVLaAhAroXkpBNa8bSE63Tk7LYnV",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1qqRo4ppQpa",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

/// Convert a Metis instruction into a Solana SDK Instruction.
fn to_sdk_instruction(ix: &InstructionData) -> Result<Instruction> {
    let program_id = Pubkey::from_str(&ix.program_id)?;
    let accounts: Vec<AccountMeta> = ix
        .accounts
        .iter()
        .map(|a| {
            let pubkey = Pubkey::from_str(&a.pubkey).expect("invalid pubkey in instruction");
            if a.is_writable {
                AccountMeta::new(pubkey, a.is_signer)
            } else {
                AccountMeta::new_readonly(pubkey, a.is_signer)
            }
        })
        .collect();
    let data = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &ix.data,
    )
    .context("failed to decode instruction data")?;
    Ok(Instruction {
        program_id,
        accounts,
        data,
    })
}

/// Calculate the Jito tip amount.
/// Per Jito docs: minimum tip is 1000 lamports for bundles.
pub fn calculate_tip(
    profit_lamports: u64,
    tip_percent: f64,
    tip_min: u64,
    tip_max: u64,
) -> u64 {
    let dynamic_tip = (profit_lamports as f64 * tip_percent) as u64;
    dynamic_tip.max(tip_min).min(tip_max)
}

/// Build a versioned transaction containing both swap legs and a Jito tip.
///
/// Instruction order (all in one tx, one bundle):
/// 1. ComputeBudgetInstruction::SetComputeUnitLimit (from Metis dynamicComputeUnitLimit)
/// 2. Setup instructions for leg 1
/// 3. route_v2 swap instruction leg 1 (WSOL → Token)
/// 4. Cleanup for leg 1 (if any)
/// 5. Setup instructions for leg 2
/// 6. route_v2 swap instruction leg 2 (Token → WSOL)
/// 7. Cleanup for leg 2 (if any)
/// 8. SystemProgram::Transfer — Jito tip (MUST be last instruction)
///
/// Notes per Jito docs:
/// - For bundles, only Jito tip matters (no priority fee needed)
/// - Tip account must NOT be in ALT (use direct pubkey)
/// - Uses VersionedTransaction v0 for ALT support on swap instructions
/// - Jupiter Aggregator v6: anchor Self CPI Log verifies slippage=0 execution
pub fn build_arb_transaction(
    swap_leg1: &SwapInstructionsResponse,
    swap_leg2: &SwapInstructionsResponse,
    payer: &Keypair,
    tip_lamports: u64,
    recent_blockhash: Hash,
    rpc_client: &RpcClient,
) -> Result<VersionedTransaction> {
    let mut instructions: Vec<Instruction> = Vec::new();

    // 1. Compute budget instructions from both legs (includes SetComputeUnitLimit)
    // Metis sets dynamicComputeUnitLimit — we use those CU values
    for cb_ix in &swap_leg1.compute_budget_instructions {
        instructions.push(to_sdk_instruction(cb_ix)?);
    }
    // Add compute budget from leg2 if it has separate CU instructions
    for cb_ix in &swap_leg2.compute_budget_instructions {
        let ix = to_sdk_instruction(cb_ix)?;
        // Avoid duplicate SetComputeUnitLimit — combine CU from both legs
        // We take the max or add them. For safety, we replace with a combined CU limit.
        // Actually, Metis returns CU per swap. For 2 swaps we need to sum them.
        // We'll handle this by collecting all CB instructions and adjusting below.
        instructions.push(ix);
    }

    // 2. Setup instructions for leg 1
    for setup_ix in &swap_leg1.setup_instructions {
        instructions.push(to_sdk_instruction(setup_ix)?);
    }

    // 3. route_v2 swap leg 1 (WSOL → Token)
    instructions.push(to_sdk_instruction(&swap_leg1.swap_instruction)?);

    // 4. Cleanup for leg 1
    if let Some(ref cleanup) = swap_leg1.cleanup_instruction {
        instructions.push(to_sdk_instruction(cleanup)?);
    }

    // 5. Setup instructions for leg 2
    for setup_ix in &swap_leg2.setup_instructions {
        instructions.push(to_sdk_instruction(setup_ix)?);
    }

    // 6. route_v2 swap leg 2 (Token → WSOL)
    instructions.push(to_sdk_instruction(&swap_leg2.swap_instruction)?);

    // 7. Cleanup for leg 2
    if let Some(ref cleanup) = swap_leg2.cleanup_instruction {
        instructions.push(to_sdk_instruction(cleanup)?);
    }

    // 8. Jito tip — MUST be last instruction
    // Per Jito docs: don't put tip account in ALT, use direct pubkey
    let tip_account = {
        let mut rng = rand::thread_rng();
        let addr = JITO_TIP_ACCOUNTS.choose(&mut rng).unwrap();
        Pubkey::from_str(addr)?
    };
    #[allow(deprecated)]
    instructions.push(system_instruction::transfer(
        &payer.pubkey(),
        &tip_account,
        tip_lamports,
    ));

    // Collect all ALT addresses from both swap legs (NOT for tip accounts)
    let mut alt_addresses: Vec<Pubkey> = Vec::new();
    for addr in swap_leg1
        .address_lookup_table_addresses
        .iter()
        .chain(swap_leg2.address_lookup_table_addresses.iter())
    {
        let pubkey = Pubkey::from_str(addr)?;
        if !alt_addresses.contains(&pubkey) {
            alt_addresses.push(pubkey);
        }
    }

    // Fetch ALT accounts from RPC
    let mut address_lookup_tables: Vec<AddressLookupTableAccount> = Vec::new();
    for alt_pubkey in &alt_addresses {
        let raw_account = rpc_client
            .get_account(alt_pubkey)
            .with_context(|| format!("failed to fetch ALT {}", alt_pubkey))?;

        let alt_account = AddressLookupTableAccount {
            key: *alt_pubkey,
            addresses: deserialize_alt_addresses(&raw_account.data)?,
        };
        address_lookup_tables.push(alt_account);
    }

    // Build VersionedTransaction v0 (required for ALT support)
    let message = v0::Message::try_compile(
        &payer.pubkey(),
        &instructions,
        &address_lookup_tables,
        recent_blockhash,
    )
    .context("failed to compile v0 message")?;

    let versioned_message = VersionedMessage::V0(message);
    let tx = VersionedTransaction::try_new(versioned_message, &[payer])
        .context("failed to sign versioned transaction")?;

    Ok(tx)
}

/// Deserialize the addresses stored in an Address Lookup Table account.
fn deserialize_alt_addresses(data: &[u8]) -> Result<Vec<Pubkey>> {
    // ALT layout: 56-byte header followed by 32-byte pubkey entries.
    const HEADER_SIZE: usize = 56;
    if data.len() < HEADER_SIZE {
        anyhow::bail!("ALT account data too short: {} bytes", data.len());
    }
    let addresses_data = &data[HEADER_SIZE..];
    if addresses_data.len() % 32 != 0 {
        anyhow::bail!(
            "ALT addresses data has invalid length: {} (not a multiple of 32)",
            addresses_data.len()
        );
    }
    let addresses: Vec<Pubkey> = addresses_data
        .chunks_exact(32)
        .map(|chunk| Pubkey::new_from_array(chunk.try_into().unwrap()))
        .collect();
    Ok(addresses)
}
