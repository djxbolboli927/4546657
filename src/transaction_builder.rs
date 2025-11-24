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
    // ✅ NEW: استفاده از توابع جدید که token_program_id را به عنوان پارامتر می‌گیرند
    get_associated_token_address_with_program_id,
    create_associated_token_account_with_program_id,
    close_account_with_program_id,
};
use crate::jito_client::TokenProgramType;

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
        token_program_type: TokenProgramType,
        fee_recipient: &Pubkey,  // ✅ NEW: از victim tx
        bonding_curve_token_account: &Pubkey,  // ✅ NEW: از victim tx
        token_program_id: &Pubkey,  // ✅ NEW: از victim tx
    ) -> Result<Transaction> {
        // ✅ محاسبه User Token Account با استفاده از دقیقاً همان token_program_id از victim tx
        let user_token_account = get_associated_token_address_with_program_id(
            &buyer.pubkey(),
            mint,
            token_program_id,  // ✅ از victim tx!
        );

        let token_program_name = match token_program_type {
            TokenProgramType::Token2022Program => "Token-2022",
            TokenProgramType::TokenProgram => "Token Program",
        };

        debug!("🔨 Building FRONT-RUN transaction:");
        debug!("   Buyer: {}", buyer.pubkey());
        debug!("   Mint: {}", mint);
        debug!("   Bonding Curve: {}", bonding_curve);
        debug!("   Creator Vault: {}", creator_vault);
        debug!("   Token Program: {}", token_program_name);
        debug!("   User Token Account: {}", user_token_account);
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

        // ✅ Create ATA با استفاده از دقیقاً همان token_program_id از victim tx
        let create_ata_ix = create_associated_token_account_with_program_id(
            &buyer.pubkey(),
            &buyer.pubkey(),
            mint,
            token_program_id,  // ✅ از victim tx!
        );
        instructions.push(create_ata_ix);
        debug!("   ✅ Added create ATA instruction ({})", token_program_name);

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
                token_program_type,
                fee_recipient,  // ✅ NEW: از victim tx
                bonding_curve_token_account,  // ✅ NEW: از victim tx
                token_program_id,  // ✅ NEW: از victim tx
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
        token_program_type: TokenProgramType,
        fee_recipient: &Pubkey,  // ✅ NEW: از victim tx
        bonding_curve_token_account: &Pubkey,  // ✅ NEW: از victim tx
        token_program_id: &Pubkey,  // ✅ NEW: از victim tx
    ) -> Result<Transaction> {
        // ✅ محاسبه User Token Account با استفاده از دقیقاً همان token_program_id از victim tx
        let user_token_account = get_associated_token_address_with_program_id(
            &seller.pubkey(),
            mint,
            token_program_id,  // ✅ از victim tx!
        );

        let token_program_name = match token_program_type {
            TokenProgramType::Token2022Program => "Token-2022",
            TokenProgramType::TokenProgram => "Token Program",
        };

        debug!("🔨 Building BACK-RUN transaction:");
        debug!("   Seller: {}", seller.pubkey());
        debug!("   Mint: {}", mint);
        debug!("   Bonding Curve: {}", bonding_curve);
        debug!("   Creator Vault: {}", creator_vault);
        debug!("   Token Program: {}", token_program_name);
        debug!("   User Token Account: {}", user_token_account);
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
                token_program_type,
                fee_recipient,  // ✅ NEW: از victim tx
                bonding_curve_token_account,  // ✅ NEW: از victim tx
                token_program_id,  // ✅ NEW: از victim tx
            )?
        );
        debug!("   ✅ Added sell instruction");

        // ✅ Close account با استفاده از دقیقاً همان token_program_id از victim tx
        let close_account_ix = close_account_with_program_id(
            &user_token_account,
            &seller.pubkey(),
            &seller.pubkey(),
            token_program_id,  // ✅ از victim tx!
        )?;
        instructions.push(close_account_ix);
        debug!("   ✅ Added close account instruction ({})", token_program_name);

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
