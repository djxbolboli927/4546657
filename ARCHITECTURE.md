# 🏗️ MEV Bot Architecture - Complete Documentation

## ✅ 4-Category Account Model

Based on your research, all 16 accounts in PumpFun transactions are categorized as follows:

### **Category 1: System Constants (8 accounts)** - HARDCODED ✅
These are global addresses that NEVER change:
- #1: Global Config: `4wTV1YmiEkRvAtNtsSGPtUrqRYQMe5SKy2uB4Jjaxnjf`
- #7: System Program: `11111111111111111111111111111111`
- #11: Event Authority: `Ce6TQqeHC9p8KetsN6JsjHK7UTZk7nasjjnr7XxXp9F1`
- #12: Pump Program: `6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P`
- #13: Global Volume: `Hq2wp8uJ9jCPsYgNHex8RtqdvMPfVGoYwjvF1ATiwn2Y`
- #15: Fee Config: `8Wf5TiAheLUqBrKXeYg2JtAFFMWtKdG2BSFgqUcPVwTt`
- #16: Fee Program: `pfeeUxB6jkeY1Hxd7CsFCAjcbHA9rWtchMGdZ6VojVZ`

**Location in code:** `src/pumpfun_instructions.rs` (constants at top of file)

---

### **Category 2: Victim-Specific Variables (5 accounts)** - EXTRACTED FROM VICTIM TX ✅
These accounts are VARIABLE and must be extracted from the victim transaction:

#### #2: Fee Recipient - ⚠️ **ROTATES!** ⚠️
- **Your Discovery:** This is NOT constant! It changes across transactions
- **Extraction:** `instruction.accounts[1]` from victim tx
- **Location:** `src/main.rs:995-997` in `extract_transaction_info()`
- **Usage:** Passed to all PumpFun instructions

#### #3: Mint
- **Extraction:** `instruction.accounts[2]` from victim tx
- **Location:** `src/main.rs:990-991` in `extract_transaction_info()`

#### #4: Bonding Curve
- **Derivation:** Calculated from mint using PDA seeds `["bonding-curve", mint]`
- **Location:** `src/pumpfun_instructions.rs:41-45` in `derive_bonding_curve()`
- **Note:** Although calculated, seeds come from victim's mint

#### #5: Associated Bonding Curve Token Account
- **Extraction:** `instruction.accounts[4]` from victim tx
- **Location:** `src/main.rs:1000-1002` in `extract_transaction_info()`
- **Critical:** This is the bonding curve's token account - must match victim!

#### #9: Token Program ID (Token vs Token-2022)
- **Extraction:** `instruction.accounts[8]` from victim tx
- **Location:** `src/main.rs:1005-1007` in `extract_transaction_info()`
- **Values:** Either `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA` or `TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb`
- **Critical Fix:** NO RPC CALLS! Extract directly from victim transaction

#### #10: Creator Vault
- **Extraction:** `instruction.accounts[9]` from victim tx
- **Location:** `src/main.rs:1010-1012` in `extract_transaction_info()`
- **Critical:** Must match the creator vault from victim transaction

---

### **Category 3: Bot Constants (2 accounts)** - HARDCODED ✅
Your bot's own addresses:

#### #6: User (Your Bot Wallet)
- **Source:** Front runner keypair loaded from environment
- **Location:** `src/wallet_manager.rs:8-12`
- **Env Var:** `FRONT_RUNNER_KEYPAIR_PATH`

#### #14: User Volume Accumulator
- **Derivation:** PDA with seeds `["user", your_wallet_pubkey]`
- **Location:** `src/pumpfun_instructions.rs:33-37` in `derive_user_volume_accumulator()`

---

### **Category 4: Bot Calculated (1 account)** - DYNAMICALLY CALCULATED ✅
#### #8: Associated User Token Account (Your Bot's ATA)
- **Calculation:** Uses correct Token Program based on #9
- **Location:** `src/transaction_builder.rs:61-68` in `build_front_run_transaction()`
- **Logic:**
  ```rust
  let user_token_account = match token_program_type {
      TokenProgramType::Token2022Program => {
          get_associated_token_address_2022(&buyer.pubkey(), mint)
      }
      TokenProgramType::TokenProgram => {
          get_associated_token_address(&buyer.pubkey(), mint)
      }
  };
  ```

