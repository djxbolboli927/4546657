#!/bin/bash

# ═══════════════════════════════════════════════════════════════
# METIS STARTUP SCRIPT FOR CIRCULAR ARBITRAGE
# ═══════════════════════════════════════════════════════════════

set -e

METIS_DIR="/home/user/4546657/metis"
cd "$METIS_DIR"

echo "🚀 Starting Metis v7 for Circular Arbitrage..."
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# بررسی وجود Binary Key
if ! grep -q "BINARY_KEY=" metis.env || grep -q "YOUR_BINARY_KEY_HERE" metis.env; then
    echo "❌ خطا: Binary Key تنظیم نشده است!"
    echo ""
    echo "برای دریافت Binary Key:"
    echo "  1. حداقل 10,000 JUP در https://vote.jup.ag/ stake کنید"
    echo "  2. از https://portal.metis.builders/ درخواست key بدهید"
    echo "  3. Key دریافتی را در فایل metis.env جایگزین کنید"
    echo ""
    exit 1
fi

# بررسی وجود market cache
if [ ! -f "market-cache.json" ]; then
    echo "⚠️  فایل market-cache.json یافت نشد!"
    echo "📥 در حال دانلود market cache..."
    curl -o market-cache.json "https://cache.jup.ag/markets?v=4"
    echo "✅ Market cache دانلود شد"
fi

# نمایش تعداد استخرها
TOTAL_MARKETS=$(cat market-cache.json | jq '. | length')
echo "📊 تعداد کل استخرها: $TOTAL_MARKETS"

# شمارش استخرهایی که فیلتر می‌شوند
echo "🔍 فیلتر استخرها بر اساس: SOL, USDC, USDT"

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "⚙️  پارامترهای Metis:"
echo "  • Circular Arbitrage: ✅ فعال"
echo "  • Market Mode: file (local cache)"
echo "  • API Endpoint: http://127.0.0.1:8080"
echo "  • Filtered Mints: SOL, USDC, USDT"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo ""

# اجرای Metis
./metis-binary \
    --env-file metis.env \
    2>&1 | tee metis.log

