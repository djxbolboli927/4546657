#!/usr/bin/env python3
"""
PMM Pool Extractor — Meteora DAMM Discovery Bot

با خواندن تراکنش‌های on-chain از یک ولت، استخرهای Meteora DAMM را پیدا می‌کند
و آدرس‌های vaultLpMint و vaultToken را مستقیماً از داده‌های حساب‌ها استخراج می‌کند.

اجرا:
    python3 pmm_pool_extractor.py
"""

import json, time, sys, http.client, ssl, urllib.parse, base64

# ═══════════════════════════════════════════
#                   CONFIG
# ═══════════════════════════════════════════

RPC    = "https://rpc.fra.shyft.to?api_key=farYqEW-7r1vxqok"
WALLET = "8oKVwqA5S2B7g2Li6yqZ4KKghpER8G1vj3bbeg8dFkSw"

# ─── تعداد تراکنش‌ها برای اسکن ───
LIMIT = 30

OUTPUT_JSON      = "pmm-pools.json"
OUTPUT_ANNOTATED = "pmm-pools-annotated.txt"

# ═══════════════════════════════════════════
#         PMM DEX — فقط Meteora DAMM برای تست
# ═══════════════════════════════════════════

PMM_DEX = {
    "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB": "Meteora_DAMM",
    # بعداً می‌توانید صرافی‌های PMM دیگر را اضافه کنید:
    # "PROGRAM_ID": "DEX_NAME",
}

METEORA_VAULT_PROGRAM = "24Uqj9JCLxUeoC3hGfh5W3s9FM9uCHDS2SG3LYwBpyTi"

# swapAccountSize — اعداد دقیق از فرمت متیس
SWAP_ACCOUNT_SIZE = {
    "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB": {
        "account_compressed_count": 13,
        "account_len": 18,
        "account_metas_count": 18,
    },
}

TOKEN_PROGRAMS = {
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
}

SKIP_ACCOUNTS = {
    "JUP6LkbZbjS1jKKwapdHNy74zcZ3tLUZoi5QNyVTaV4",
    "JUP2jxvXaqu7NQY1GmNf4m1vodw12LVXYxbFL2uB9Ne",
    "ComputeBudget111111111111111111111111111111",
    "11111111111111111111111111111111",
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJe1bz",
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",
    "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr",
    "Memo1UhkJBfCR6MNLc2aTHqszREt9aGdCrEBN6HNsA9M",
    "SysvarRent111111111111111111111111111111111",
    "SysvarC1ock11111111111111111111111111111111",
    "So11111111111111111111111111111111111111112",
}

KNOWN_TOKENS = {
    "So11111111111111111111111111111111111111112":    "SOL",
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v": "USDC",
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB":  "USDT",
    "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN":   "JUP",
    "27G8MtK7VtTcCHkpASjSDdkWWYfoqT6ggEuKidVJidD4":  "JLP",
    "mSoLzYCxHdYgdzU16g5QSh3i5K3z3KZK7ytfqcJm7So":  "mSOL",
    "J1toso1uCk3RLmjorhTtrVwY9HJ7X8V9yYac6Y7kGCPn": "JitoSOL",
    "bSo13r4TkiE4KumL71LsHTPpL2euBYLFx6h9HP3piy1":  "bSOL",
    "7vfCXTUXx5WJV5JADk17DUJ4ksgau7utNKj4b963voxs":  "ETH",
    "DezXAZ8z7PnrnRJjz3wXBoRgixCa6xjnB7YaB1pPB263":  "BONK",
}

# ═══════════════════════════════════════════
#   Meteora DAMM Pool — ساختار بایت (Borsh)
# ═══════════════════════════════════════════
#
#  offset   0 -  7 : discriminator (8 bytes)
#  offset   8 - 39 : lp_mint
#  offset  40 - 71 : token_a_mint
#  offset  72 -103 : token_b_mint
#  offset 104 -135 : a_vault
#  offset 136 -167 : b_vault
#  offset 168 -199 : a_vault_lp   (حساب LP پول در vault A)
#  offset 200 -231 : b_vault_lp   (حساب LP پول در vault B)
#  offset 232 -263 : a_vault_lp_mint  ← vaultLpMint.a
#  offset 264 -295 : b_vault_lp_mint  ← vaultLpMint.b

DAMM_POOL_MIN_SIZE = 296

DAMM_OFFSETS = {
    "lp_mint":         8,
    "token_a_mint":   40,
    "token_b_mint":   72,
    "a_vault":       104,
    "b_vault":       136,
    "a_vault_lp":    168,
    "b_vault_lp":    200,
    "a_vault_lp_mint": 232,
    "b_vault_lp_mint": 264,
}