---

## 🔄 Complete Data Flow

### Step 1: Receive Victim Transaction (Shreds)
**File:** `src/main.rs`
**Function:** `run_shreds_task()` (line 1121)
- Receives real-time Shreds from network
- Contains COMPLETE victim transaction before confirmation

### Step 2: Extract All Critical Accounts
**File:** `src/main.rs`
**Function:** `extract_transaction_info()` (line 975)
**Extracts:**
```rust
TransactionInfo {
    buyer: String,                                    // Account #6 from victim
    mint: String,                                     // Account #3 from victim
    bonding_curve: String,                            // Calculated from mint
    max_sol: u64,                                     // From instruction data
    token_amount: u64,                                // From instruction data
    priority_fee: u64,                                // Extracted from ComputeBudget instruction
    signature: String,                                // Used for frontrun_target

    // ✅ CRITICAL EXTRACTED ACCOUNTS:
    fee_recipient: Option<String>,                    // Account #2 - VARIABLE!
    bonding_curve_token_account: Option<String>,      // Account #5
    token_program_id: Option<String>,                 // Account #9
    creator_vault: Option<String>,                    // Account #10
}
```

### Step 3: Worker Thread Processing
**File:** `src/main.rs`
**Function:** `unified_worker_thread()` (line 531)

#### Filters Applied (in order):
1. **Pool exists?** (line 549) - Skip if no pool data from Geyser
2. **Minimum SOL?** (line 560) - Skip if < 0.05 SOL
3. **Same-block activity?** (line 568) - Skip bot recycling
4. **Target already confirmed?** (line 588) - Skip if too late
5. **Profitable simulation?** (line 609) - Only proceed if net profit > threshold

### Step 4: Detect Token Program Type
**File:** `src/main.rs` (line 696-713)
```rust
let token_program_id_str = match &tx_info.token_program_id {
    Some(tp) => tp,
    None => { error!("No token program ID!"); continue; }
};

let token_program_type = if token_program_id_str == "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb" {
    TokenProgramType::Token2022Program  // PumpFun uses this!
} else if token_program_id_str == "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA" {
    TokenProgramType::TokenProgram
} else {
    error!("Unknown token program!"); continue;
};
```

### Step 5: Parse Extracted Accounts
**File:** `src/main.rs` (line 632-759)
- Converts all extracted string addresses to `Pubkey` types
- Validates all required accounts are present
- Skips transaction if any critical account is missing

### Step 6: Build Front-Run Transaction
**File:** `src/transaction_builder.rs`
**Function:** `build_front_run_transaction()` (line 45)

**Instructions Created:**
1. Set Compute Unit Limit (200,000 CU)
2. Set Priority Fee (from victim or base)
3. Create ATA instruction (correct Token Program!)
4. Buy instruction (all 16 accounts)

**Critical Code for ATA:**
```rust
let create_ata_ix = match token_program_type {
    TokenProgramType::Token2022Program => {
        create_associated_token_account_2022(...)  // Uses Token-2022!
    }
    TokenProgramType::TokenProgram => {
        create_associated_token_account(...)
    }
};
```

### Step 7: Build Back-Run Transaction
**File:** `src/transaction_builder.rs`
**Function:** `build_back_run_transaction()` (line 153)

**Instructions Created:**
1. Set Compute Unit Limit (400,000 CU)
2. Set Priority Fee
3. Sell instruction (14 accounts for sell)
4. Close ATA instruction (recover rent)
5. Jito tip transfer

### Step 8: RPC Simulation (Optional)
**File:** `src/jito_client.rs`
**Function:** `simulate_transaction()` (line 137)

