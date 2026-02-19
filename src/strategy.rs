// ============================================================================
//  STRATEGY — Arbitraj Strateji Motoru
//
//  Havuz verilerini analiz eder, fırsatları tespit eder,
//  Newton-Raphson ile optimal miktarı hesaplar,
//  REVM ile simüle eder ve kontrat tetikleme kararı verir.
// ============================================================================

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::transports::Transport;
use alloy::network::Ethereum;
use alloy::signers::local::PrivateKeySigner;
use alloy::network::EthereumWallet;
use colored::*;
use chrono::Local;
use std::time::Duration;

use crate::types::*;
use crate::math;
use crate::simulator::SimulationEngine;

// ─────────────────────────────────────────────────────────────────────────────
// Zaman Damgası
// ─────────────────────────────────────────────────────────────────────────────

fn timestamp() -> String {
    Local::now().format("%H:%M:%S%.3f").to_string()
}

// ─────────────────────────────────────────────────────────────────────────────
// Arbitraj Fırsat Tespiti
// ─────────────────────────────────────────────────────────────────────────────

/// Her iki havuzun fiyatlarını karşılaştır ve fırsat varsa tespit et
///
/// Fırsat Koşulları:
///   1. Her iki havuz aktif ve veriler taze
///   2. Fiyat farkı (spread) > minimum eşik
///   3. Newton-Raphson ile hesaplanan kâr > minimum net kâr
pub fn check_arbitrage_opportunity(
    pools: &[PoolConfig],
    states: &[SharedPoolState],
    config: &BotConfig,
) -> Option<ArbitrageOpportunity> {
    if pools.len() < 2 || states.len() < 2 {
        return None;
    }

    // Read lock — çok kısa süreli
    let state_a = states[0].read().clone();
    let state_b = states[1].read().clone();

    // Her iki havuz aktif mi?
    if !state_a.is_active() || !state_b.is_active() {
        return None;
    }

    // Veri tazeliği kontrolü
    if state_a.staleness_ms() > config.max_staleness_ms
        || state_b.staleness_ms() > config.max_staleness_ms
    {
        return None;
    }

    let price_a = state_a.eth_price_usd;
    let price_b = state_b.eth_price_usd;

    // Spread hesapla
    let spread = (price_a - price_b).abs();
    let min_price = price_a.min(price_b);
    let spread_pct = if min_price > 0.0 {
        (spread / min_price) * 100.0
    } else {
        return None;
    };

    // Yön belirleme: Ucuzdan al, pahalıya sat
    let (buy_idx, sell_idx) = if price_a < price_b {
        (0, 1) // A ucuz, B pahalı
    } else {
        (1, 0) // B ucuz, A pahalı
    };

    let buy_state = if buy_idx == 0 { &state_a } else { &state_b };
    let sell_state = if sell_idx == 0 { &state_a } else { &state_b };
    let eth_price_ref = (price_a + price_b) / 2.0;

    // ─── Newton-Raphson Optimal Miktar Hesaplama ──────────────────
    let nr_result = math::find_optimal_amount(
        sell_state,
        pools[sell_idx].fee_fraction,
        buy_state,
        pools[buy_idx].fee_fraction,
        config.gas_cost_usd,
        config.flash_loan_fee_bps,
        eth_price_ref,
        config.max_trade_size_weth,
    );

    // Kârlı değilse fırsatı atla
    if nr_result.expected_profit < config.min_net_profit_usd || nr_result.optimal_amount <= 0.0 {
        return None;
    }

    Some(ArbitrageOpportunity {
        buy_pool_idx: buy_idx,
        sell_pool_idx: sell_idx,
        optimal_amount_weth: nr_result.optimal_amount,
        expected_profit_usd: nr_result.expected_profit,
        buy_price: buy_state.eth_price_usd,
        sell_price: sell_state.eth_price_usd,
        spread_pct,
        nr_converged: nr_result.converged,
        nr_iterations: nr_result.iterations,
    })
}

// ─────────────────────────────────────────────────────────────────────────────
// Fırsat Değerlendirme ve Yürütme
// ─────────────────────────────────────────────────────────────────────────────

