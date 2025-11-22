use anyhow::{Result, anyhow};
use solana_sdk::{
    signature::{Keypair, read_keypair_file, Signer},
    pubkey::Pubkey,
};
use std::path::Path;

pub struct WalletManager {
    pub front_runner: Keypair,
    pub token_receiver: Pubkey,
    pub tip_payer: Keypair,
}

impl WalletManager {
    pub fn new(
        front_runner_path: impl AsRef<Path>,
        token_receiver_path: impl AsRef<Path>,
        tip_payer_path: impl AsRef<Path>,
    ) -> Result<Self> {
        let front_runner = read_keypair_file(front_runner_path.as_ref())
            .map_err(|e| anyhow!("Failed to read front runner keypair: {}", e))?;

        let token_receiver_keypair = read_keypair_file(token_receiver_path.as_ref())
            .map_err(|e| anyhow!("Failed to read token receiver keypair: {}", e))?;
        let token_receiver = token_receiver_keypair.pubkey();

        let tip_payer = read_keypair_file(tip_payer_path.as_ref())
            .map_err(|e| anyhow!("Failed to read tip payer keypair: {}", e))?;

        Ok(Self {
            front_runner,
            token_receiver,
            tip_payer,
        })
    }

    pub fn get_addresses(&self) -> (String, String, String) {
        (
            self.front_runner.pubkey().to_string(),
            self.token_receiver.to_string(),
            self.tip_payer.pubkey().to_string(),
        )
    }

    pub fn validate_addresses(&self) -> bool {
        let front_runner_address = self.front_runner.pubkey().to_string();
        let token_receiver_address = self.token_receiver.to_string();
        let tip_payer_address = self.tip_payer.pubkey().to_string();

        front_runner_address == "7U1YMyiKggYRCnQMBHknEFMv12iuwXTdmgD7z2zXZmcn" &&
        token_receiver_address == "Fxd29tMTBpdCKRj3hCB26o9fX9ZKb2rAQS5eJR22hQ5g" &&
        tip_payer_address == "65kYw4URNNAegKPHGektJn29cuaoEygpxnQFYSrU6a8K"
    }
}