**Critical Fix Applied:**
```rust
let request = serde_json::json!({
    "jsonrpc": "2.0",
    "id": 1,
    "method": "simulateTransaction",
    "params": [
        encoded,
        {
            "encoding": "base58",
            "commitment": "processed",
            "replaceRecentBlockhash": true,  // ✅ CRITICAL FIX!
            "sigVerify": false,
        }
    ]
});
```

**Why This Matters:**
- `replaceRecentBlockhash: true` - Ignores blockhash age in simulation
- Prevents "Blockhash not found" errors
- Allows simulation even with old blockhash

### Step 9: Bundle Submission
**File:** `src/jito_client.rs`
**Function:** `send_bundle_with_victim()` (line 267)

**Bundle Structure:**
```
[
    Front-run Transaction (your buy),
    // Victim signature used as frontrun_target - Jito inserts victim tx here
    Back-run Transaction (your sell)
]
```

**Jito Parameters:**
```json
{
    "frontrun_target": "victim_signature_string"
}
```

---

## 🔧 Critical Files Breakdown

### `src/main.rs` (1400 lines)
**Purpose:** Main orchestration and worker logic
**Key Functions:**
- `extract_transaction_info()` - Extracts all accounts from victim tx
- `unified_worker_thread()` - Processes transactions and executes attacks
- `simulate_sandwich_attack()` - AMM math calculations
- `main()` - Setup and coordination

### `src/pumpfun_instructions.rs` (147 lines)
**Purpose:** Creates PumpFun buy/sell instructions
**Key Functions:**
- `derive_bonding_curve()` - Calculates bonding curve PDA
- `derive_user_volume_accumulator()` - Calculates user PDA
- `create_buy_instruction()` - Creates buy with 16 accounts
- `create_sell_instruction()` - Creates sell with 14 accounts

### `src/transaction_builder.rs` (300 lines)
**Purpose:** Builds complete transactions with all instructions
**Key Functions:**
- `build_front_run_transaction()` - Front-run with ATA creation
- `build_back_run_transaction()` - Back-run with ATA close + tip
- `get_recent_blockhash()` - Fetches blockhash from RPC

### `src/jito_client.rs` (575 lines)
**Purpose:** Jito bundle submission and RPC simulation
**Key Functions:**
- `simulate_transaction()` - RPC simulation with `replaceRecentBlockhash`
- `send_bundle_with_victim()` - Sends bundle to Jito
- `check_target_transaction_status()` - Checks if victim already confirmed

### `src/spl_utils.rs` (117 lines)
**Purpose:** SPL Token utility functions with Token-2022 support
**Key Functions:**
- `get_associated_token_address()` - Standard Token Program ATA
- `get_associated_token_address_2022()` - Token-2022 ATA
- `create_associated_token_account()` - Standard Token Program
- `create_associated_token_account_2022()` - Token-2022
- `close_account()` - Standard Token Program
- `close_account_2022()` - Token-2022

### `src/wallet_manager.rs` (45 lines)
**Purpose:** Loads and manages keypairs
**Key Struct:**
```rust
pub struct WalletManager {
    pub front_runner: Keypair,
    pub token_receiver: Pubkey,
    pub tip_payer: Keypair,
}
```

### `src/config.rs` (187 lines)
**Purpose:** Parses Geyser filter configuration from config.json
**Key Struct:**
```rust
pub struct Config {
    pub commitment: Option<String>,
    pub transactions: HashMap<String, TransactionFilter>,
    pub accounts: HashMap<String, AccountFilter>,
    // ... other filters
}
```

---

## ✅ All Fixes Applied

### Fix #1: Fee Recipient is NOT Constant
- **Your Discovery:** Fee Recipient rotates across transactions
- **Fix:** Extract from `instruction.accounts[1]` in victim tx
- **Location:** `src/main.rs:995-997` and passed throughout

### Fix #2: Bonding Curve Token Account Extracted
- **Issue:** Was being calculated, should be extracted
- **Fix:** Extract from `instruction.accounts[4]` in victim tx
- **Location:** `src/main.rs:1000-1002` and passed throughout

