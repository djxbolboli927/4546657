# MEV Bot v16.0 - Token2022 Fix

## ✅ مشکل حل شد: `IncorrectProgramId` Error

### 🔍 تشخیص مشکل

خطای `InstructionError(2, "IncorrectProgramId")` به این دلیل رخ می‌داد که:

1. **PumpFun به Token-2022 مهاجرت کرده است** یا از هر دو Token Program و Token2022 پشتیبانی می‌کند
2. کد قدیمی **همیشه فرض می‌کرد** که Token Program قدیمی استفاده می‌شود
3. در instruction `CreateIdempotent` برای ATA، program ID اشتباه ارسال می‌شد

### ✅ راه حل

**استخراج مستقیم Token Program ID از تراکنش قربانی!**

بجای محاسبه یا حدس زدن، حالا کد:
- Token Program ID را از **Account #8** تراکنش قربانی استخراج می‌کند
- User Token Account را از **Account #5** تراکنش قربانی استخراج می‌کند
- Creator Vault را از **Account #9** تراکنش قربانی استخراج می‌کند

این روش تضمین می‌کند که **دقیقاً همان account هایی که در تراکنش قربانی موفق بوده استفاده شود**.

### 🚀 تغییرات اعمال شده

#### 1. `spl_utils.rs` - پشتیبانی از Token2022
```rust
// ✅ هر دو Token Program پشتیبانی می‌شود
pub const TOKEN_PROGRAM_ID: Pubkey = ...;
pub const TOKEN_2022_PROGRAM_ID: Pubkey = ...;

// ✅ توابع جدید با پارامتر token_program_id
pub fn create_associated_token_account_with_program_id(...)
pub fn close_account_with_program_id(...)
```

#### 2. `pumpfun_instructions.rs` - استفاده از token_program_id
```rust
// ✅ پارامتر token_program_id اضافه شد
pub fn create_buy_instruction(
    ...
    token_program_id: &Pubkey,  // NEW!
    ...
)
```

#### 3. `transaction_builder.rs` - انتقال token_program_id
```rust
// ✅ همه توابع token_program_id را می‌پذیرند
pub async fn build_front_run_transaction(
    ...
    token_program_id: &Pubkey,  // NEW!
    ...
)
```

#### 4. `main.rs` - استخراج از تراکنش قربانی
```rust
// ✅ فیلدهای جدید در TransactionInfo
struct TransactionInfo {
    ...
    token_program_id: String,      // Account #8
    user_token_account: String,    // Account #5
    creator_vault: Option<String>, // Account #9
}

// ✅ استخراج در extract_transaction_info
let token_program_id = instruction.accounts.get(8)
    .and_then(|&idx| account_keys.get(idx as usize))
    .map(|pk| pk.to_string());
```

### 📦 نصب و اجرا

1. **کلون پروژه:**
```bash
git clone <repo-url>
cd mev_bot_unified
```

2. **تنظیمات:**
```bash
cp .env.example .env
# ویرایش .env و افزودن کلیدهای API و مسیرهای wallet
```

3. **کامپایل:**
```bash
cargo build --release
```

4. **اجرا:**
```bash
cargo run --release
```

### 🎯 ویژگی‌ها

- ✅ **پشتیبانی کامل از Token2022**: هر دو Token Program و Token2022
- ✅ **استخراج خودکار**: همه account ها از تراکنش قربانی استخراج می‌شوند
- ✅ **بدون RPC اضافی**: نیازی به fetch کردن اطلاعات on-chain نیست
- ✅ **سرعت بالا**: صفر تاخیر برای تشخیص Token Program
- ✅ **دقت 100%**: استفاده از همان account های موفق تراکنش قربانی

### 🔧 تنظیمات

در فایل `.env`:
```bash
# RPC Endpoint برای شبیه‌سازی
SOLANA_RPC_ENDPOINT="https://edge.erpc.global?api-key=YOUR_KEY"

# Wallet Paths
FRONT_RUNNER_KEYPAIR_PATH="/path/to/wallet_front_runner.json"
TOKEN_RECEIVER_KEYPAIR_PATH="/path/to/wallet_token_receiver.json"
TIP_PAYER_KEYPAIR_PATH="/path/to/wallet_tip_payer.json"
```

### 📊 لاگ‌های بهبود یافته

```
✅ [W0] Using Token Program from victim tx: TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb
   Creator Vault: ...
   Mint: ...
   Bonding Curve: ...
```

### 🐛 عیب‌یابی

اگر هنوز خطا می‌بینید:

1. **چک کنید wallet ها موجود باشند:**
```bash
ls -la /path/to/wallets/
```

2. **چک کنید RPC endpoint کار می‌کند:**
```bash
curl -X POST "$SOLANA_RPC_ENDPOINT" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"getHealth"}'
```

3. **لاگ‌ها را بررسی کنید:**
```bash
RUST_LOG=debug cargo run --release
```

### 📝 نکات مهم

- کد با **Token Program قدیمی** و **Token2022** کار می‌کند
- استخراج خودکار از تراکنش قربانی = **دقت کامل**
- شبیه‌سازی RPC فعال است - می‌توانید با `ENABLE_RPC_SIMULATION = false` غیرفعال کنید

### 🚀 نسخه

- **v16.0**: Token2022 Fix - استخراج Token Program ID از تراکنش قربانی

---

**هشدار**: این نرم‌افزار صرفاً برای اهداف آموزشی و تحقیقاتی است. استفاده در محیط‌های production به مسئولیت خود شماست.
