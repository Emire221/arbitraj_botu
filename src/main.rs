// ============================================================================
//  ARBITRAJ BOTU v5.0 — "Kuantum Beyin"
//  Base Network Çapraz-DEX Arbitraj Sistemi
//
//  v5.0 Devrim Niteliğinde Yenilikler:
//  ✓ Yerel Durum Senkronizasyonu (Event/Mempool yerine State Sync)
//  ✓ REVM ile Yerel Simülasyon (eth_call yerine — 0 gecikme)
//  ✓ Newton-Raphson Optimal Hacim (Sabit TRADE_SIZE yerine — Dinamik)
//  ✓ Uniswap V3 + Aerodrome CL çapraz-DEX desteği
//  ✓ Modüler mimari (types, math, state_sync, simulator, strategy)
// ============================================================================

mod types;
mod math;
mod state_sync;
mod simulator;
mod strategy;

use types::*;
use state_sync::*;
use simulator::SimulationEngine;
use strategy::*;

use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use futures_util::StreamExt;
use eyre::Result;
use chrono::Local;
use colored::*;
use std::sync::Arc;
use std::time::{Duration, Instant};
use parking_lot::RwLock;

// ─────────────────────────────────────────────────────────────────────────────
// Terminal Çıktı Yardımcıları
// ─────────────────────────────────────────────────────────────────────────────

fn timestamp() -> String {
    Local::now().format("%H:%M:%S%.3f").to_string()
}

fn print_banner(config: &BotConfig) {
    println!();
    println!(
        "{}",
        "╔══════════════════════════════════════════════════════════════════╗"
            .cyan().bold()
    );
    println!(
        "{}",
        "║         ARBITRAJ BOTU v5.0 — Kuantum Beyin                     ║"
            .cyan().bold()
    );
    println!(
        "{}",
        "║    Base Network Çapraz-DEX Arbitraj Sistemi                     ║"
            .cyan().bold()
    );
    println!(
        "{}",
        "╠══════════════════════════════════════════════════════════════════╣"
            .cyan().bold()
    );
    println!(
        "{}",
        "║  [Faz 2] Yerel State Sync + REVM Simülasyon                    ║"
            .cyan()
    );
    println!(
        "{}",
        "║  [Faz 3] Newton-Raphson Optimal Hacim Hesaplama                ║"
            .cyan()
    );
    println!(
        "{}",
        "╚══════════════════════════════════════════════════════════════════╝"
            .cyan().bold()
    );
    println!();
    println!("  {} Motor          : {}", "▸".cyan(), "Rust + Alloy + REVM (Sıfır Gecikme)".white());
    println!("  {} Ağ             : {}", "▸".cyan(), format!("Base Network (Chain ID: {})", config.chain_id).white());
    println!("  {} Strateji       : {}", "▸".cyan(), "Çapraz-DEX Spread Arbitrajı (Uniswap V3 + Aerodrome)".white());
    println!("  {} Veri Kaynağı   : {}", "▸".cyan(), "Yerel State Sync (Blok bazlı — Event YOK)".white());
    println!("  {} Simülasyon     : {}", "▸".cyan(), "REVM (Yerel EVM — eth_call YOK)".white());
    println!("  {} Optimizasyon   : {}", "▸".cyan(), "Newton-Raphson (Sabit TRADE_SIZE YOK — Dinamik)".white());
    println!("  {} Flash Loan     : {}", "▸".cyan(), format!("Aave V3 (%{:.2} Komisyon)", config.flash_loan_fee_bps / 100.0).white());
    println!("  {} Maks İşlem     : {}", "▸".cyan(), format!("{:.1} WETH", config.max_trade_size_weth).white());
    println!("  {} Min. Net Kâr   : {}", "▸".cyan(), format!("{:.2}$", config.min_net_profit_usd).white());
    println!(
        "  {} Başlangıç      : {}",
        "▸".cyan(),
        Local::now().format("%Y-%m-%d %H:%M:%S").to_string().yellow()
    );
    println!(
        "  {} Mod            : {}",
        "▸".cyan(),
        if config.execution_enabled() {
            "CANLI (Kontrat Tetikleme Aktif)".green().bold().to_string()
        } else {
            "GÖZLEM (Sadece İzleme)".yellow().bold().to_string()
        }
    );
    println!();
}

