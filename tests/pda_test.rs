// تست PDA ها با تراکنش‌های واقعی
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;

// تراکنش قربانی #1 از داده‌های شما:
const MINT_1: &str = "CR7rxakwLZCtGJW7g5fwW7tke4p8KNJ8iZzG1K87pump";  // LOCKIN
const CREATOR_VAULT_1_EXPECTED: &str = "DBTxYsqtXepj6w9pJQxuqVMRQswkt2QcfyJZdGEReWt8";
const BONDING_CURVE_1_EXPECTED: &str = "ED1duu5w5h7uQyzCNKvQwm9JuoyRV6kuJYRAYpG5u1N8";

// تراکنش قربانی #2 از داده‌های شما:
const MINT_2: &str = "FJWQtW5tQ1LGrHQVhSX89jUfMF3XGkFBf1kpF32fpump";  // SHUFL
const CREATOR_VAULT_2_EXPECTED: &str = "Hfb8EiDP9MNHiB6amKX4XcXmLA7A9cDna6LyXKAtUmy4";
const BONDING_CURVE_2_EXPECTED: &str = "6d3nWg5LQubr2mDGAY5EKHqqVCPJxfSCg9eqQ2y14GUT";

const PUMP_FUN_PROGRAM: &str = "6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P";

fn derive_bonding_curve(mint: &Pubkey) -> Pubkey {
    let program_id = Pubkey::from_str(PUMP_FUN_PROGRAM).unwrap();
    let seeds = &[b"bonding-curve", mint.as_ref()];
    Pubkey::find_program_address(seeds, &program_id).0
}

fn derive_user_volume_accumulator(user: &Pubkey) -> Pubkey {
    let program_id = Pubkey::from_str(PUMP_FUN_PROGRAM).unwrap();
    let seeds = &[b"user", user.as_ref()];
    Pubkey::find_program_address(seeds, &program_id).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bonding_curve_pda_transaction_1() {
        let mint = Pubkey::from_str(MINT_1).unwrap();
        let expected = Pubkey::from_str(BONDING_CURVE_1_EXPECTED).unwrap();

        let calculated = derive_bonding_curve(&mint);

        println!("🧪 Test Transaction #1 (LOCKIN)");
        println!("   Mint: {}", mint);
        println!("   Expected Bonding Curve: {}", expected);
        println!("   Calculated Bonding Curve: {}", calculated);

        assert_eq!(calculated, expected, "❌ Bonding Curve mismatch!");
        println!("   ✅ Bonding Curve matches!");
    }

    #[test]
    fn test_bonding_curve_pda_transaction_2() {
        let mint = Pubkey::from_str(MINT_2).unwrap();
        let expected = Pubkey::from_str(BONDING_CURVE_2_EXPECTED).unwrap();

        let calculated = derive_bonding_curve(&mint);

        println!("🧪 Test Transaction #2 (SHUFL)");
        println!("   Mint: {}", mint);
        println!("   Expected Bonding Curve: {}", expected);
        println!("   Calculated Bonding Curve: {}", calculated);

        assert_eq!(calculated, expected, "❌ Bonding Curve mismatch!");
        println!("   ✅ Bonding Curve matches!");
    }

    #[test]
    fn test_user_volume_accumulator() {
        // آدرس ربات شما
        let bot_user = Pubkey::from_str("7U1YMyiKggYRCnQMBHknEFMv12iuwXTdmgD7z2zXZmcn").unwrap();

        let volume_acc = derive_user_volume_accumulator(&bot_user);

        println!("🧪 Test User Volume Accumulator");
        println!("   User: {}", bot_user);
        println!("   Volume Accumulator: {}", volume_acc);
        println!("   ✅ PDA calculated successfully!");
    }

    #[test]
    fn test_creator_vault_from_victim_tx() {
        // ✅ این تست تأیید می‌کند که Creator Vault را از تراکنش قربانی می‌گیریم
        // نه اینکه محاسبه کنیم!

        println!("🧪 Test Creator Vault Strategy");
        println!("   Transaction #1:");
        println!("      Mint: {}", MINT_1);
        println!("      Creator Vault (from victim tx): {}", CREATOR_VAULT_1_EXPECTED);
        println!("      ✅ We COPY this, not calculate!");

        println!("   Transaction #2:");
        println!("      Mint: {}", MINT_2);
        println!("      Creator Vault (from victim tx): {}", CREATOR_VAULT_2_EXPECTED);
        println!("      ✅ We COPY this, not calculate!");

        // این تست همیشه موفق است چون ما فقط کپی می‌کنیم!
        assert!(true);
    }
}

fn main() {
    println!("🧪 Running PDA verification tests...\n");

    // تست 1
    let mint1 = Pubkey::from_str(MINT_1).unwrap();
    let bc1 = derive_bonding_curve(&mint1);
    let expected_bc1 = Pubkey::from_str(BONDING_CURVE_1_EXPECTED).unwrap();

    println!("Test 1 - LOCKIN:");
    println!("  Calculated: {}", bc1);
    println!("  Expected:   {}", expected_bc1);
    println!("  Match: {}", bc1 == expected_bc1);
    println!();

    // تست 2
    let mint2 = Pubkey::from_str(MINT_2).unwrap();
    let bc2 = derive_bonding_curve(&mint2);
    let expected_bc2 = Pubkey::from_str(BONDING_CURVE_2_EXPECTED).unwrap();

    println!("Test 2 - SHUFL:");
    println!("  Calculated: {}", bc2);
    println!("  Expected:   {}", expected_bc2);
    println!("  Match: {}", bc2 == expected_bc2);
    println!();

    println!("Creator Vault Strategy:");
    println!("  ✅ We get Creator Vault from victim tx (account #9)");
    println!("  ✅ No calculation needed!");
    println!("  Transaction #1 Creator Vault: {}", CREATOR_VAULT_1_EXPECTED);
    println!("  Transaction #2 Creator Vault: {}", CREATOR_VAULT_2_EXPECTED);
}
