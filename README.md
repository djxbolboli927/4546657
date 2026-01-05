# Sandwich Bot - Bundle Simulation Mode

🤖 **ربات ساندویچ اتک برای Solana (Pump.fun)**

## 📋 تغییرات اعمال شده

### ✅ ویژگی‌های جدید

1. **ساخت باندل کامل (Bundle)**
   - Front-run transaction (تراکنش پیش‌خرید)
   - Victim transaction (تراکنش قربانی - از شبکه)
   - Back-run transaction (تراکنش فروش)

2. **شبیه‌سازی باندل با Jito**
   - ارسال باندل به Jito Block Engine برای شبیه‌سازی
   - دریافت نتایج دقیق برای هر تراکنش در باندل
   - لاگ کامل از خطاها و موفقیت‌ها

3. **Rate Limiting (محدودیت نرخ)**
   - محدودیت ۱ درخواست در ثانیه (دقیقاً 1 req/sec)
   - استفاده از `tokio::sync::Semaphore` با `try_acquire()` (non-blocking)
   - **هیچ انتظاری نمی‌کشد**: اگر rate limit فعال باشد، تراکنش skip می‌شود
   - اطمینان از ارسال فوری تراکنش‌های سودده

4. **فیلتر سودآوری**
   - فقط تراکنش‌های سودده به Jito ارسال می‌شوند
   - محاسبه دقیق سود خالص
   - آمار کامل از profitable/unprofitable

## 🔧 تغییرات تکنیکال

### `src/main.rs`
- اضافه شدن فیلد `raw_transaction: Vec<u8>` به `TransactionInfo`
- ذخیره تراکنش کامل victim در `extract_transaction_info()`
- پیاده‌سازی rate limiter با `Semaphore`
- آپدیت `unified_worker_thread()` برای ساخت باندل کامل
- ارسال فقط باندل‌های profitable به Jito

### `src/jito_client.rs`
- تابع `simulate_bundle()` برای شبیه‌سازی باندل
- ساختار کامل پاسخ شبیه‌سازی
- هندلینگ خطاهای دقیق‌تر

### سایر فایل‌ها
- همه فایل‌های ماژول‌ها (config, wallet_manager, transaction_builder, و غیره)

## 📊 نحوه کار

1. **دریافت تراکنش victim از شبکه**
   - از طریق ShredStream
   - استخراج اطلاعات کامل
   - ذخیره تراکنش serialized

2. **شبیه‌سازی محلی**
   - محاسبه سودآوری
   - فیلتر کردن unprofitable

3. **ساخت باندل**
   - Front-run: خرید با ۷۰٪ slippage buffer
   - Victim: تراکنش اصلی
   - Back-run: فروش + close account + Jito tip

4. **ارسال برای شبیه‌سازی**
   - محدودیت ۱ req/sec (non-blocking)
   - اگر rate limit فعال باشد، تراکنش skip می‌شود (بدون انتظار)
   - فقط profitable bundles
   - دریافت نتایج کامل

## 🚀 اجرا

```bash
# نصب وابستگی‌ها
cargo build --release

# اجرا
RUST_LOG=info cargo run --release
```

## ⚙️ تنظیمات

در فایل `.env`:
- `SOLANA_RPC_ENDPOINT`: RPC endpoint برای شبیه‌سازی
- `SHRED_ENDPOINT`: ShredStream endpoint
- `GRPC_ENDPOINT`: Geyser GRPC endpoint
- Wallet paths

## 📈 خروجی نمونه

```
╔═══════════════════════════════════════════════════════════╗
║ ✅ PROFITABLE SANDWICH [Worker 1]
╠═══════════════════════════════════════════════════════════╣
║ VICTIM:
║   Signature: ...abc12345
║   Token Amount: 1000000
║   Max SOL: 0.500000
║   Priority Fee: 50000 μLamp
╠═══════════════════════════════════════════════════════════╣
║ ATTACK:
║   1. Front-run: 0.300000 SOL → 500000 tokens
║   2. Victim pays: 0.450000 SOL (limit: 0.500000)
║   3. Back-run: 500000 tokens → 0.320000 SOL
╠═══════════════════════════════════════════════════════════╣
║ PROFIT:
║   Gross: 0.020000 SOL
║   Fees: 0.005000 SOL
║   Net: 0.015000 SOL
║   ROI: 5.00%
╚═══════════════════════════════════════════════════════════╝

📦 Sending bundle to Jito for simulation...
✅ BUNDLE SIMULATION SUCCESS!
   📊 Summary: {...}
   ✅ TX 0 SUCCESS (⛽ Units: 25000)
   ✅ TX 1 SUCCESS (⛽ Units: 50000)
   ✅ TX 2 SUCCESS (⛽ Units: 40000)
```

## ⚠️ نکات مهم

1. **فقط شبیه‌سازی**: این کد فقط باندل‌ها را شبیه‌سازی می‌کند، اجرا نمی‌کند
2. **Rate Limit**: دقیقاً ۱ req/sec - بدون انتظار (non-blocking)
3. **Profitable Only**: فقط تراکنش‌های سودده ارسال می‌شوند
4. **No Queueing**: تراکنش‌ها در صف انتظار قرار نمی‌گیرند - یا فوراً ارسال یا skip

## 🐛 رفع خطاهای رایج

### خطای Deserialization
```
invalid value: continue signal on byte-three
```

**دلیل**:
- ما `VersionedTransaction` را serialize می‌کردیم
- اما سعی داشتیم به `Transaction` (legacy) deserialize کنیم
- فرمت‌های متفاوت باعث خطا می‌شدند
- **نتیجه**: victim transaction به bundle اضافه نمی‌شد

**راه‌حل**: ✅ اصلاح شد
```rust
// قبل (اشتباه)
bincode::deserialize::<Transaction>(&raw_tx)

// بعد (درست)
let versioned = bincode::deserialize::<VersionedTransaction>(&raw_tx)?;
let legacy = versioned.into_legacy_transaction()?;
```

### نحوه عملکرد Rate Limiting
- از `try_acquire()` استفاده می‌کند (non-blocking)
- اگر semaphore available نباشد → تراکنش skip می‌شود
- **هیچ انتظاری نمی‌کشد** - برای جلوگیری از missed opportunities

## 🔄 مراحل بعدی (اختیاری)

برای اجرای واقعی:
1. تغییر `simulate_bundle()` به `send_bundle_with_victim()`
2. اضافه کردن تایید کاربر
3. افزایش امنیت و error handling
4. مانیتورینگ دقیق‌تر

## 📝 License

This is for educational purposes only.
