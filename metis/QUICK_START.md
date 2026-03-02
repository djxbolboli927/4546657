# ⚡ راهنمای سریع Metis - آربیتراژ سیرکولار

## 🎯 دستورات ضروری

### 1️⃣ راه‌اندازی اولیه

```bash
# رفتن به دایرکتوری Metis
cd /home/user/4546657/metis

# ویرایش فایل env و وارد کردن Binary Key
nano metis.env
# خط BINARY_KEY=YOUR_BINARY_KEY_HERE را با key واقعی جایگزین کنید

# اجرای Metis
./start-metis.sh
```

---

### 2️⃣ تست اتصال (در ترمینال دیگر)

```bash
# Health Check
curl http://localhost:8080/health

# تست Quote ساده (1 SOL به USDC)
curl "http://localhost:8080/quote?\
inputMint=So11111111111111111111111111111111111111112&\
outputMint=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v&\
amount=1000000000&\
slippageBps=50"
```

---

### 3️⃣ تست آربیتراژ سیرکولار

```bash
# SOL → ... → SOL (آربیتراژ)
curl "http://localhost:8080/quote?\
inputMint=So11111111111111111111111111111111111111112&\
outputMint=So11111111111111111111111111111111111111112&\
amount=1000000000&\
slippageBps=0&\
onlyDirectRoutes=false" | jq .
```

اگر سودآور باشد، `outAmount > inAmount` خواهد بود.

---

### 4️⃣ Quote برای Jito Bundle

```bash
curl "http://localhost:8080/quote?\
inputMint=So11111111111111111111111111111111111111112&\
outputMint=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v&\
amount=1000000000&\
slippageBps=0&\
forJitoBundle=true" | jq .
```

پارامتر `forJitoBundle=true` استخرهای ناسازگار را حذف می‌کند.

---

### 5️⃣ دریافت Swap Instructions (برای ساخت تراکنش دستی)

```bash
curl -X POST http://localhost:8080/swap-instructions \
  -H "Content-Type: application/json" \
  -d '{
    "userPublicKey": "YOUR_WALLET_ADDRESS",
    "quoteResponse": { ... },
    "dynamicComputeUnitLimit": true,
    "skipUserAccountsRpcCalls": true
  }' | jq .
```

این دستورات خام را برمی‌گرداند که می‌توانید Jito tip را به آن اضافه کنید.

---

## 📋 چک‌لیست قبل از استفاده در Production

- [ ] Binary Key معتبر دریافت شده (10,000 JUP staked)
- [ ] RPC endpoint با quality بالا (پینگ < 50ms)
- [ ] Market cache به‌روز است (کمتر از 30 دقیقه قبل)
- [ ] فیلترها تنظیم شده‌اند (FILTER_MARKETS_WITH_MINTS)
- [ ] تست در Devnet انجام شده
- [ ] Wallet با SOL و توکن‌های لازم شارژ شده
- [ ] Jito client آماده است
- [ ] منطق محاسبه Tip پیاده‌سازی شده
- [ ] سیستم مانیتورینگ و logging فعال است

---

## 🔧 دستورات نگهداری

### به‌روزرسانی Market Cache

```bash
cd /home/user/4546657/metis
curl -o market-cache.json "https://cache.jup.ag/markets?v=4"
# سپس Metis را restart کنید
```

### بررسی لاگ‌ها

```bash
tail -f /home/user/4546657/metis/metis.log
```

### نمایش تعداد استخرهای فیلتر شده

```bash
# با فیلتر SOL, USDC, USDT
cat market-cache.json | jq '[.[] | select(
  (.inputMint == "So11111111111111111111111111111111111111112" or .outputMint == "So11111111111111111111111111111111111111112") or
  (.inputMint == "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" or .outputMint == "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v") or
  (.inputMint == "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB" or .outputMint == "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB")
)] | length'
```

---

## 🎨 مثال‌های آربیتراژ

