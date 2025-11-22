use anyhow::Result;
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
    get_associated_token_address,
    create_associated_token_account,
    close_account,
};

pub struct TransactionBuilder {
    pub rpc_client: solana_client::rpc_client::RpcClient,
}

impl TransactionBuilder {
    pub fn new(rpc_endpoint: &str) -> Self {
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
        let user_token_account = get_associated_token_address(&buyer.pubkey(), mint);

        let mut instructions = Vec::new();

        instructions.push(
            ComputeBudgetInstruction::set_compute_unit_limit(200_000)
        );

        instructions.push(
            ComputeBudgetInstruction::set_compute_unit_price(priority_fee_microlamports)
        );

        instructions.push(
            create_associated_token_account(
                &buyer.pubkey(),
                &buyer.pubkey(),
                mint,
            )
        );

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

        let message = Message::new_with_blockhash(
            &instructions,
            Some(&buyer.pubkey()),
            &recent_blockhash,
        );

        let mut transaction = Transaction::new_unsigned(message);
        transaction.sign(&[buyer], recent_blockhash);

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
        let user_token_account = get_associated_token_address(&seller.pubkey(), mint);

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
                bonding_curve,
                creator_vault,
                &user_token_account,
                token_amount,
                min_sol_output,
            )?
        );

        instructions.push(
            close_account(
                &user_token_account,
                &seller.pubkey(),
                &seller.pubkey(),
            )?
        );

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

    pub async fn get_recent_blockhash(&self) -> Result<Hash> {
        let blockhash = self.rpc_client
            .get_latest_blockhash()
            .map_err(|e| anyhow::anyhow!("Failed to get blockhash: {}", e))?;

        Ok(blockhash)
    }
}