# ═══════════════════════════════════════════
#   Meteora Vault — ساختار بایت (Borsh)
# ═══════════════════════════════════════════
#
#  offset  0 -  7 : discriminator (8 bytes)
#  offset  8      : enabled (u8)
#  offset  9      : vault_bump (u8)       ─┐ VaultBumps
#  offset 10      : token_vault_bump (u8) ─┘
#  offset 11 - 18 : total_amount (u64)
#  offset 19 - 50 : token_vault  ← vaultToken
#  offset 51 - 82 : fee_vault
#  offset 83 -114 : token_mint
#  offset 115-146 : lp_mint

VAULT_TOKEN_OFFSET = 19
VAULT_MIN_SIZE     = 51

# ═══════════════════════════════════════════
#                  BASE58
# ═══════════════════════════════════════════

_B58 = "123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz"

def bytes_to_base58(data):
    leading = 0
    for b in data:
        if b == 0:
            leading += 1
        else:
            break
    n = int.from_bytes(data, "big")
    chars = []
    while n:
        n, r = divmod(n, 58)
        chars.append(_B58[r])
    return "1" * leading + "".join(reversed(chars))

def read_pubkey(data, offset):
    """32 بایت از offset می‌خواند و به base58 تبدیل می‌کند."""
    if not data or len(data) < offset + 32:
        return None
    raw = data[offset: offset + 32]
    if not any(raw):   # همه صفر = invalid
        return None
    return bytes_to_base58(raw)

# ═══════════════════════════════════════════
#                   RPC
# ═══════════════════════════════════════════

def rpc_call(method, params):
    parsed = urllib.parse.urlparse(RPC)
    host   = parsed.netloc
    path   = parsed.path or "/"
    if parsed.query:
        path += "?" + parsed.query

    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": params})
    ctx  = ssl.create_default_context()
    conn = http.client.HTTPSConnection(host, timeout=30, context=ctx)
    try:
        conn.request("POST", path, body, {
            "Content-Type": "application/json",
            "Accept":       "application/json",
            "User-Agent":   "pmm-pool-extractor/1.0",
        })
        resp = conn.getresponse()
        raw  = resp.read().decode()
        if resp.status != 200:
            print(f"    ❌ HTTP {resp.status}: {raw[:200]}")
            return {}
        return json.loads(raw)
    finally:
        conn.close()

# کش حساب‌ها (encoding=base64 برای پارس binary)
_raw_cache = {}

def _fetch_raw_batch(pubkeys):
    to_fetch = [p for p in pubkeys if p not in _raw_cache]
    for i in range(0, len(to_fetch), 100):
        batch = to_fetch[i: i + 100]
        resp  = rpc_call("getMultipleAccounts", [
            batch, {"encoding": "base64", "commitment": "confirmed"}
        ])
        values = resp.get("result", {}).get("value", [None] * len(batch))
        for pk, info in zip(batch, values):
            _raw_cache[pk] = info
        time.sleep(0.12)

def _account_bytes(info):
    """اطلاعات حساب → bytes خام."""
    if not info:
        return None
    data_field = info.get("data")
    if isinstance(data_field, list) and data_field:
        try:
            return base64.b64decode(data_field[0])
        except Exception:
            return None
    return None

def get_raw_batch(pubkeys):
    _fetch_raw_batch(pubkeys)
    return {pk: _raw_cache.get(pk) for pk in pubkeys}

def get_raw_single(pubkey):
    if pubkey not in _raw_cache:
        _fetch_raw_batch([pubkey])
    info = _raw_cache.get(pubkey)
    return info, _account_bytes(info)

# ═══════════════════════════════════════════
#          استخراج دستورات DEX
# ═══════════════════════════════════════════

def extract_pmm_instructions(msg, meta):
    """دستورات Meteora DAMM را از outer و inner instructions پیدا می‌کند."""
    ixs  = []
    seen = set()

    def _add(prog, accs):
        if len(accs) < 5:
            return
        key = prog + "|" + ",".join(accs[:8])
        if key in seen:
            return
        seen.add(key)
        ixs.append({
            "dex_program": prog,
            "dex_name":    PMM_DEX[prog],
            "accounts":    accs,
        })

    # Outer instructions
    for ix in msg.get("instructions", []):
        prog = ix.get("programId", "")
        if prog in PMM_DEX:
            _add(prog, ix.get("accounts", []))

    # Inner instructions
    for group in meta.get("innerInstructions", []):
        for ix in group.get("instructions", []):
            prog = ix.get("programId", "")
            if prog in PMM_DEX:
                _add(prog, ix.get("accounts", []))

    return ixs

# ═══════════════════════════════════════════
#           پیدا کردن Pool Account
# ═══════════════════════════════════════════