### مسیر 2-hop: SOL → USDC → SOL

```json
{
  "inputMint": "So11111111111111111111111111111111111111112",
  "outputMint": "So11111111111111111111111111111111111111112",
  "amount": "1000000000",
  "slippageBps": 0,
  "routePlan": [
    { "swapInfo": { "inputMint": "SOL", "outputMint": "USDC" } },
    { "swapInfo": { "inputMint": "USDC", "outputMint": "SOL" } }
  ]
}
```

### مسیر 3-hop: SOL → USDC → USDT → SOL

```json
{
  "routePlan": [
    { "swapInfo": { "inputMint": "SOL", "outputMint": "USDC" } },
    { "swapInfo": { "inputMint": "USDC", "outputMint": "USDT" } },
    { "swapInfo": { "inputMint": "USDT", "outputMint": "SOL" } }
  ]
}
```

---

## 💡 نکات مهم

### Slippage در آربیتراژ

برای آربیتراژ همیشه `slippageBps=0` تنظیم کنید:
- اگر قیمت تغییر کند → تراکنش fail می‌شود
- با Jito bundle → تراکنش اصلاً روی chain نمی‌آید
- شما کارمزد نمی‌پردازید برای تراکنش‌های failed

### محاسبه Tip Jito

```javascript
// محاسبه سود خالص
const profit = quotedOutAmount - inputAmount;

// Tip = 5% تا 50% سود (بسته به رقابت)
const tipPercent = 0.05; // شروع از 5%
let tip = Math.floor(profit * tipPercent);

// حداقل و حداکثر
tip = Math.max(tip, 1000);      // حداقل 1000 lamports
tip = Math.min(tip, 50_000_000); // حداکثر 0.05 SOL

// فقط بفرست اگر سود بیشتر از tip است
if (profit - tip > minProfit) {
  sendBundle(transaction, tip);
}
```

### انتخاب Jito Tip Account

```javascript
const JITO_TIP_ACCOUNTS = [
  "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
  "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
  "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
  "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
  "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
  "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
  "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
  "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT"
];

// انتخاب تصادفی برای جلوگیری از contention
const randomTipAccount = JITO_TIP_ACCOUNTS[
  Math.floor(Math.random() * JITO_TIP_ACCOUNTS.length)
];
```

---

## 🚨 عیب‌یابی سریع

### Metis اجرا نمی‌شود

```bash
# چک کنید Binary Key درست است
grep BINARY_KEY metis.env

# چک کنید RPC در دسترس است
curl "$RPC_URL"

# لاگ را بررسی کنید
tail -20 metis.log
```

### Quote خطا می‌دهد

```bash
# چک کنید Metis در حال اجراست
curl http://localhost:8080/health

# بررسی صحت آدرس توکن‌ها
# SOL: So11111111111111111111111111111111111111112
# USDC: EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v
# USDT: Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB
```

### آربیتراژ سودآور پیدا نمی‌شود

این طبیعی است! بازار بسیار رقابتی است:
- بیشتر وقت‌ها فرصتی وجود ندارد
- فرصت‌ها خیلی سریع از بین می‌روند (< 400ms)
- رباتهای دیگر هم دارند جستجو می‌کنند

راه‌حل:
1. سرعت بیشتر (co-location با validators)
2. استراتژی‌های پیشرفته‌تر (multi-hop، کم‌عمق‌ها)
3. رصد رویدادها (new pool creation، big swaps)

---

## 📞 پشتیبانی

- راهنمای کامل: `README_METIS.md`
- مستندات Metis: https://metis.builders/docs
- مستندات Jupiter: https://station.jup.ag/docs/apis/swap-api
- مستندات Jito: https://docs.jito.wtf/

---

**✅ آماده برای معامله!**

وقتی Binary Key را دریافت کردید، فقط کافیست `./start-metis.sh` را اجرا کنید و ربات خود را به Metis متصل کنید.