/// Bulunan arbitraj fırsatını değerlendir, simüle et ve gerekirse yürüt
pub async fn evaluate_and_execute<T: Transport + Clone, P: Provider<T, Ethereum> + Sync>(
    _provider: &P,
    config: &BotConfig,
    pools: &[PoolConfig],
    states: &[SharedPoolState],
    opportunity: &ArbitrageOpportunity,
    sim_engine: &SimulationEngine,
    stats: &mut ArbitrageStats,
) {
    let buy_pool = &pools[opportunity.buy_pool_idx];
    let sell_pool = &pools[opportunity.sell_pool_idx];

    // ─── İstatistik Güncelle ─────────────────────────────────────
    stats.total_opportunities += 1;
    if opportunity.spread_pct > stats.max_spread_pct {
        stats.max_spread_pct = opportunity.spread_pct;
    }

    // ─── REVM Simülasyonu ──────────────────────────────────────
    let sim_result = sim_engine.validate_mathematical(
        pools,
        states,
        opportunity.buy_pool_idx,
        opportunity.sell_pool_idx,
        opportunity.optimal_amount_weth,
    );

    // Kontrat adresi varsa tam REVM simülasyonu da yap
    let _revm_result = if let Some(contract_addr) = config.contract_address {
        let amount_wei = U256::from((opportunity.optimal_amount_weth * 1e18) as u128);
        let min_profit_eth = opportunity.expected_profit_usd / opportunity.buy_price;
        let min_profit_wei = U256::from((min_profit_eth * 1e18) as u128);

        let calldata = crate::simulator::encode_execute_arbitrage(
            buy_pool.address,
            sell_pool.address,
            amount_wei,
            min_profit_wei,
        );

        let caller = config.private_key.as_ref()
            .and_then(|pk| pk.parse::<PrivateKeySigner>().ok())
            .map(|signer| signer.address())
            .unwrap_or_default();

        sim_engine.simulate(
            pools,
            states,
            caller,
            contract_addr,
            calldata,
            U256::ZERO,
        )
    } else {
        sim_result.clone()
    };

    // Simülasyon başarısız → işlemi atla
    if !sim_result.success {
        stats.failed_simulations += 1;
        print_simulation_failure(opportunity, &sim_result, pools);
        return;
    }

    // ─── KÂRLI FIRSAT RAPORU ─────────────────────────────────
    stats.profitable_opportunities += 1;
    stats.total_potential_profit += opportunity.expected_profit_usd;
    if opportunity.expected_profit_usd > stats.max_profit_usd {
        stats.max_profit_usd = opportunity.expected_profit_usd;
    }

    print_opportunity_report(opportunity, &sim_result, pools, config);

    // ─── KONTRAT TETİKLEME ────────────────────────────────────
    if config.execution_enabled() {
        let rpc_url = config.rpc_wss_url.clone();
        let pk = config.private_key.clone().unwrap();
        let contract_addr = config.contract_address.unwrap();
        let buy_addr = buy_pool.address;
        let sell_addr = sell_pool.address;
        let trade_weth = opportunity.optimal_amount_weth;
        let min_profit = opportunity.expected_profit_usd;
        let buy_price = opportunity.buy_price;

        stats.executed_trades += 1;

        tokio::spawn(async move {
            execute_on_chain(
                rpc_url, pk, contract_addr,
                buy_addr, sell_addr,
                trade_weth, min_profit, buy_price,
            ).await;
        });
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Kontrat Tetikleme (Zincir Üzeri)
// ─────────────────────────────────────────────────────────────────────────────

use alloy::providers::ProviderBuilder;
use alloy::sol;

sol! {
    #[sol(rpc)]
    contract IArbitrageExecutor {
        function executeArbitrage(
            address buyPool,
            address sellPool,
            uint256 amountIn,
            uint256 minProfit
        ) external returns (uint256 profit);
    }
}

/// Arbitraj kontratını zincir üzerinde tetikle
async fn execute_on_chain(
    rpc_url: String,
    private_key: String,
    contract_address: Address,
    buy_pool: Address,
    sell_pool: Address,
    trade_size_weth: f64,
    min_profit_usd: f64,
    buy_price: f64,
) {
    println!("\n  {} {}", "🚀".yellow(), "KONTRAT TETİKLEME BAŞLATILDI".yellow().bold());

    match execute_inner(
        &rpc_url, &private_key, contract_address,
        buy_pool, sell_pool,
        trade_size_weth, min_profit_usd, buy_price,
    ).await {
        Ok(hash) => {
            println!("  {} TX başarılı: {}", "✅".green(), hash.green().bold());
        }
        Err(e) => {
            println!("  {} TX hatası: {}", "❌".red(), format!("{}", e).red());
        }
    }
}

/// Kontrat tetikleme iç mantığı
async fn execute_inner(
    rpc_url: &str,
    private_key: &str,
    contract_address: Address,
    buy_pool: Address,
    sell_pool: Address,
    trade_size_weth: f64,
    min_profit_usd: f64,
    buy_price: f64,
) -> eyre::Result<String> {
    use alloy::providers::WsConnect;

    let signer: PrivateKeySigner = private_key
        .parse()
        .map_err(|_| eyre::eyre!("Geçersiz private key"))?;
    let wallet = EthereumWallet::from(signer);

    let ws = WsConnect::new(rpc_url);
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_ws(ws)
        .await
        .map_err(|e| eyre::eyre!("TX provider bağlantı hatası: {}", e))?;

    let amount_in_wei = U256::from((trade_size_weth * 1e18) as u128);
    let min_profit_eth = min_profit_usd / buy_price;
    let min_profit_wei = U256::from((min_profit_eth * 1e18) as u128);

    let contract = IArbitrageExecutor::new(contract_address, &provider);
    let call = contract.executeArbitrage(buy_pool, sell_pool, amount_in_wei, min_profit_wei);

    println!("  {} TX gönderiliyor... (miktar: {:.6} WETH)", "📤".yellow(), trade_size_weth);
    let pending = call.send().await.map_err(|e| eyre::eyre!("TX gönderme hatası: {}", e))?;
    let tx_hash = format!("{:?}", pending.tx_hash());
    println!("  {} TX yayınlandı: {}", "📡".blue(), &tx_hash);

    match tokio::time::timeout(Duration::from_secs(60), pending.get_receipt()).await {
        Ok(Ok(receipt)) => {
            println!(
                "  {} Blok: #{}",
                "✅".green(),
                receipt.block_number.unwrap_or_default()
            );
        }
        Ok(Err(e)) => println!("  {} Onay hatası: {}", "⚠️".yellow(), e),
        Err(_) => println!("  {} Zaman aşımı (60s)", "⏰".yellow()),
    }

    Ok(tx_hash)
}

// ─────────────────────────────────────────────────────────────────────────────
// Terminal Çıktıları
// ─────────────────────────────────────────────────────────────────────────────

/// Simülasyon hatası raporu
fn print_simulation_failure(
    opp: &ArbitrageOpportunity,
    sim: &SimulationResult,
    _pools: &[PoolConfig],
) {
    println!(
        "     {} [{}] REVM Simülasyon BAŞARISIZ | Spread: {:.4}% | Sebep: {}",
        "⚠️".yellow(),
        timestamp().dimmed(),
        opp.spread_pct,
        sim.error.as_deref().unwrap_or("Bilinmiyor").red(),
    );
}

/// Kârlı fırsat raporu
fn print_opportunity_report(
    opp: &ArbitrageOpportunity,
    sim: &SimulationResult,
    pools: &[PoolConfig],
    config: &BotConfig,
) {
    let buy = &pools[opp.buy_pool_idx];
    let sell = &pools[opp.sell_pool_idx];

    println!();
    println!("{}", "  ╔═══════════════════════════════════════════════════════════╗".red().bold());
    println!("{}", "  ║     🚨🚨🚨  KÂRLI ARBİTRAJ FIRSATI  🚨🚨🚨              ║".red().bold());
    println!("{}", "  ╠═══════════════════════════════════════════════════════════╣".red().bold());
    println!("  {}  Zaman            : {}", "║".red(), timestamp().white().bold());
    println!(
        "  {}  Yön              : {} → {}",
        "║".red(),
        format!("{}'dan AL ({:.2}$)", buy.name, opp.buy_price).green().bold(),
        format!("{}'e SAT ({:.2}$)", sell.name, opp.sell_price).red().bold(),
    );
    println!("  {}  Spread           : {:.4}%", "║".red(), opp.spread_pct);
    println!("  {}  ──────────────────────────────────────────────────────", "║".red());
    println!(
        "  {}  Optimal Miktar   : {} WETH (Newton-Raphson: {}i, {})",
        "║".red(),
        format!("{:.6}", opp.optimal_amount_weth).white().bold(),
        opp.nr_iterations,
        if opp.nr_converged { "yakınsadı".green() } else { "yakınsamadı".yellow() },
    );
    println!(
        "  {}  {} NET KÂR       : {:.4}$",
        "║".red(),
        "💰",
        format!("{:.4}", opp.expected_profit_usd).green().bold(),
    );
    println!(
        "  {}  REVM Simülasyon  : {} (Gas: {})",
        "║".red(),
        if sim.success { "BAŞARILI".green().bold() } else { "BAŞARISIZ".red().bold() },
        sim.gas_used,
    );

    if config.execution_enabled() {
        println!(
            "  {}  Durum            : {}",
            "║".red(),
            "🚀 KONTRAT TETİKLENİYOR...".yellow().bold()
        );
    } else {
        println!(
            "  {}  Durum            : {}",
            "║".red(),
            "👁 Gözlem Modu (tetikleme devre dışı)".dimmed()
        );
    }
    println!("{}", "  ╚═══════════════════════════════════════════════════════════╝".red().bold());
    println!();
}
