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
    create_buy_instruction, create_sell_instruction, derive_bonding_curve,
};
use crate::spl_utils::{
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

    /// ساخت تراکنش خرید (Front-Run) با ۱۶ اکانت
    #[allow(clippy::too_many_arguments)]
    pub async fn build_front_run_transaction(
        &self,
        buyer: &Keypair,
        mint: &Pubkey,
        creator_vault: &Pubkey,
        token_amount: u64,
        max_sol_cost: u64,
        priority_fee_microlamports: u64,
        recent_blockhash: Hash,
        token_program_type: TokenProgramType,
        token_program_id: &Pubkey,
    ) -> Result<Transaction> {

        let bonding_curve = derive_bonding_curve(mint);

        let bonding_curve_token_account = get_associated_token_address_with_program_id(
            &bonding_curve,
            mint,
            token_program_id,
        );

        let user_token_account = get_associated_token_address_with_program_id(
            &buyer.pubkey(),
            mint,
            token_program_id,
        );

        let mut instructions = Vec::new();

        instructions.push(
            ComputeBudgetInstruction::set_compute_unit_limit(250_000)
        );

        instructions.push(
            ComputeBudgetInstruction::set_compute_unit_price(priority_fee_microlamports)
        );

        let create_ata_ix = create_associated_token_account_with_program_id(
            &buyer.pubkey(),
            &buyer.pubkey(),
            mint,
            token_program_id,
        );
        instructions.push(create_ata_ix);

        instructions.push(
            create_buy_instruction(
                &buyer.pubkey(),
                mint,
                &bonding_curve,
                &bonding_curve_token_account,
                &user_token_account,
                token_amount,
                max_sol_cost,
                token_program_id,
                creator_vault,
            )?
        );

        let message = Message::new_with_blockhash(
            &instructions,
            Some(&buyer.pubkey()),
            &recent_blockhash,
        );

        let mut transaction = Transaction::new_unsigned(message);
        transaction.sign(&[buyer], recent_blockhash);

        Ok(transaction)
    }

    /// ساخت تراکنش فروش (Back-Run)
    #[allow(clippy::too_many_arguments)]
    pub async fn build_back_run_transaction(
        &self,
        seller: &Keypair,
        mint: &Pubkey,
        creator_vault: &Pubkey,
        token_amount: u64,
        min_sol_output: u64,
        priority_fee_microlamports: u64,
        jito_tip_lamports: u64,
        jito_tip_account: &Pubkey,
        recent_blockhash: Hash,
        _token_program_type: TokenProgramType,
        token_program_id: &Pubkey,
    ) -> Result<Transaction> {
        let bonding_curve = derive_bonding_curve(mint);
        let bonding_curve_token_account = get_associated_token_address_with_program_id(
            &bonding_curve,
            mint,
            token_program_id,
        );
        let user_token_account = get_associated_token_address_with_program_id(
            &seller.pubkey(),
            mint,
            token_program_id,
        );

        let mut instructions = Vec::new();

        instructions.push(
            ComputeBudgetInstruction::set_compute_unit_limit(400_000)
        );

        instructions.push(
            ComputeBudgetInstruction::set_compute_unit_price(priority_fee_microlamports)
        );

        instructions.push(
            create_sell_instruction(
                &seller.pubkey(),
                mint,
                &bonding_curve,
                &bonding_curve_token_account,
                &user_token_account,
                token_amount,
                min_sol_output,
                token_program_id,
                creator_vault,
            )?
        );

        let close_account_ix = close_account_with_program_id(
            &user_token_account,
            &seller.pubkey(),
            &seller.pubkey(),
            token_program_id,
        )?;
        instructions.push(close_account_ix);

        instructions.push(
            system_instruction::transfer(
                &seller.pubkey(),
                jito_tip_account,
                jito_tip_lamports,
            )
        );

        let message = Message::new_with_blockhash(
            &instructions,
            Some(&seller.pubkey()),
            &recent_blockhash,
        );

        let mut transaction = Transaction::new_unsigned(message);
        transaction.sign(&[seller], recent_blockhash);

        Ok(transaction)
    }

    /// ساخت تراکنش پرداخت انعام (tip) به Jito
    /// این تراکنش باید در باندل قرار بگیرد تا Jito bundle را بپذیرد
    pub fn build_jito_tip_transaction(
        &self,
        payer: &Keypair,
        tip_lamports: u64,
        recent_blockhash: Hash,
    ) -> Result<Transaction> {
        // آدرس‌های تیپ Jito برای Frankfurt region
        const JITO_TIP_ACCOUNTS: [&str; 8] = [
            "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
            "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
            "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
            "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
            "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
            "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
            "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
            "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
        ];

        // انتخاب یکی از آدرس‌ها (می‌توان random کرد، اما فعلا اولی را می‌گیریم)
        let tip_account = JITO_TIP_ACCOUNTS[0]
            .parse::<Pubkey>()
            .map_err(|e| anyhow::anyhow!("Invalid Jito tip account: {}", e))?;

        debug!("💰 Building Jito tip transaction: {} lamports to {}", tip_lamports, tip_account);

        // ساخت instruction انتقال SOL
        let transfer_ix = system_instruction::transfer(
            &payer.pubkey(),
            &tip_account,
            tip_lamports,
        );

        // ساخت message و transaction
        let message = Message::new_with_blockhash(
            &[transfer_ix],
            Some(&payer.pubkey()),
            &recent_blockhash,
        );

        let mut transaction = Transaction::new_unsigned(message);
        transaction.sign(&[payer], recent_blockhash);

        Ok(transaction)
    }

    pub async fn get_recent_blockhash(&self) -> Result<Hash> {
        let blockhash = self.rpc_client
            .get_latest_blockhash()
            .map_err(|e| anyhow::anyhow!("Failed to get blockhash: {}", e))?;
        Ok(blockhash)
    }
}
