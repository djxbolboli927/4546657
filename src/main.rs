#[allow(dead_code)]
mod account_cache;
mod alt_cache;
mod arbitrage;
mod blockhash_cache;
mod config;
mod dex_accounts;
mod jito;
#[allow(dead_code)]
mod jito_grpc;
#[allow(dead_code)]
mod litesvm_sim;
mod metis;
mod metrics;
mod program_registry;
mod rate_limiter;
mod pool_state_socket;
mod pool_state_store;
mod pool_state_stream;
mod template_cache;
mod validator;
mod token_metrics;
mod tokens;
mod transaction;
mod wallet;

use anyhow::Result;
use solana_client::rpc_client::RpcClient;
use solana_sdk::signer::Signer;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use tracing::error;

use alt_cache::AltCache;
use blockhash_cache::BlockhashCache;
use rate_limiter::RateLimiter;

fn main() -> Result<()> {
    let log_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "error".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            format!("{log_filter},hyper_util=error,hyper=error,reqwest=error,h2=error,tonic=error"),
        ))
        .init();

    let config = config::Config::load("config.toml")?;

    let worker_threads = config.performance.threads.max(1);
    let pinned_cores: Vec<usize> = config.performance.bot_cpu_cores.clone();
    let available_cores = core_affinity::get_core_ids().unwrap_or_default();
    let next_worker = Arc::new(AtomicUsize::new(0));

    let mut builder = tokio::runtime::Builder::new_multi_thread();
    builder.worker_threads(worker_threads).enable_all();
    builder.thread_name("arb-worker");

    if !pinned_cores.is_empty() {
        let cores = pinned_cores.clone();
        let available = available_cores.clone();
        let counter = next_worker.clone();
        builder.on_thread_start(move || {
            let idx = counter.fetch_add(1, Ordering::SeqCst);
            let target = cores[idx % cores.len()];
            if let Some(core_id) = available.iter().find(|c| c.id == target) {
                core_affinity::set_for_current(*core_id);
            }
        });
    }

    let runtime = builder.build()?;
    runtime.block_on(async_main(config))
}