def find_pool_account(dex_program, accounts):
    """
    از لیست حساب‌های دستور، حسابی که:
      - owned by dex_program باشد
      - اندازه داده‌اش کافی باشد (pool state)
    را برمی‌گرداند.
    """
    skip = (
        SKIP_ACCOUNTS
        | set(PMM_DEX.keys())
        | TOKEN_PROGRAMS
        | {WALLET, METEORA_VAULT_PROGRAM}
    )
    candidates = [a for a in accounts if a not in skip]
    if not candidates:
        return None, None

    # حداکثر 35 کاندید اول را بررسی می‌کنیم
    infos = get_raw_batch(candidates[:35])

    for pubkey in candidates[:35]:
        info = infos.get(pubkey)
        if not info:
            continue
        if info.get("owner") != dex_program:
            continue
        data = _account_bytes(info)
        if data and len(data) >= DAMM_POOL_MIN_SIZE:
            return pubkey, data

    return None, None

# ═══════════════════════════════════════════
#        پارس Pool و Vault، ساخت نتیجه
# ═══════════════════════════════════════════

def parse_damm_pool(data):
    """ساختار Meteora DAMM Pool را پارس می‌کند."""
    if not data or len(data) < DAMM_POOL_MIN_SIZE:
        return None
    result = {}
    for field, offset in DAMM_OFFSETS.items():
        pk = read_pubkey(data, offset)
        if pk:
            result[field] = pk
    return result

def process_damm_pool(pool_pubkey, pool_bytes, alt, sig):
    """
    Pool را پارس می‌کند، Vault‌ها را fetch می‌کند،
    و دیکشنری نتیجه را برمی‌گرداند.
    """
    pool = parse_damm_pool(pool_bytes)
    if not pool:
        print("   ❌ pool parse failed (data too short)")
        return None

    required = ["token_a_mint", "token_b_mint",
                "a_vault", "b_vault",
                "a_vault_lp_mint", "b_vault_lp_mint"]
    missing = [f for f in required if not pool.get(f)]
    if missing:
        print(f"   ❌ missing pool fields: {missing}")
        return None

    # Fetch vault accounts برای استخراج token_vault
    _, a_vault_bytes = get_raw_single(pool["a_vault"])
    _, b_vault_bytes = get_raw_single(pool["b_vault"])

    vault_token_a = read_pubkey(a_vault_bytes, VAULT_TOKEN_OFFSET) if a_vault_bytes else None
    vault_token_b = read_pubkey(b_vault_bytes, VAULT_TOKEN_OFFSET) if b_vault_bytes else None

    if not vault_token_a or not vault_token_b:
        # اگر offset اشتباه بود، اطلاعات debug چاپ می‌کنیم
        print(f"   ❌ vault token parse failed")
        if a_vault_bytes:
            print(f"      vault_a size={len(a_vault_bytes)}, hex[:64]={a_vault_bytes[:64].hex()}")
        if b_vault_bytes:
            print(f"      vault_b size={len(b_vault_bytes)}, hex[:64]={b_vault_bytes[:64].hex()}")
        return None

    return {
        "pool_pubkey":    pool_pubkey,
        "dex_program":    "Eo7WjKq67rjJQSZxS6z3YkapzY3eMj6Xy8X5EQVn5UaB",
        "dex_name":       "Meteora_DAMM",
        "token_a_mint":   pool["token_a_mint"],
        "token_b_mint":   pool["token_b_mint"],
        "vault_lp_mint_a": pool["a_vault_lp_mint"],
        "vault_lp_mint_b": pool["b_vault_lp_mint"],
        "vault_token_a":  vault_token_a,
        "vault_token_b":  vault_token_b,
        "alt":            alt,
        "sig":            sig,
    }

# ═══════════════════════════════════════════
#                   MAIN
# ═══════════════════════════════════════════

def token_label(mint):
    return KNOWN_TOKENS.get(mint, mint[:12] + "…")