fn print_pool_header(pools: &[PoolConfig]) {
    println!("{}", "  ┌──────────────────────────────────────────────────────────────┐".dimmed());
    println!("  {} {}", "│".dimmed(), "Gözetlenen Havuzlar:".white().bold());
    for (i, p) in pools.iter().enumerate() {
        let icon = if i == 0 { "🔵" } else { "🟣" };
        println!(
            "  {}   {} {} ({} — Ücret: %{:.2})",
            "│".dimmed(),
            icon,
            p.name,
            p.dex,
            p.fee_bps as f64 / 100.0
        );
        println!("  {}     {}", "│".dimmed(), format!("{}", p.address).dimmed());
    }
    println!("{}", "  └──────────────────────────────────────────────────────────────┘".dimmed());
    println!();
}

fn print_block_update(
    block_number: u64,
    pools: &[PoolConfig],
    states: &[SharedPoolState],
    sync_ms: u128,
) {
    let mut pool_info = String::new();
    for (i, (config, state_lock)) in pools.iter().zip(states.iter()).enumerate() {
        let state = state_lock.read();
        if state.is_active() {
            if i > 0 {
                pool_info.push_str(" | ");
            }
            let short_name = if config.name.len() > 12 {
                &config.name[..12]
            } else {
                &config.name
            };
            pool_info.push_str(&format!(
                "{}={:.2}$",
                short_name,
                state.eth_price_usd,
            ));
        }
    }

    println!(
        "  {} [{}] Blok #{} | {} | Senk: {}ms",
        "🧱".blue(),
        timestamp().dimmed(),
        format!("{}", block_number).white().bold(),
        pool_info,
        sync_ms,
    );
}

fn print_spread_info(pools: &[PoolConfig], states: &[SharedPoolState]) {
    if states.len() < 2 {
        return;
    }

    let state_a = states[0].read();
    let state_b = states[1].read();

    if !state_a.is_active() || !state_b.is_active() {
        return;
    }

    let spread = (state_a.eth_price_usd - state_b.eth_price_usd).abs();
    let min_price = state_a.eth_price_usd.min(state_b.eth_price_usd);
    let spread_pct = if min_price > 0.0 {
        (spread / min_price) * 100.0
    } else {
        0.0
    };

    if spread_pct > 0.001 {
        let direction = if state_a.eth_price_usd < state_b.eth_price_usd {
            format!("{} → {}", pools[0].name, pools[1].name)
        } else {
            format!("{} → {}", pools[1].name, pools[0].name)
        };

        if spread_pct > 0.05 {
            println!(
                "     {} Spread: {:.4}% ({:.4}$) | {} AL→SAT",
                "📊".yellow(), spread_pct, spread, direction,
            );
        } else {
            println!(
                "     {} Spread: {:.4}% ({:.4}$) | {}",
                "📊", spread_pct, spread, direction,
            );
        }
    }
}

fn print_stats_summary(stats: &ArbitrageStats, states: &[SharedPoolState]) {
    println!();
    println!("{}", "  ┌───── OTURUM İSTATİSTİKLERİ ─────────────────────────────────┐".yellow());
    println!("  {}  Çalışma Süresi       : {}", "│".yellow(), stats.uptime_str().white().bold());
    println!("  {}  İşlenen Blok         : {}", "│".yellow(), format!("{}", stats.total_blocks_processed).white());
    println!("  {}  Tespit Edilen Fırsat  : {}", "│".yellow(), format!("{}", stats.total_opportunities).white());
    println!(
        "  {}  Net Kârlı Fırsat     : {}",
        "│".yellow(),
        if stats.profitable_opportunities > 0 {
            format!("{}", stats.profitable_opportunities).green().bold().to_string()
        } else {
            format!("{}", stats.profitable_opportunities).dimmed().to_string()
        }
    );
    println!("  {}  Başarısız Simülasyon  : {}", "│".yellow(), stats.failed_simulations);
    println!(
        "  {}  Yürütülen İşlem      : {}",
        "│".yellow(),
        if stats.executed_trades > 0 {
            format!("{}", stats.executed_trades).green().bold().to_string()
        } else {
            format!("{}", stats.executed_trades).dimmed().to_string()
        }
    );
    println!("  {}  Maks. Spread          : {:.4}%", "│".yellow(), stats.max_spread_pct);
    println!("  {}  Maks. Kâr (tek)       : {:.4}$", "│".yellow(), stats.max_profit_usd);
    println!("  {}  Toplam Pot. Kâr       : {:.4}$", "│".yellow(), stats.total_potential_profit);

    for (i, state_lock) in states.iter().enumerate() {
        let state = state_lock.read();
        if state.is_active() {
            println!(
                "  {}  Havuz {} Fiyat       : {:.2}$ (tick: {})",
                "│".yellow(), i + 1, state.eth_price_usd, state.tick
            );
        }
    }

    println!("{}", "  └──────────────────────────────────────────────────────────────┘".yellow());
    println!();
}

