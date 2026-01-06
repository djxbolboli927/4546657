# 🌍 MEV Bot with Geographic-Aware Leader Detection

ربات MEV برای شبکه سولانا با قابلیت تشخیص هوشمند موقعیت جغرافیایی Block Leader

## 📋 فهرست مطالب

- [معرفی](#معرفی)
- [ویژگی‌های کلیدی](#ویژگی‌های-کلیدی)
- [معماری سیستم](#معماری-سیستم)
- [نصب و راه‌اندازی](#نصب-و-راه‌اندازی)
- [پیکربندی](#پیکربندی)
- [نحوه کار Leader Oracle](#نحوه-کار-leader-oracle)

---

## معرفی

این پروژه یک ربات MEV (Maximal Extractable Value) پیشرفته برای شبکه Solana است که با استفاده از **ERPC Leader Slot Information API** قادر به تشخیص موقعیت جغرافیایی Block Leader بعدی است و تنها زمانی معامله می‌کند که لیدر در اروپا قرار داشته باشد.

### چرا این مهم است؟

در استراتژی‌های MEV مانند Sandwich Attack، **تأخیر شبکه (Latency)** عامل حیاتی موفقیت است:

- اگر سرور شما در **فرانکفورت** باشد و Block Leader در **توکیو**، RTT شبکه بیش از **200-250ms** است
- زمان هر Slot در Solana فقط **400ms** است
- این ربات به‌صورت **پویا** موقعیت لیدر را بررسی کرده و تنها در اسلات‌های با لیدر اروپایی معامله می‌کند

---

## ویژگی‌های کلیدی

### 🌍 Leader Oracle Module

- **تشخیص خودکار** موقعیت جغرافیایی Block Leader بعدی
- **فیلترینگ پیشرفته** بر اساس کد کشور، منطقه و Ping
- **کش هوشمند** با پیش‌بارگذاری 100 اسلات آینده
- **به‌روزرسانی خودکار** در پس‌زمینه

### 🎯 Dynamic Jito Endpoint Selection

ربات به‌صورت خودکار بهترین Jito Block Engine را انتخاب می‌کند:

- لیدر در **آلمان (DE)** → `frankfurt.mainnet.block-engine.jito.wtf`
- لیدر در **هلند (NL)** → `amsterdam.mainnet.block-engine.jito.wtf`
- لیدر در **آمریکا (US)** → `ny.mainnet.block-engine.jito.wtf`
- لیدر در **ژاپن (JP)** → `tokyo.mainnet.block-engine.jito.wtf`

---

## معماری سیستم

```
ERPC Leader API → Leader Oracle → Main Loop → Worker Pool
                                      ↓
                                  Workers (6x)
                                      ↓
                         Check Leader (Geographic Filter)
                                      ↓
                              Simulate Profit
                                      ↓
                              Build Bundle
                                      ↓
                    Send to Optimal Jito Endpoint
```

---

## نصب و راه‌اندازی

```bash
# 1. Build project
cargo build --release

# 2. Configure .env
cp .env.example .env
nano .env

# 3. Run bot
cargo run --release
```

---

## پیکربندی

### فایل `.env` - تنظیمات Leader Oracle

```env
# ERPC Leader API
ERPC_LEADER_API_ENDPOINT="https://edge.erpc.global"
ERPC_API_KEY="YOUR_API_KEY_HERE"

# Geographic Filtering
ALLOWED_COUNTRIES="DE,NL,FR,GB,CH,BE,PL,SE,FI"
ALLOWED_REGIONS="Europe,EU"
MAX_LATENCY_MS="30"

# Wallets
FRONT_RUNNER_KEYPAIR_PATH="/path/to/wallet_front_runner.json"
TOKEN_RECEIVER_KEYPAIR_PATH="/path/to/wallet_token_receiver.json"
TIP_PAYER_KEYPAIR_PATH="/path/to/wallet_tip_payer.json"
```

---

## نحوه کار Leader Oracle

### الگوریتم تصمیم‌گیری

```rust
async fn can_trade(slot: u64) -> bool {
    // 1️⃣ بررسی Ping (بالاترین اولویت)
    if ping <= 30ms {
        return true;  // ✅ Latency عالی
    }

    // 2️⃣ بررسی کد کشور
    if country in ["DE", "NL", "FR", ...] {
        return true;  // ✅ کشور مجاز
    }

    // 3️⃣ بررسی منطقه
    if region.contains("Europe") {
        return true;  // ✅ منطقه مجاز
    }

    return false;  // ⛔ Block trade
}
```

---

## ساختار فایل‌ها

```
src/
├── main.rs                    # نقطه ورود + Worker Pool
├── leader_oracle.rs           # 🌍 ماژول تشخیص لیدر
├── jito_client.rs             # ارتباط با Jito
├── transaction_builder.rs     # ساخت تراکنش‌ها
├── pumpfun_instructions.rs    # Instructions Pump.fun
└── ...
```

---

## لاگ‌های مهم

```
🌍 Leader Oracle initializing...
✅ Leader schedule updated: 100 slots cached
✅ Slot 245830192: TRADE ALLOWED (Country: DE)
⛔ Slot 245830193: TRADE BLOCKED (Outside Europe)
📊 Cache Stats: 98 total slots, 67 in Europe (68.4%)
```

---

## آمار عملکرد

بر اساس تحلیل‌ها:
- **68% از اسلات‌ها** دارای لیدر اروپایی هستند
- با این روش، شما در **2/3 زمان** فعال هستید اما با **بهترین کیفیت شبکه**

---

## API Reference

### ERPC Leader Slot Information API

```json
POST https://edge.erpc.global?api-key=YOUR_KEY

{
  "jsonrpc": "2.0",
  "method": "getLeaderSlots",
  "params": [startSlot, count]
}
```

Response شامل:
- `slot`: شماره اسلات
- `country`: کد کشور (DE, JP, US, ...)
- `region`: نام منطقه
- `ping`: تأخیر از فرانکفورت (ms)

---

## ⚠️ هشدار

استفاده از این ربات در شبکه اصلی به مسئولیت خودتان است. همیشه ابتدا در Devnet تست کنید.

---

**🌍 Built with ❤️ for Low-Latency MEV Trading**