### Fix #3: Token Program ID Extracted (No RPC!)
- **Issue:** Was using RPC to detect, causing 429 errors
- **Fix:** Extract from `instruction.accounts[8]` in victim tx
- **Location:** `src/main.rs:1005-1007` and `src/main.rs:696-713`

### Fix #4: Creator Vault Extracted
- **Fix:** Extract from `instruction.accounts[9]` in victim tx
- **Location:** `src/main.rs:1010-1012`

### Fix #5: ReplaceRecentBlockhash in Simulation
- **Issue:** Blockhash validation was failing
- **Fix:** Added `replaceRecentBlockhash: true` parameter
- **Location:** `src/jito_client.rs:157`

### Fix #6: Check Target Transaction Status
- **Purpose:** Skip if victim already confirmed (too late!)
- **Location:** `src/main.rs:588-603`

---

## 🚀 How to Build and Run

### Build from Scratch
```bash
# Clean all previous builds
cargo clean

# Build optimized release version
cargo build --release

# This will take 2-3 minutes for fresh build
```

### Run with Debug Logging
```bash
# Set log level to debug for full output
RUST_LOG=debug cargo run --release
```

### Expected Log Output
```
═══════════════════════════════════════════════════════════
MEV Bot v17.0 - EXTRACT ALL ACCOUNTS FROM VICTIM TX 🚀
Workers: 6
RPC Simulation: ENABLED (with replaceRecentBlockhash)
═══════════════════════════════════════════════════════════
🎯 CRITICAL FIXES:
   ✅ Fee Recipient extracted from victim tx (NOT hardcoded!)
   ✅ Bonding Curve Token Account from victim tx (NOT calculated!)
   ✅ Token Program ID from victim tx (NO RPC detection!)
   ✅ Creator vault from victim tx (NO RPC calls!)
   ✅ replaceRecentBlockhash=true in simulation
   ✅ Check target tx status before sending bundle
   ✅ Full debugging logs
═══════════════════════════════════════════════════════════
```

---

## 📊 What Success Looks Like

### When Front-Run Simulation Succeeds:
```
🔬 [W0] Simulating transactions with RPC (replaceRecentBlockhash=true)...
✅ [W0] Front-run simulation SUCCESS
   CU consumed: 145234
✅ [W0] Back-run simulation SUCCESS
   CU consumed: 198567
🎉 [W0] Both simulations PASSED!
```

### When Bundle is Sent:
```
📦 [W0] Sending bundle to Jito...
   🎯 Victim: 5KJp7z...
✅ [W0] Bundle sent!
   Bundle ID: 1a2b3c4d...
   Tip: 0.001000 SOL
   Expected profit: 0.003500 SOL
```

---

## 🐛 Common Issues & Solutions

### Issue: "No token program ID in victim tx!"
**Cause:** Victim transaction doesn't have account #9
**Solution:** This is normal for non-PumpFun transactions - skip is correct

### Issue: "No fee recipient in victim tx!"
**Cause:** Victim transaction doesn't have account #2
**Solution:** This is normal for non-PumpFun transactions - skip is correct

### Issue: "Front-run simulation FAILED" with Error 2006
**Cause:** Wrong accounts being passed
**Solution:** Verify all extracted accounts are being passed correctly

### Issue: "Front-run simulation FAILED" with IncorrectProgramId
**Cause:** Token Program mismatch
**Solution:** Verify `token_program_type` is detected correctly from account #9

---

## 📝 Summary

**What is Hardcoded:**
- System constants (8 accounts from Category 1)
- Bot wallet addresses (2 accounts from Category 3)

**What is Extracted from Victim Transaction:**
- Fee Recipient (#2) - ⚠️ ROTATES!
- Mint (#3)
- Associated Bonding Curve Token Account (#5)
- Token Program ID (#9)
- Creator Vault (#10)

**What is Calculated:**
- Bonding Curve PDA (from mint)
- User Volume Accumulator PDA (from bot wallet)
- Associated User Token Account (from bot wallet + mint + token program type)

**Critical Insight:**
Everything variable MUST come from the victim transaction. No RPC calls for detection!
The 4-Category Model ensures we use exactly the same accounts as the victim.