// ─────────────────────────────────────────────────────────────────────────────
// ANA GİRİŞ NOKTASI — Yeniden Bağlanma Döngüsü
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // .env dosyasını yükle
    dotenvy::dotenv().ok();

    // Yapılandırmayı oku
    let config = BotConfig::from_env()?;

    // Havuz yapılandırmalarını oku
    let pools = load_pool_configs_from_env()?;

    // Banner göster
    print_banner(&config);
    print_pool_header(&pools);

    // Yeniden bağlanma döngüsü
    let mut retry_count: u32 = 0;
    let mut retry_delay = config.initial_retry_delay_secs;

    loop {
        if retry_count > 0 {
            println!(
                "  {} Yeniden bağlanma denemesi #{} ({} saniye beklendi)",
                "🔄".yellow(), retry_count, retry_delay
            );
        }

        match run_bot(&config, &pools).await {
            Ok(_) => {
                println!(
                    "\n  {} WebSocket bağlantısı kesildi. Yeniden bağlanılıyor...",
                    "⚠️".yellow()
                );
                retry_delay = config.initial_retry_delay_secs;
            }
            Err(e) => {
                println!(
                    "\n  {} Hata: {:#}",
                    "❌".red(), e
                );
            }
        }

        retry_count += 1;

        if config.max_retries > 0 && retry_count >= config.max_retries {
            println!(
                "  {} Maksimum deneme ({}) aşıldı. Bot kapatılıyor.",
                "🛑".red(), config.max_retries
            );
            return Err(eyre::eyre!("Maksimum yeniden bağlanma denemesi aşıldı"));
        }

        println!(
            "  {} {} saniye sonra tekrar denenecek...",
            "⏳".yellow(), retry_delay
        );
        tokio::time::sleep(Duration::from_secs(retry_delay)).await;
        retry_delay = (retry_delay * 2).min(config.max_retry_delay_secs);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BOT MOTORU — Blok Dinle → State Sync → Fırsat Tara → Simüle → Yürüt
// ─────────────────────────────────────────────────────────────────────────────

async fn run_bot(config: &BotConfig, pools: &[PoolConfig]) -> Result<()> {
    // ══════════════ BAĞLANTI ══════════════
    println!("  {} WebSocket bağlantısı kuruluyor...", "⏳".yellow());
    let connect_start = Instant::now();

    let ws = WsConnect::new(&config.rpc_wss_url);
    let provider = ProviderBuilder::new().on_ws(ws).await?;

    let connect_ms = connect_start.elapsed().as_millis();
    println!("  {} WSS bağlantı kuruldu! ({}ms)", "✅".green(), connect_ms);

    // Son blok
    let block = provider.get_block_number().await?;
    println!(
        "  {} Güncel blok: #{}",
        "🧱".blue(),
        format!("{}", block).white().bold()
    );

    // ══════════════ PAYLAŞIMLI DURUM ══════════════
    let states: Vec<SharedPoolState> = pools.iter()
        .map(|_| Arc::new(RwLock::new(PoolState::default())))
        .collect();

    // ══════════════ İLK SENKRONİZASYON ══════════════
    println!("\n  {} İlk durum senkronizasyonu yapılıyor...", "🔄".yellow());

    // Bytecode önbelleğe al (bir kez — REVM için)
    let bytecode_results = cache_all_bytecodes(&provider, pools, &states).await;
    for (i, result) in bytecode_results.iter().enumerate() {
        match result {
            Ok(_) => println!("  {}   {} bytecode önbelleğe alındı", "✅".green(), pools[i].name),
            Err(e) => println!("  {}   {} bytecode hatası: {}", "⚠️".yellow(), pools[i].name, e),
        }
    }

    // İlk state sync
    let sync_results = sync_all_pools(&provider, pools, &states, block).await;
    for (i, result) in sync_results.iter().enumerate() {
        match result {
            Ok(_) => {
                let state = states[i].read();
                println!(
                    "  {}   {} → {:.2}$ | Tick: {} | Likidite: {:.2e}",
                    "✅".green(),
                    pools[i].name,
                    state.eth_price_usd,
                    state.tick,
                    state.liquidity_f64,
                );
            }
            Err(e) => println!("  {}   {} state hatası: {}", "❌".red(), pools[i].name, e),
        }
    }

    // ══════════════ REVM SİMÜLASYON MOTORU ══════════════
    let mut sim_engine = SimulationEngine::new();
    sim_engine.cache_bytecodes(pools, &states);
    println!("\n  {} REVM simülasyon motoru hazır", "✅".green());

    // Execution modu
    if config.execution_enabled() {
        println!(
            "  {} Kontrat tetikleme: {} (Adres: {})",
            "🚀".green(),
            "AKTİF".green().bold(),
            config.contract_address.unwrap()
        );
    } else {
        println!(
            "  {} Kontrat tetikleme: {} (Sadece gözlem)",
            "ℹ️".blue(),
            "DEVRE DIŞI".yellow().bold()
        );
    }

    // ══════════════ BLOK BAŞLIĞI ABONELİĞİ ══════════════
    println!();
    println!("{}", "  ════════════════════════════════════════════════════════════════".green());
    println!("  {}  CANLI YAYIN BAŞLADI — Yeni bloklar dinleniyor...", "📡".green());
    println!("  {}  Döngü: State Sync → Fırsat Tara → Newton-Raphson → REVM → Yürüt", "📡".green());
    println!("{}", "  ════════════════════════════════════════════════════════════════".green());
    println!();

    let sub = provider.subscribe_blocks().await?;
    let mut stream = sub.into_stream();
    let mut stats = ArbitrageStats::new();

    // ══════════════ ANA DÖNGÜ — BLOK BAZLI ══════════════
    while let Some(block_header) = stream.next().await {
        let block_start = Instant::now();
        let block_number = block_header.header.number.unwrap_or(0);

        // ── 1. DURUM SENKRONİZASYONU ────────────────────────
        let sync_results = sync_all_pools(&provider, pools, &states, block_number).await;

        let sync_ms = block_start.elapsed().as_millis();
        let all_synced = sync_results.iter().all(|r| r.is_ok());

        // Hata raporlama
        for (i, result) in sync_results.iter().enumerate() {
            if let Err(e) = result {
                println!(
                    "  {} [Blok #{}] {} sync hatası: {}",
                    "⚠️".yellow(), block_number, pools[i].name, e
                );
            }
        }

        stats.total_blocks_processed += 1;

        // ── 2. BLOK + SPREAD BİLGİSİ ───────────────────────
        print_block_update(block_number, pools, &states, sync_ms);
        print_spread_info(pools, &states);

        // ── 3. ARBİTRAJ FIRSATI KONTROLÜ ────────────────────
        if all_synced {
            if let Some(opportunity) = check_arbitrage_opportunity(pools, &states, config) {
                // ── 4. DEĞERLENDİR + SİMÜLE + YÜRÜT ──────────
                evaluate_and_execute(
                    &provider,
                    config,
                    pools,
                    &states,
                    &opportunity,
                    &sim_engine,
                    &mut stats,
                ).await;
            }
        }

        // ── 5. PERİYODİK İSTATİSTİK ────────────────────────
        if stats.total_blocks_processed % config.stats_interval == 0
            && stats.total_blocks_processed > 0
        {
            print_stats_summary(&stats, &states);
        }
    }

    Ok(())
}