async fn async_main(config: config::Config) -> Result<()> {
    let token_mints = tokens::load_tokens(&config.trading.tokens_file)?;

    let trading_keypair = Arc::new(wallet::read_keypair(&config.jito.trading_keypair)?);

    let rpc_client = Arc::new(RpcClient::new(config.rpc.url.clone()));

    let wsol_mint = solana_sdk::pubkey::Pubkey::from_str_const(tokens::WSOL_MINT);
    let wsol_ata = spl_associated_token_account::get_associated_token_address(
        &trading_keypair.pubkey(),
        &wsol_mint,
    );

    // ── Template cache: load hop templates from disk and start periodic flush ─
    let template_store = template_cache::TemplateStore::new();
    if config.template_cache.save_new || config.template_cache.serve_route {
        let hops_loaded = template_store.load_from_disk();
        let routes_loaded = template_store.load_routes_from_disk();
        eprintln!(
            "[template] loaded {hops_loaded} hop templates and {routes_loaded} route templates from /root/c/cache/"
        );
        template_store.spawn_flush_task(60);
    }

    // ── Pool state + optional price validator ────────────────────────────────
    // Data source priority:
    //   1. pool_state.socket  — reads from yellowstone_fanout Unix socket
    //      (recommended: same data as Metis, no extra Yellowstone connection)
    //   2. pool_state.enabled — direct Yellowstone gRPC subscription (fallback)
    //
    // When validation.enabled = true the normal Metis/Jito bot loop is skipped.
    let need_pool_state = !config.pool_state.socket.is_empty()
        || config.pool_state.enabled
        || config.validation.enabled;

    let pool_state_result = if need_pool_state {
        match pool_state_stream::load_mix_json(&config.pool_state.mix_json) {
            Ok(parsed) => {
                let store = pool_state_store::PoolStateStore::new(
                    parsed.account_to_pools,
                    parsed.pool_to_accounts,
                );
                eprintln!(
                    "[pool_state] mix.json: {} pools, {} accounts, {} vault pairs",
                    store.pool_count,
                    parsed.subscribe_list.len(),
                    parsed.vault_pairs.len(),
                );

                if !config.pool_state.socket.is_empty() {
                    // ── Preferred: read from fanout socket (no extra gRPC) ──────
                    eprintln!(
                        "[pool_state] data source: fanout socket {}",
                        config.pool_state.socket
                    );
                    pool_state_socket::spawn_socket_reader(
                        config.pool_state.socket.clone(),
                        store.clone(),
                    );
                } else if config.pool_state.enabled || config.validation.enabled {
                    // ── Fallback: direct Yellowstone gRPC ───────────────────────
                    eprintln!("[pool_state] data source: direct Yellowstone gRPC");
                    pool_state_stream::spawn_pool_state_stream(
                        config.yellowstone_grpc.endpoint.clone(),
                        config.yellowstone_grpc.x_token.clone(),
                        parsed.subscribe_list,
                        store.clone(),
                    );
                }

                Some((store, parsed.vault_pairs))
            }
            Err(e) => {
                eprintln!(
                    "[pool_state] WARNING: could not load mix.json ({e}); pool state disabled"
                );
                None
            }
        }
    } else {
        None
    };

    // ── Validation mode: skip bot, only run price comparisons ─────────────────
    if config.validation.enabled {
        if let Some((store, vault_pairs)) = pool_state_result {
            validator::spawn_validator(
                vault_pairs,
                store,
                config.validation.clone(),
                config.jupiter_price.clone(),
            );
        } else {
            eprintln!("[validator] ERROR: pool state unavailable; set pool_state.mix_json in config");
        }
        // Park here until ctrl-c — no bot activity
        tokio::signal::ctrl_c().await.ok();
        return Ok(());
    }

    let metrics = metrics::Metrics::new();
    metrics.spawn_reporter(config.performance.queue_max_age_ms, template_store.clone());

    let token_metrics = token_metrics::TokenMetrics::new(&token_mints);
    token_metrics.spawn_reporter();

    let blockhash_cache = Arc::new(BlockhashCache::new(rpc_client.clone()));

    let tip_pubkeys = transaction::jito_tip_pubkeys();
    let alt_cache = AltCache::new(tip_pubkeys);

    let metis = Arc::new(metis::MetisClient::new(
        &config.metis.url,
        config.performance.quote_timeout_ms,
    ));

    let jito_client = Arc::new(jito::JitoClient::new(&config.jito.urls, &config.jito.uuid));

    let jito_limiter = Arc::new(Mutex::new(
        RateLimiter::new(config.jito.max_bundles_per_second),
    ));

    let (jito_grpc_client, jito_grpc_limiter) = if config.jito_grpc.enabled {
        match jito_grpc::JitoGrpcClient::new(
            &config.jito_grpc.endpoints,
            &config.jito_grpc.auth_keypair,
        )
        .await
        {
            Ok(client) => {
                let limiter = Arc::new(Mutex::new(RateLimiter::new(
                    config.jito_grpc.max_bundles_per_second,
                )));
                (Some(Arc::new(client)), Some(limiter))
            }
            Err(e) => {
                eprintln!("Jito gRPC init failed: {e} — continuing REST-only");
                (None, None)
            }
        }
    } else {
        (None, None)
    };

    let (sim_cache, sim_pool) = if config.simulation.enabled {
        let cache = account_cache::AccountCache::new(rpc_client.clone());

        if let Ok(s) = rpc_client.get_slot() {
            cache.seed_slot(s);
        }

        let dex_pools = dex_accounts::load(&config.simulation.dex_dir);
        let mut live_extra = vec![wsol_ata];
        live_extra.extend_from_slice(&dex_pools.subscribe_accounts);

        cache.spawn_subscription(
            config.yellowstone_grpc.endpoint.clone(),
            config.yellowstone_grpc.x_token.clone(),
            program_registry::all_program_ids(),
            live_extra,
        );

        let mut warm: Vec<solana_sdk::pubkey::Pubkey> = token_mints
            .iter()
            .filter_map(|s| solana_sdk::pubkey::Pubkey::try_from(s.as_str()).ok())
            .collect();
        warm.push(wsol_mint);
        warm.push(wsol_ata);
        warm.push(trading_keypair.pubkey());
        for mint_str in &token_mints {
            if let Ok(mint) = solana_sdk::pubkey::Pubkey::try_from(mint_str.as_str()) {
                let ata = spl_associated_token_account::get_associated_token_address(
                    &trading_keypair.pubkey(),
                    &mint,
                );
                warm.push(ata);
            }
        }
        warm.extend_from_slice(&dex_pools.all_accounts);
        cache.prefetch(&warm);

        let pool = litesvm_sim::SimulatorPool::new(
            config.simulation.workers,
            &config.simulation.so_dir,
            wsol_ata,
            config.simulation.fail_closed,
            cache.stream_slot(),
        )?;
        (Some(Arc::new(cache)), Some(Arc::new(pool)))
    } else {
        (None, None)
    };

    // ── Build shared CalcCtx ─────────────────────────────────────────────────
    let calc_ctx = Arc::new(arbitrage::CalcCtx {
        metis: metis.clone(),
        blockhash_cache: blockhash_cache.clone(),
        trading_keypair: trading_keypair.clone(),
        rpc_client: rpc_client.clone(),
        alt_cache: alt_cache.clone(),
        jito: jito_client,
        jito_grpc: jito_grpc_client,
        jito_limiter: jito_limiter.clone(),
        jito_grpc_limiter: jito_grpc_limiter.clone(),
        cu_limits: config.performance.cu_limits.clone(),
        user_pubkey: trading_keypair.pubkey().to_string(),
        sim_cache,
        sim_pool,
        template_store,
    });

    let worker_count = config.performance.calc_workers.max(1);
    let jito_capacity = config.jito.max_bundles_per_second as usize
        + jito_grpc_limiter
            .as_ref()
            .map(|_| config.jito_grpc.max_bundles_per_second as usize)
            .unwrap_or(0);
    let pipeline = arbitrage::spawn_workers(
        calc_ctx.clone(),
        metrics.clone(),
        worker_count,
        config.performance.queue_max_age_ms,
    );

    eprintln!(
        "scanner ready | tokens={} | pairs_per_scan={} | calc_workers={worker_count} | jito_capacity_per_sec={jito_capacity} | quote_concurrency={}",
        token_mints.len(),
        {
            let steps = ((config.trading.max_amount_sol - config.trading.min_amount_sol)
                / config.trading.step_sol) as usize
                + 1;
            steps * token_mints.len() * 2 // ×2: free + direct route per pair
        },
        config.performance.max_concurrent_quotes.max(1),
    );

    loop {
        if let Err(e) = arbitrage::scan_all_tokens(
            &token_mints,
            &config,
            &calc_ctx,
            &pipeline,
            &metrics,
            &token_metrics,
        )
        .await
        {
            error!(error = %e, "scan cycle error");
        }
    }
}
