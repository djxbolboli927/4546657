# 🚀 راهنمای نصب و راه‌اندازی Metis برای آربیتراژ سیرکولار

این راهنما شامل تمام مراحل نصب، پیکربندی و اجرای موتور مسیریابی Metis برای استراتژی‌های آربیتراژ سیرکولار در شبکه Solana است.

---

## 📑 فهرست مطالب

- [معرفی Metis](#معرفی-metis)
- [الزامات سیستم](#الزامات-سیستم)
- [مرحله ۱: دریافت Binary Key](#مرحله-۱-دریافت-binary-key)
- [مرحله ۲: پیکربندی](#مرحله-۲-پیکربندی)
- [مرحله ۳: راه‌اندازی Metis](#مرحله-۳-راهاندازی-metis)
- [آربیتراژ سیرکولار](#آربیتراژ-سیرکولار)
- [یکپارچگی با Jito](#یکپارچگی-با-jito)
- [بهینه‌سازی و فیلترها](#بهینهسازی-و-فیلترها)
- [عیب‌یابی](#عیبیابی)

---

## معرفی Metis

**Metis** موتور مسیریابی Jupiter است که به صورت محلی (self-hosted) روی سرور شما اجرا می‌شود و امکانات زیر را فراهم می‌کند:

### مزایای استفاده از Metis محلی:

✅ **تاخیر فوق‌پایین (Ultra-low latency)**: زمان پاسخ زیر 1 میلی‌ثانیه
✅ **کنترل کامل**: دسترسی به تمام پارامترها و فیلترها
✅ **آربیتراژ سیرکولار**: امکان معاملات SOL → USDC → SOL
✅ **کش محلی**: 188,382 استخر نقدینگی در حافظه سرور
✅ **فیلترینگ پیشرفته**: انتخاب دقیق DEX ها و توکن‌های مورد نظر

---

## الزامات سیستم

### نرم‌افزار:
- Ubuntu 20.04+ یا هر Linux مدرن
- حداقل 8GB RAM (توصیه: 16GB)
- حداقل 2GB فضای دیسک
- `curl`, `jq` نصب شده

### الزامات شبکه:
- RPC endpoint (ERPC, Helius, Triton One و غیره)
- اتصال اینترنت پایدار با پینگ پایین

### الزامات مالی:
- **حداقل 10,000 توکن JUP** برای stake کردن و دریافت Binary Key

---

## مرحله ۱: دریافت Binary Key

Metis نسخه 7 نیازمند یک Binary Key معتبر است که از طریق stake کردن JUP به دست می‌آید.

### گام‌های دریافت Binary Key:

#### ۱. Stake کردن JUP

1. به https://vote.jup.ag/ بروید
2. کیف پول خود را متصل کنید
3. **حداقل 10,000 JUP** را در یک escrow account واحد stake کنید

⚠️ **مهم**: تمام JUP را در یک حساب stake کنید، نه چند حساب جداگانه.

#### ۲. درخواست Binary Key

1. به https://portal.metis.builders/ بروید
2. فرم درخواست را پر کنید
3. یک پیام one-time با کیف پول خود امضا کنید (بدون تراکنش on-chain)
4. Binary Key خود را از طریق ایمیل دریافت کنید

#### ۳. ذخیره امن Key

Binary Key را در فایل `metis.env` قرار دهید:

```bash
BINARY_KEY=your_actual_binary_key_here
```

⚠️ **امنیت**: هرگز Binary Key را در GitHub قرار ندهید یا عمومی نکنید!

---

## مرحله ۲: پیکربندی

### فایل‌های موجود:

```
metis/
├── metis-binary              # فایل اجرایی Metis (v7.0.7)
├── metis.env                 # تنظیمات محیطی
├── market-cache.json         # کش 188,382 استخر (56MB)
├── start-metis.sh           # اسکریپت راه‌اندازی
└── README_METIS.md          # این فایل
```

### بررسی تنظیمات `metis.env`:

```bash
# Binary Key (از portal.metis.builders)
BINARY_KEY=YOUR_KEY_HERE

# RPC Endpoint
RPC_URL=https://edge.erpc.global?api-key=YOUR_API_KEY

# آربیتراژ سیرکولار
ALLOW_CIRCULAR_ARBITRAGE=true

# فیلتر توکن‌ها (SOL, USDC, USDT)
FILTER_MARKETS_WITH_MINTS=So11111111111111111111111111111111111111112,EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v,Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB

# تنظیمات کش
MARKET_MODE=file
MARKET_CACHE=/home/user/4546657/metis/market-cache.json
```

---

## مرحله ۳: راه‌اندازی Metis

### روش ۱: استفاده از اسکریپت خودکار (توصیه می‌شود)

```bash
cd /home/user/4546657/metis
./start-metis.sh
```

اسکریپت به صورت خودکار:
- Binary Key را بررسی می‌کند
- Market cache را به‌روزرسانی می‌کند (در صورت نیاز)
- Metis را با تنظیمات بهینه اجرا می‌کند
- لاگ‌ها را در فایل `metis.log` ذخیره می‌کند

### روش ۲: اجرای دستی

```bash
cd /home/user/4546657/metis

./metis-binary \
    --env-file metis.env
```

### تست اتصال:

پس از اجرای Metis، در یک ترمینال جدید:

```bash
# تست health check
curl http://localhost:8080/health

# دریافت قیمت برای یک معامله آربیتراژ
curl "http://localhost:8080/quote?inputMint=So11111111111111111111111111111111111111112&outputMint=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v&amount=1000000000&slippageBps=50"
```

---

## آربیتراژ سیرکولار

### مفهوم:

آربیتراژ سیرکولار یعنی شروع و پایان معامله با یک توکن یکسان:

```
SOL → USDC → USDT → SOL
```

### فعال‌سازی:

```bash
ALLOW_CIRCULAR_ARBITRAGE=true
```

### مثال API Call:

```bash
curl "http://localhost:8080/quote?\
inputMint=So11111111111111111111111111111111111111112&\
outputMint=So11111111111111111111111111111111111111112&\
amount=1000000000&\
slippageBps=0&\
onlyDirectRoutes=false"
```

این درخواست بهترین مسیر برای تبدیل 1 SOL → ... → SOL را پیدا می‌کند.

---

## یکپارچگی با Jito

برای استفاده از Metis در کنار Jito bundles و جلوگیری از MEV:

### ۱. استفاده از پارامتر `forJitoBundle`

```bash
curl "http://localhost:8080/quote?\
inputMint=So11111111111111111111111111111111111111112&\
outputMint=EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v&\
amount=1000000000&\
forJitoBundle=true"
```

این پارامتر باعث می‌شود Metis:
- استخرهای ناسازگار با Jito (مثل HumidiFi) را حذف کند
- مسیرهای بهینه برای bundle های Jito انتخاب کند

### ۲. ساخت تراکنش با `/swap-instructions`

به جای `/swap` از `/swap-instructions` استفاده کنید تا دستورات خام را دریافت کرده و Jito tip را به آن اضافه کنید:

```json
POST http://localhost:8080/swap-instructions
{
  "quoteResponse": { ... },
  "userPublicKey": "YOUR_WALLET",
  "dynamicComputeUnitLimit": true
}
```

پاسخ شامل:
- `computeBudgetInstructions`
- `setupInstructions`
- `swapInstruction`
- `cleanupInstruction`

سپس Jito tip را به عنوان آخرین instruction اضافه کنید.

### ۳. محاسبه Tip داینامیک

Tip را به صورت off-chain بر اساس سود محاسبه کنید:

```javascript
const profit = quotedOutAmount - inputAmount;
const tipPercent = 0.05; // 5%
const tip = Math.max(
    Math.floor(profit * tipPercent),
    1000  // حداقل tip Jito
);
```

### ۴. ارسال به Jito Block Engine

```javascript
// اضافه کردن tip instruction
const tipInstruction = SystemProgram.transfer({
    fromPubkey: wallet.publicKey,
    toPubkey: jitoTipAccount,  // یکی از 8 آدرس Jito
    lamports: tip
});

// ساخت bundle
const bundle = [transaction];
await jitoClient.sendBundle(bundle);
```

---

## بهینه‌سازی و فیلترها

### فیلتر توکن‌ها

فقط استخرهایی که شامل توکن‌های زیر هستند لود می‌شوند:

```bash
FILTER_MARKETS_WITH_MINTS=\
So11111111111111111111111111111111111111112,\  # SOL
EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v,\  # USDC
Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB    # USDT
```

این فیلتر تعداد استخرها را از 188,382 به حدود 10,000 کاهش می‌دهد.

### فیلتر DEX ها

برای استفاده فقط از DEX های معتبر:

```bash
# فقط Raydium و Orca
DEX_PROGRAM_IDS=\
675kPX9MHTjS2zt1qfr1NYHuzeLXfQM9H24wFSUt1Mp8,\  # Raydium
whirLbMiicVdio4qvUfM5KAg6Ct8VwpYzGff3uctyCc     # Orca Whirlpool
```

یا برای حذف DEX های مشکل‌دار:

```bash
EXCLUDE_DEX_PROGRAM_IDS=problematic_dex_program_id
```

### به‌روزرسانی Market Cache

Market cache هر 30 دقیقه یکبار به‌روز می‌شود:

```bash
# دانلود جدیدترین نسخه
curl -o market-cache.json "https://cache.jup.ag/markets?v=4"

# restart Metis
./start-metis.sh
```

---

## عیب‌یابی

### خطا: "Binary key authentication failed"

**علت**: Binary Key نامعتبر یا منقضی شده
**راه‌حل**:
1. بررسی کنید که 10,000 JUP هنوز stake شده است
2. Binary Key را از portal.metis.builders دوباره دریافت کنید

### خطا: "RPC connection failed"

**علت**: مشکل در اتصال به RPC endpoint
**راه‌حل**:
1. RPC_URL را بررسی کنید
2. API key را چک کنید
3. از یک RPC دیگر امتحان کنید

### Metis خیلی کند است

**راه‌حل**:
1. فیلتر توکن‌ها را فعال کنید (FILTER_MARKETS_WITH_MINTS)
2. تعداد DEX ها را محدود کنید
3. RAM سرور را افزایش دهید

### Market cache قدیمی است

**راه‌حل**:
```bash
# دانلود دستی
curl -o market-cache.json "https://cache.jup.ag/markets?v=4"

# یا استفاده از mode=europa برای به‌روزرسانی خودکار
MARKET_MODE=europa
```

---

## 📊 آمار و اطلاعات

### Market Cache فعلی:
- **تعداد کل استخرها**: 188,382
- **Raydium**: 121,323 استخر (64%)
- **Meteora DLMM**: 41,161 استخر (22%)
- **Orca Whirlpool**: 880 استخر
- **Phoenix**: 3,278 استخر

### نسخه Metis:
- **نسخه فعلی**: 7.0.7
- **تاریخ انتشار**: Feb 27, 2025
- **حجم binary**: 28MB
- **پلتفرم**: x86_64-unknown-linux-gnu

---

## 🔗 منابع مفید

- [Metis Documentation](https://metis.builders/docs/self-host)
- [Binary Key Portal](https://portal.metis.builders/)
- [JUP Staking](https://vote.jup.ag/)
- [Jupiter API Docs](https://station.jup.ag/docs/apis/swap-api)
- [Jito Documentation](https://docs.jito.wtf/)
- [GitHub - Metis Binary](https://github.com/jup-ag/metis-binary)

---

## ⚠️ هشدارها

1. **امنیت Binary Key**: هرگز Binary Key را در کد عمومی قرار ندهید
2. **استفاده از Mainnet**: همیشه ابتدا در Devnet تست کنید
3. **مدیریت ریسک**: آربیتراژ ریسک دارد - سرمایه بیشتر از توان خود وارد نکنید
4. **Slippage**: برای آربیتراژ slippage=0 تنظیم کنید
5. **Gas Fees**: همیشه Jito tip را در محاسبات سود لحاظ کنید

---

**🎯 آماده برای آربیتراژ سیرکولار!**

اگر سؤالی دارید، به مستندات بالا مراجعه کنید یا با تیم پشتیبانی تماس بگیرید.
