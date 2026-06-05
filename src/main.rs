#[allow(dead_code)]
mod account_cache;
mod alt_cache;
mod arbitrage;
mod arb_cycle;
mod arb_validator;
mod blockhash_cache;
mod config;
mod cycle_executor;
mod dex;
mod dex_accounts;
mod jito;
#[allow(dead_code)]
mod jito_grpc;
#[allow(dead_code)]
mod litesvm_sim;
mod metis;
mod metrics;
mod native_ix;
mod no_metis_executor;
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

    // ── Pool state data source — explicit `mode`, no implicit gRPC ──────────
    //   mode = "direct_grpc_fast"  → bot subscribes directly to Yellowstone.
    //   mode = "relay_socket"      → bot reads from the fanout Unix socket.
    //   mode = "disabled"          → no pool-state stream.
    //
    // validation.enabled is a *consumer* flag — it never selects a data source.
    // If validation is on but the source is disabled, we bail with a clear
    // message rather than silently doing nothing.
    let source = config.pool_state.resolved_mode();
    if source == "invalid" {
        anyhow::bail!(
            "[pool_state] mode = \"{}\" is not recognised. Use one of: \
             direct_grpc_fast, relay_socket, disabled.",
            config.pool_state.mode
        );
    }
    if source == "disabled" && config.validation.enabled {
        anyhow::bail!(
            "validation.enabled=true but pool_state source is disabled.\n\
             Set [pool_state].mode = \"direct_grpc_fast\" (direct Yellowstone gRPC)\n\
             or [pool_state].mode = \"relay_socket\" with a socket path."
        );
    }

    let pool_state_result = if source != "disabled" {
        match pool_state_stream::load_mix_json(&config.pool_state.mix_json) {
            Ok(parsed) => {
                // Optional truncation for connectivity tests. 0 = no limit.
                let mut subscribe_list = parsed.subscribe_list;
                let full = subscribe_list.len();
                let max = config.pool_state.max_accounts;
                if max > 0 && full > max {
                    eprintln!(
                        "[pool_state] WARNING: truncating subscription {full} → {max} \
                         accounts (max_accounts={max}); some pools will never go live. \
                         Set max_accounts=0 for production."
                    );
                    subscribe_list.truncate(max);
                }

                let store = pool_state_store::PoolStateStore::new(
                    parsed.account_to_pools,
                    parsed.pool_to_accounts,
                );
                eprintln!(
                    "[pool_state] mode={source} mix.json: {} pools, {} unique accounts, {} vault pairs",
                    store.pool_count,
                    full,
                    parsed.vault_pairs.len(),
                );

                match source {
                    "relay_socket" => {
                        if config.pool_state.socket.is_empty() {
                            anyhow::bail!(
                                "[pool_state] mode=relay_socket but socket path is empty. \
                                 Set [pool_state].socket = \"/tmp/yellowstone_fanout.sock\"."
                            );
                        }
                        eprintln!(
                            "[pool_state] data source: relay socket {}",
                            config.pool_state.socket
                        );
                        pool_state_socket::spawn_socket_reader(
                            config.pool_state.socket.clone(),
                            store.clone(),
                        );
                    }
                    "direct_grpc_fast" => {
                        eprintln!(
                            "[pool_state] data source: direct Yellowstone gRPC ({})",
                            config.yellowstone_grpc.endpoint
                        );
                        pool_state_stream::spawn_pool_state_stream_sharded(
                            config.yellowstone_grpc.endpoint.clone(),
                            config.yellowstone_grpc.x_token.clone(),
                            subscribe_list,
                            store.clone(),
                            config.pool_state.accounts_per_stream,
                        );
                    }
                    _ => unreachable!(),
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

    // ── Validation mode: price comparison + cycle scan ───────────────────────
    // When execute_cycles=true, profitable cycles are also forwarded to Metis
    // for /swap-instructions and then submitted to Jito.
    if config.validation.enabled {
        if let Some((store, vault_pairs)) = pool_state_result {
            // 1. Jupiter price validator (local spot price vs Jupiter reference).
            validator::spawn_validator(
                vault_pairs.clone(),
                store.clone(),
                rpc_client.clone(),
                config.validation.clone(),
                config.jupiter_price.clone(),
            );

            // 2a. validate_local mode: per-hop Metis /quote comparison, no Jito sends.
            //     Takes priority over execute_cycles when both are enabled.
            let hit_tx = if config.arb_test.validate_local {
                let metis_val = Arc::new(metis::MetisClient::new(
                    &config.metis.url,
                    config.performance.quote_timeout_ms,
                ));
                let (tx, rx) = tokio::sync::mpsc::channel::<arb_cycle::CycleHit>(200);
                arb_validator::spawn_arb_validator(rx, metis_val, config.arb_test.clone());
                Some(tx)
            // 2b. send_no_metis mode: native DEX instructions, direct Jito send.
            } else if config.arb_test.send_no_metis {
                let blockhash_cache_nm = Arc::new(BlockhashCache::new(rpc_client.clone()));
                let jito_nm = Arc::new(jito::JitoClient::new(
                    &config.jito.urls,
                    &config.jito.uuid,
                ));
                let jito_lim_nm = Arc::new(Mutex::new(
                    RateLimiter::new(config.jito.max_bundles_per_second),
                ));

                let (jito_grpc_nm, grpc_lim_nm) = if config.jito_grpc.enabled {
                    match jito_grpc::JitoGrpcClient::new(
                        &config.jito_grpc.endpoints,
                        &config.jito_grpc.auth_keypair,
                    )
                    .await
                    {
                        Ok(client) => {
                            let lim = Arc::new(Mutex::new(RateLimiter::new(
                                config.jito_grpc.max_bundles_per_second,
                            )));
                            (Some(Arc::new(client)), Some(lim))
                        }
                        Err(e) => {
                            eprintln!("[no_metis] Jito gRPC init failed: {e} — REST-only");
                            (None, None)
                        }
                    }
                } else {
                    (None, None)
                };

                let cu_limit = config.arb_test.no_metis.cu_limit;
                let nm_cfg = config.arb_test.no_metis.clone();

                let ctx = Arc::new(no_metis_executor::NoMetisCtx {
                    jito: jito_nm,
                    jito_grpc: jito_grpc_nm,
                    jito_limiter: jito_lim_nm,
                    jito_grpc_limiter: grpc_lim_nm,
                    trading_keypair: trading_keypair.clone(),
                    rpc_client: rpc_client.clone(),
                    blockhash_cache: blockhash_cache_nm,
                    store: store.clone(),
                    cfg: nm_cfg,
                    cu_limit,
                });

                let (tx, rx) = tokio::sync::mpsc::channel::<arb_cycle::CycleHit>(200);
                no_metis_executor::spawn_no_metis_executor(rx, ctx);
                Some(tx)
            // 2c. Optionally init execution stack and forward hits to Jito.
            } else if config.validation.execute_cycles {
                let blockhash_cache_exec = Arc::new(BlockhashCache::new(rpc_client.clone()));
                let alt_cache_exec = AltCache::new(transaction::jito_tip_pubkeys());
                let metis_exec = Arc::new(metis::MetisClient::new(
                    &config.metis.url,
                    config.performance.quote_timeout_ms,
                ));
                let jito_exec = Arc::new(jito::JitoClient::new(
                    &config.jito.urls,
                    &config.jito.uuid,
                ));
                let jito_lim_exec = Arc::new(Mutex::new(
                    RateLimiter::new(config.jito.max_bundles_per_second),
                ));

                let (jito_grpc_exec, grpc_lim_exec) = if config.jito_grpc.enabled {
                    match jito_grpc::JitoGrpcClient::new(
                        &config.jito_grpc.endpoints,
                        &config.jito_grpc.auth_keypair,
                    )
                    .await
                    {
                        Ok(client) => {
                            let lim = Arc::new(Mutex::new(RateLimiter::new(
                                config.jito_grpc.max_bundles_per_second,
                            )));
                            (Some(Arc::new(client)), Some(lim))
                        }
                        Err(e) => {
                            eprintln!("[executor] Jito gRPC init failed: {e} — REST-only");
                            (None, None)
                        }
                    }
                } else {
                    (None, None)
                };

                let ctx = Arc::new(cycle_executor::ExecutorCtx {
                    metis: metis_exec,
                    jito: jito_exec,
                    jito_grpc: jito_grpc_exec,
                    jito_limiter: jito_lim_exec,
                    jito_grpc_limiter: grpc_lim_exec,
                    trading_keypair: trading_keypair.clone(),
                    rpc_client: rpc_client.clone(),
                    blockhash_cache: blockhash_cache_exec,
                    alt_cache: alt_cache_exec,
                    cu_limits: config.performance.cu_limits.clone(),
                    user_pubkey: trading_keypair.pubkey().to_string(),
                    min_profit_lamports: config.validation.min_exec_profit_lamports,
                    tip_lamports: config.jito.tip_min_lamports,
                    base_fee_lamports: config.trading.base_fee_lamports,
                });

                let (tx, rx) = tokio::sync::mpsc::channel(200);
                cycle_executor::spawn_cycle_executor(rx, ctx);
                eprintln!(
                    "[executor] started — min_profit={}L tip={}L base_fee={}L",
                    config.validation.min_exec_profit_lamports,
                    config.jito.tip_min_lamports,
                    config.trading.base_fee_lamports,
                );
                Some(tx)
            } else {
                None
            };

            // 3. WSOL cycle evaluator — finds 2-hop and 3-hop arb opportunities.
            //    Net-positive hits are forwarded to executor when execute_cycles=true.
            arb_cycle::spawn_cycle_scanner(
                vault_pairs,
                store,
                config.validation.interval_secs,
                config.validation.max_pools_log,
                arb_cycle::DEFAULT_TX_COST,
                hit_tx,
            );
        } else {
            eprintln!("[validator] ERROR: pool state unavailable; set pool_state.mix_json in config");
        }
        // Park here until ctrl-c.
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