def main():
    print("═" * 52)
    print("  PMM Pool Extractor — Meteora DAMM")
    print(f"  Wallet : {WALLET[:20]}...")
    print(f"  Limit  : {LIMIT} transactions")
    print("═" * 52 + "\n")

    print(f"📡 Fetching {LIMIT} signatures...")
    resp = rpc_call("getSignaturesForAddress",
                    [WALLET, {"limit": LIMIT, "commitment": "confirmed"}])
    sigs = [s["signature"] for s in resp.get("result", []) if not s.get("err")]
    print(f"   ✅ {len(sigs)} successful transactions\n")

    if not sigs:
        print("❌ No transactions found")
        sys.exit(1)

    results    = []
    seen_pools = set()

    for i, sig in enumerate(sigs, 1):
        print(f"─── [{i}/{len(sigs)}] {sig[:32]}... ───")

        try:
            resp = rpc_call("getTransaction", [sig, {
                "encoding":                      "jsonParsed",
                "maxSupportedTransactionVersion": 0,
                "commitment":                    "confirmed",
            }])
            tx = resp.get("result")
        except Exception as e:
            print(f"   ❌ Error: {e}")
            time.sleep(0.3)
            continue

        if not tx:
            print("   ❌ null transaction")
            time.sleep(0.15)
            continue

        msg  = tx["transaction"]["message"]
        meta = tx["meta"]

        alts     = msg.get("addressTableLookups", [])
        alt_keys = [a["accountKey"] for a in alts] if alts else []

        if not alt_keys:
            print("   ➖ no ALT — skipping")
            time.sleep(0.1)
            continue

        dex_ixs = extract_pmm_instructions(msg, meta)

        if not dex_ixs:
            print("   ➖ no Meteora DAMM instructions")
            time.sleep(0.1)
            continue

        print(f"   📊 {len(dex_ixs)} DAMM instruction(s), ALTs: {len(alt_keys)}")

        for dix in dex_ixs:
            prog = dix["dex_program"]
            accs = dix["accounts"]
            print(f"   🔍 {dix['dex_name']} ({len(accs)} accounts) → ", end="", flush=True)

            pool_pubkey, pool_bytes = find_pool_account(prog, accs)

            if not pool_pubkey:
                print("❌ pool not found")
                continue

            if pool_pubkey in seen_pools:
                print("♻️  duplicate")
                continue

            seen_pools.add(pool_pubkey)

            result = process_damm_pool(pool_pubkey, pool_bytes, alt_keys[0], sig)
            if not result:
                continue

            la = token_label(result["token_a_mint"])
            lb = token_label(result["token_b_mint"])
            print(f"✅ {la} / {lb}")
            print(f"      Pool          : {pool_pubkey}")
            print(f"      vaultLpMint.a : {result['vault_lp_mint_a']}")
            print(f"      vaultLpMint.b : {result['vault_lp_mint_b']}")
            print(f"      vaultToken.a  : {result['vault_token_a']}")
            print(f"      vaultToken.b  : {result['vault_token_b']}")

            results.append(result)

        time.sleep(0.15)

    # ── خروجی ──
    print("\n" + "═" * 52)
    print(f"  Results : {len(results)} PMM pools found")
    print(f"  Cache   : {len(_raw_cache)} accounts fetched")
    print("═" * 52 + "\n")

    if not results:
        print("❌ No Meteora DAMM pools found")
        print("   → تعداد LIMIT را بالا ببرید یا ولت دیگری را امتحان کنید")
        sys.exit(0)

    # ── ساخت فرمت متیس ──
    cache_entries = []
    for r in results:
        entry = {
            "pubkey": r["pool_pubkey"],
            "owner":  r["dex_program"],
            "params": {
                "addressLookupTableAddress": r["alt"],
                "routingGroup": 2,
                "swapAccountSize": SWAP_ACCOUNT_SIZE[r["dex_program"]],
                "vaultLpMint": {
                    "a": r["vault_lp_mint_a"],
                    "b": r["vault_lp_mint_b"],
                },
                "vaultToken": {
                    "a": r["vault_token_a"],
                    "b": r["vault_token_b"],
                },
            },
        }
        cache_entries.append(entry)

    # ── ذخیره JSON ──
    with open(OUTPUT_JSON, "w") as f:
        json.dump(cache_entries, f, indent=2)

    # ── ذخیره annotated ──
    with open(OUTPUT_ANNOTATED, "w") as f:
        f.write("// PMM Pool Extractor — Meteora DAMM\n")
        f.write(f"// Wallet: {WALLET}\n")
        f.write(f"// Total: {len(results)} pools\n\n[\n")
        for idx, (r, e) in enumerate(zip(results, cache_entries)):
            comma = "," if idx < len(results) - 1 else ""
            la = token_label(r["token_a_mint"])
            lb = token_label(r["token_b_mint"])
            f.write(f"  // [{idx+1}] {r['dex_name']} | {la} / {lb}\n")
            f.write(f"  // Mints: {r['token_a_mint']}, {r['token_b_mint']}\n")
            f.write(f"  // TX: {r['sig'][:56]}...\n")
            block = json.dumps(e, indent=2).replace("\n", "\n  ")
            f.write(f"  {block}{comma}\n\n")
        f.write("]\n")

    print(f"📄 {OUTPUT_JSON}")
    print(f"📝 {OUTPUT_ANNOTATED}\n")

    for idx, (r, e) in enumerate(zip(results, cache_entries)):
        la = token_label(r["token_a_mint"])
        lb = token_label(r["token_b_mint"])
        print(f"┌─ [{idx+1}] {r['dex_name']}  |  {la} / {lb}")
        print(f"│  Pool : {r['pool_pubkey']}")
        print(f"│  ALT  : {r['alt']}")
        print("└─ Cache entry:")
        for line in json.dumps(e, indent=2).split("\n"):
            print(f"   {line}")
        print()


if __name__ == "__main__":
    main()
