use anyhow::Result;
use log::{info, debug, error};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    hash::Hash,
    message::Message,
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    system_instruction,
    transaction::Transaction,
};

use crate::pumpfun_instructions::{
    create_buy_instruction, create_sell_instruction,
};
use crate::spl_utils::{
    get_associated_token_address_2022,  // ✅ تغییر به Token-2022
    create_associated_token_account_2022,  // ✅ تغییر به Token-2022
    close_account_2022,  // ✅ تغییر به Token-2022
};

pub struct TransactionBuilder {
    pub rpc_client: solana_client::rpc_client::RpcClient,
}

impl TransactionBuilder {
    pub fn new(rpc_endpoint: &str) -> Self {
        info!("🔧 Transaction Builder initialized with RPC: {}", rpc_endpoint);
        Self {
            rpc_client: solana_client::rpc_client::RpcClient::new_with_commitment(
                rpc_endpoint.to_string(),
                CommitmentConfig::confirmed(),
            ),
        }
    }

    pub async fn build_front_run_transaction(
        &self,
        buyer: &Keypair,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        creator_vault: &Pubkey,
        token_amount: u64,
        max_sol_cost: u64,
        priority_fee_microlamports: u64,
        recent_blockhash: Hash,
    ) -> Result<Transaction> {
        // ✅ استفاده از Token-2022 برای محاسبه ATA
        let user_token_account = get_associated_token_address_2022(&buyer.pubkey(), mint);

        debug!("🔨 Building FRONT-RUN transaction:");
        debug!("   Buyer: {}", buyer.pubkey());
        debug!("   Mint: {}", mint);
        debug!("   Bonding Curve: {}", bonding_curve);
        debug!("   Creator Vault: {}", creator_vault);
        debug!("   User Token Account (Token-2022): {}", user_token_account);
        debug!("   Token Amount: {}", token_amount);
        debug!("   Max SOL Cost: {} lamports", max_sol_cost);
        debug!("   Priority Fee: {} μLamp", priority_fee_microlamports);

        let mut instructions = Vec::new();

        // Compute budget
        instructions.push(
            ComputeBudgetInstruction::set_compute_unit_limit(200_000)
        );
        debug!("   ✅ Added compute unit limit: 200,000");

        instructions.push(
            ComputeBudgetInstruction::set_compute_unit_price(priority_fee_microlamports)
        );
        debug!("   ✅ Added priority fee: {}", priority_fee_microlamports);

        // ✅ Create ATA با Token-2022
        instructions.push(
            create_associated_token_account_2022(
                &buyer.pubkey(),
                &buyer.pubkey(),
                mint,
            )
        );
        debug!("   ✅ Added create ATA instruction (Token-2022)");

        // Buy instruction
        instructions.push(
            create_buy_instruction(
                &buyer.pubkey(),
                mint,
                bonding_curve,
                creator_vault,
                &user_token_account,
                token_amount,
                max_sol_cost,
            )?
        );
        debug!("   ✅ Added buy instruction");

        let message = Message::new_with_blockhash(
            &instructions,
            Some(&buyer.pubkey()),
            &recent_blockhash,
        );

        let mut transaction = Transaction::new_unsigned(message);
        transaction.sign(&[buyer], recent_blockhash);

        info!("✅ Front-run transaction built successfully");
        info!("   Signature: {}", bs58::encode(&transaction.signatures[0]).into_string());
        info!("   Instructions count: {}", instructions.len());

        Ok(transaction)
    }

    pub async fn build_back_run_transaction(
        &self,
        seller: &Keypair,
        mint: &Pubkey,
        bonding_curve: &Pubkey,
        creator_vault: &Pubkey,
        token_amount: u64,
        min_sol_output: u64,
        priority_fee_microlamports: u64,
        jito_tip_lamports: u64,
        jito_tip_account: &Pubkey,
        recent_blockhash: Hash,
    ) -> Result<Transaction> {
        // ✅ استفاده از Token-2022 برای محاسبه ATA
        let user_token_account = get_associated_token_address_2022(&seller.pubkey(), mint);

        debug!("🔨 Building BACK-RUN transaction:");
        debug!("   Seller: {}", seller.pubkey());
        debug!("   Mint: {}", mint);
        debug!("   Bonding Curve: {}", bonding_curve);
        debug!("   Creator Vault: {}", creator_vault);
        debug!("   User Token Account (Token-2022): {}", user_token_account);
        debug!("   Token Amount: {}", token_amount);
        debug!("   Min SOL Output: {} lamports", min_sol_output);
        debug!("   Priority Fee: {} μLamp", priority_fee_microlamports);
        debug!("   Jito Tip: {} lamports", jito_tip_lamports);

        let mut instructions = Vec::new();

        // Compute budget
        instructions.push(
            ComputeBudgetInstruction::set_compute_unit_limit(400_000)
        );
        debug!("   ✅ Added compute unit limit: 400,000");

        instructions.push(
            ComputeBudgetInstruction::set_compute_unit_price(priority_fee_microlamports)
        );
        debug!("   ✅ Added priority fee: {}", priority_fee_microlamports);

        // Sell instruction
        instructions.push(
            create_sell_instruction(
                &seller.pubkey(),
                mint,
                bonding_curve,
                creator_vault,
                &user_token_account,
                token_amount,
                min_sol_output,
            )?
        );
        debug!("   ✅ Added sell instruction");

        // ✅ Close account با Token-2022
        instructions.push(
            close_account_2022(
                &user_token_account,
                &seller.pubkey(),
                &seller.pubkey(),
            )?
        );
        debug!("   ✅ Added close account instruction (Token-2022)");

        // Jito tip
        instructions.push(
            system_instruction::transfer(
                &seller.pubkey(),
                jito_tip_account,
                jito_tip_lamports,
            )
        );
        debug!("   ✅ Added Jito tip transfer");

        let message = Message::new_with_blockhash(
            &instructions,
            Some(&seller.pubkey()),
            &recent_blockhash,
        );

        let mut transaction = Transaction::new_unsigned(message);
        transaction.sign(&[seller], recent_blockhash);

        info!("✅ Back-run transaction built successfully");
        info!("   Signature: {}", bs58::encode(&transaction.signatures[0]).into_string());
        info!("   Instructions count: {}", instructions.len());

        Ok(transaction)
    }

    pub async fn get_recent_blockhash(&self) -> Result<Hash> {
        debug!("🔍 Fetching recent blockhash...");
        let blockhash = self.rpc_client
            .get_latest_blockhash()
            .map_err(|e| {
                error!("❌ Failed to get blockhash: {}", e);
                anyhow::anyhow!("Failed to get blockhash: {}", e)
            })?;

        debug!("   ✅ Blockhash: {}", blockhash);
        Ok(blockhash)
    }

    /// ✅ چک کردن وجود token account قبل از simulation
    pub async fn check_token_account_exists(&self, account: &Pubkey) -> bool {
        match self.rpc_client.get_account(account) {
            Ok(_) => {
                debug!("   ✅ Token account exists: {}", account);
                true
            }
            Err(_) => {
                debug!("   ⚠️  Token account does NOT exist: {}", account);
                false
            }
        }
    }
}
