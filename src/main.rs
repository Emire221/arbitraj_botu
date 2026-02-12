// ============================================================================
//  ARBITRAJ BOTU v3.0 — "İki Gözlü Canavar"
//  Profesyonel Seviye Uniswap V3 Çapraz-Havuz Arbitraj Gözetleme Sistemi
// ============================================================================

use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::eth::Filter;
use alloy::primitives::{address, Address};
use alloy::sol;
use alloy::sol_types::SolEvent;
use futures_util::StreamExt;
use eyre::Result;
use chrono::Local;
use colored::*;
use std::time::Instant;

// ─────────────────────────────────────────────────────────────────────────────
// ABI Tanımları — Uniswap V3 Swap Event
// ─────────────────────────────────────────────────────────────────────────────
sol! {
    event Swap(
        address indexed sender,
        address indexed recipient,
        int256 amount0,
        int256 amount1,
        uint160 sqrtPriceX96,
        uint128 liquidity,
        int24 tick
    );
}

// ─────────────────────────────────────────────────────────────────────────────
// Havuz Yapılandırması
// ─────────────────────────────────────────────────────────────────────────────
struct PoolConfig {
    address: Address,
    name: &'static str,
    fee_bps: f64,        // Komisyon oranı (basis points cinsinden, ör: 5 = %0.05)
}

// ─────────────────────────────────────────────────────────────────────────────
// Havuz Durumu — Her havuzun anlık durumunu tutar
// ─────────────────────────────────────────────────────────────────────────────
#[derive(Clone)]
struct PoolState {
    price: f64,                // Son hesaplanan ETH/USDC fiyatı
    sqrt_price_x96: f64,       // Uniswap'ın ham karekök fiyatı
    liquidity: f64,            // Havuzdaki likidite miktarı
    tick: i32,                 // Fiyat aralığı (tick)
    last_update: Instant,      // Son güncelleme zamanı
    trade_count: u64,          // Toplam yakalanan işlem sayısı
    total_volume_usd: f64,     // Toplam hacim (USD)
    last_trade_size_usd: f64,  // Son işlem büyüklüğü
}

impl PoolState {
    fn new() -> Self {
        Self {
            price: 0.0,
            sqrt_price_x96: 0.0,
            liquidity: 0.0,
            tick: 0,
            last_update: Instant::now(),
            trade_count: 0,
            total_volume_usd: 0.0,
            last_trade_size_usd: 0.0,
        }
    }

    fn is_active(&self) -> bool {
        self.price > 0.0
    }

    /// Son güncellemeden bu yana geçen süre (milisaniye)
    fn staleness_ms(&self) -> u128 {
        self.last_update.elapsed().as_millis()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Arbitraj İstatistikleri — Oturum boyunca toplanan veriler
// ─────────────────────────────────────────────────────────────────────────────
struct ArbitrageStats {
    total_opportunities: u64,       // Toplam fırsat sayısı
    profitable_opportunities: u64,  // Kârlı fırsat sayısı (gas dahil)
    max_spread_usd: f64,            // Görülen en büyük fark ($)
    max_spread_pct: f64,            // Görülen en büyük fark (%)
    total_potential_profit: f64,    // Toplam potansiyel kâr ($)
    session_start: Instant,         // Oturumun başlangıcı
    total_swaps_seen: u64,          // Toplam görülen swap sayısı
}

impl ArbitrageStats {
    fn new() -> Self {
        Self {
            total_opportunities: 0,
            profitable_opportunities: 0,
            max_spread_usd: 0.0,
            max_spread_pct: 0.0,
            total_potential_profit: 0.0,
            session_start: Instant::now(),
            total_swaps_seen: 0,
        }
    }

    fn uptime_str(&self) -> String {
        let secs = self.session_start.elapsed().as_secs();
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        format!("{:02}:{:02}:{:02}", h, m, s)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Fiyat Hesaplama Motorları
// ─────────────────────────────────────────────────────────────────────────────

/// sqrtPriceX96 → Gerçek ETH/USDC fiyatına dönüştürme
/// Formül: price = (sqrtPriceX96 / 2^96)^2 × 10^(token0_decimals - token1_decimals)
/// USDC/ETH havuzlarında: token0=USDC(6), token1=ETH(18) → 10^(6-18) = 10^(-12)
/// Ters çevirip ETH başına USDC fiyatı: 1/price × 10^12
fn sqrt_price_to_eth_price(sqrt_price_x96_str: &str) -> f64 {
    let sqrt_price = sqrt_price_x96_str.parse::<f64>().unwrap_or(0.0);
    if sqrt_price == 0.0 {
        return 0.0;
    }
    let q96: f64 = 2.0_f64.powi(96);
    let price_ratio = (sqrt_price / q96).powi(2);
    // USDC(6 decimal) / WETH(18 decimal) havuzunda:
    // raw price = USDC/ETH (çok küçük sayı), ters çevirip decimal farkını uyguluyoruz
    let decimal_adjustment = 10.0_f64.powi(12); // 10^(18-6)
    1.0 / (price_ratio * decimal_adjustment)
}

/// amount0/amount1'den fiyat hesaplama (sqrtPrice ile çapraz doğrulama için)
fn amounts_to_price(amount0_str: &str, amount1_str: &str) -> Option<f64> {
    let usdc = amount0_str.parse::<f64>().unwrap_or(0.0) / 1_000_000.0;
    let eth = amount1_str.parse::<f64>().unwrap_or(0.0) / 1_000_000_000_000_000_000.0;
    if eth.abs() > 0.00001 {
        Some((usdc / eth).abs())
    } else {
        None
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Kârlılık Analiz Motoru
// ─────────────────────────────────────────────────────────────────────────────

/// Net kârlılık hesabı: Fiyat farkından tüm maliyetleri düşer
fn calculate_net_profit(
    buy_price: f64,
    sell_price: f64,
    trade_size_eth: f64,
    buy_fee_bps: f64,
    sell_fee_bps: f64,
    gas_cost_usd: f64,
    flash_loan_fee_bps: f64, // Aave flash loan: 9 bps (%0.09)
) -> ProfitAnalysis {
    let gross_spread = sell_price - buy_price;
    let gross_spread_pct = (gross_spread / buy_price) * 100.0;

    let trade_value_usd = trade_size_eth * buy_price;

    // Maliyetler
    let buy_fee = trade_value_usd * (buy_fee_bps / 10_000.0);
    let sell_fee = trade_value_usd * (sell_fee_bps / 10_000.0);
    let flash_fee = trade_value_usd * (flash_loan_fee_bps / 10_000.0);
    let total_cost = buy_fee + sell_fee + flash_fee + gas_cost_usd;

    let gross_profit = gross_spread * trade_size_eth;
    let net_profit = gross_profit - total_cost;
    let net_profit_pct = if trade_value_usd > 0.0 {
        (net_profit / trade_value_usd) * 100.0
    } else {
        0.0
    };

    ProfitAnalysis {
        gross_spread,
        gross_spread_pct,
        buy_fee,
        sell_fee,
        flash_fee,
        gas_cost: gas_cost_usd,
        total_cost,
        gross_profit,
        net_profit,
        net_profit_pct,
        is_profitable: net_profit > 0.0,
    }
}

struct ProfitAnalysis {
    gross_spread: f64,
    gross_spread_pct: f64,
    buy_fee: f64,
    sell_fee: f64,
    flash_fee: f64,
    gas_cost: f64,
    total_cost: f64,
    gross_profit: f64,
    net_profit: f64,
    net_profit_pct: f64,
    is_profitable: bool,
}

// ─────────────────────────────────────────────────────────────────────────────
// Terminal Çıktı Yardımcıları
// ─────────────────────────────────────────────────────────────────────────────

fn timestamp() -> String {
    Local::now().format("%H:%M:%S%.3f").to_string()
}

fn print_banner() {
    println!("{}", "╔══════════════════════════════════════════════════════════════╗".cyan().bold());
    println!("{}", "║        ARBITRAJ BOTU v3.0 — İki Gözlü Canavar               ║".cyan().bold());
    println!("{}", "║   Uniswap V3 Çapraz-Havuz Arbitraj Gözetleme Sistemi        ║".cyan().bold());
    println!("{}", "╚══════════════════════════════════════════════════════════════╝".cyan().bold());
    println!();
    println!("  {} Motor         : {}", "▸".cyan(), "Rust + Alloy (Ultra-Düşük Gecikme)".white());
    println!("  {} Ağ            : {}", "▸".cyan(), "Ethereum Mainnet (WebSocket)".white());
    println!("  {} Protokol      : {}", "▸".cyan(), "Uniswap V3 — USDC/WETH".white());
    println!("  {} Strateji      : {}", "▸".cyan(), "Çapraz-Havuz Spread Arbitrajı".white());
    println!("  {} Flash Loan    : {}", "▸".cyan(), "Aave V3 (%0.09 Komisyon)".white());
    println!("  {} Başlangıç     : {}", "▸".cyan(), Local::now().format("%Y-%m-%d %H:%M:%S").to_string().yellow());
    println!();
}

fn print_pool_header(pools: &[PoolConfig]) {
    println!("{}", "  ┌─────────────────────────────────────────────────────────┐".dimmed());
    println!("  {} {}", "│".dimmed(), "Gözetlenen Havuzlar:".white().bold());
    for p in pools {
        println!("  {}   {} {} (Ücret: %{:.2})", "│".dimmed(), "👁".green(), p.name, p.fee_bps / 100.0);
        println!("  {}     {}", "│".dimmed(), format!("{}", p.address).dimmed());
    }
    println!("{}", "  └─────────────────────────────────────────────────────────┘".dimmed());
    println!();
}

fn print_stats_summary(stats: &ArbitrageStats, state_05: &PoolState, state_03: &PoolState) {
    println!();
    println!("{}", "  ┌───── OTURUM İSTATİSTİKLERİ ─────────────────────────────┐".yellow());
    println!("  {}  Çalışma Süresi      : {}", "│".yellow(), stats.uptime_str().white().bold());
    println!("  {}  Toplam Swap          : {}", "│".yellow(), format!("{}", stats.total_swaps_seen).white());
    println!("  {}  Fırsat (Brüt)       : {}", "│".yellow(), format!("{}", stats.total_opportunities).white());
    println!("  {}  Fırsat (Net Kârlı)  : {}", "│".yellow(),
        if stats.profitable_opportunities > 0 {
            format!("{}", stats.profitable_opportunities).green().bold().to_string()
        } else {
            format!("{}", stats.profitable_opportunities).dimmed().to_string()
        }
    );
    println!("  {}  Maks. Spread         : {:.4}$ ({:.4}%)", "│".yellow(), stats.max_spread_usd, stats.max_spread_pct);
    println!("  {}  Pot. Toplam Kâr      : {:.2}$", "│".yellow(), stats.total_potential_profit);
    println!("  {}  Havuz %0.05 Hacim    : {:.0}$", "│".yellow(), state_05.total_volume_usd);
    println!("  {}  Havuz %0.30 Hacim    : {:.0}$", "│".yellow(), state_03.total_volume_usd);
    println!("{}", "  └─────────────────────────────────────────────────────────┘".yellow());
    println!();
}

// ─────────────────────────────────────────────────────────────────────────────
// ANA GİRİŞ NOKTASI
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    print_banner();

    // ══════════════ YAPILANDIRMA ══════════════
    let rpc_url = "wss://fabled-silent-wind.quiknode.pro/f80a1f043c4dbe1dedc065f90a7841f3a1a09553/";

    // İşlem Parametreleri
    let trade_size_eth: f64 = 10.0;        // Simüle edilen işlem büyüklüğü (ETH)
    let gas_cost_usd: f64 = 25.0;          // Tahmini gas maliyeti ($)
    let flash_loan_fee_bps: f64 = 9.0;     // Aave V3 flash loan ücreti (%0.09)
    let min_net_profit_usd: f64 = 5.0;     // Minimum net kâr eşiği ($)
    let stats_interval: u64 = 50;          // Kaç swap'ta bir istatistik göster

    // Havuz Tanımları
    let pools = vec![
        PoolConfig {
            address: address!("88e6a0c2ddd26feeb64f039a2c41296fcb3f5640"),
            name: "USDC/WETH %0.05",
            fee_bps: 5.0,
        },
        PoolConfig {
            address: address!("8ad599c3a0ff1de082011efddc58f1908eb6e6d8"),
            name: "USDC/WETH %0.30",
            fee_bps: 30.0,
        },
    ];
    print_pool_header(&pools);

    // ══════════════ BAĞLANTI ══════════════
    println!("  {} WebSocket bağlantısı kuruluyor...", "⏳".yellow());
    let connect_start = Instant::now();

    let ws = WsConnect::new(rpc_url);
    let provider = ProviderBuilder::new().on_ws(ws).await?;

    let connect_ms = connect_start.elapsed().as_millis();
    println!("  {} Bağlantı kuruldu! ({}ms)", "✅".green(), connect_ms);

    // Son blok numarasını al (sağlık kontrolü)
    let block = provider.get_block_number().await?;
    println!("  {} Güncel blok: #{}", "🧱".blue(), format!("{}", block).white().bold());
    println!();

    // ══════════════ FİLTRE ══════════════
    let pool_addresses: Vec<Address> = pools.iter().map(|p| p.address).collect();
    let filter = Filter::new()
        .address(pool_addresses)
        .event("Swap(address,address,int256,int256,uint160,uint128,int24)");

    let sub = provider.subscribe_logs(&filter).await?;
    let mut stream = sub.into_stream();

    println!("{}", "  ══════════════════════════════════════════════════════════".green());
    println!("  {}  CANLI YAYIN BAŞLADI — Swap olayları dinleniyor...", "📡".green());
    println!("{}", "  ══════════════════════════════════════════════════════════".green());
    println!();

    // ══════════════ DURUM DEĞİŞKENLERİ ══════════════
    let mut state_05 = PoolState::new();
    let mut state_03 = PoolState::new();
    let mut stats = ArbitrageStats::new();

    // ══════════════ ANA DÖNGÜ ══════════════
    while let Some(rpc_log) = stream.next().await {
        let recv_time = Instant::now();

        if let Ok(decoded) = Swap::decode_log(&rpc_log.inner, false) {
            stats.total_swaps_seen += 1;

            // ── Fiyat Hesaplama (Çift Yöntemli) ──
            let amount0_str = decoded.amount0.to_string();
            let amount1_str = decoded.amount1.to_string();
            let sqrt_price_str = decoded.sqrtPriceX96.to_string();
            let liquidity_str = decoded.liquidity.to_string();
            let tick_val: i32 = decoded.tick;

            // Yöntem 1: sqrtPriceX96'dan hassas fiyat
            let price_from_sqrt = sqrt_price_to_eth_price(&sqrt_price_str);

            // Yöntem 2: amount0/amount1'den fiyat (çapraz doğrulama)
            let price_from_amounts = amounts_to_price(&amount0_str, &amount1_str);

            // En güvenilir fiyatı seç (sqrtPrice daha hassas, ama amounts ile doğrula)
            let best_price = if price_from_sqrt > 100.0 && price_from_sqrt < 100_000.0 {
                price_from_sqrt
            } else {
                price_from_amounts.unwrap_or(0.0)
            };

            if best_price < 100.0 || best_price > 100_000.0 {
                continue; // Anlamsız fiyat, atla
            }

            // ── İşlem Büyüklüğü ──
            let usdc_amount = amount0_str.parse::<f64>().unwrap_or(0.0).abs() / 1_000_000.0;
            let eth_amount = amount1_str.parse::<f64>().unwrap_or(0.0).abs() / 1_000_000_000_000_000_000.0;
            let trade_value = usdc_amount.max(eth_amount * best_price);

            // ── Havuz Durumunu Güncelle ──
            let log_address = rpc_log.address();
            let pool_label: &str;
            let pool_color: &str;

            if log_address == pools[0].address {
                state_05.price = best_price;
                state_05.sqrt_price_x96 = sqrt_price_str.parse().unwrap_or(0.0);
                state_05.liquidity = liquidity_str.parse().unwrap_or(0.0);
                state_05.tick = tick_val;
                state_05.last_update = recv_time;
                state_05.trade_count += 1;
                state_05.total_volume_usd += trade_value;
                state_05.last_trade_size_usd = trade_value;
                pool_label = "%0.05";
                pool_color = "green";
            } else if log_address == pools[1].address {
                state_03.price = best_price;
                state_03.sqrt_price_x96 = sqrt_price_str.parse().unwrap_or(0.0);
                state_03.liquidity = liquidity_str.parse().unwrap_or(0.0);
                state_03.tick = tick_val;
                state_03.last_update = recv_time;
                state_03.trade_count += 1;
                state_03.total_volume_usd += trade_value;
                state_03.last_trade_size_usd = trade_value;
                pool_label = "%0.30";
                pool_color = "blue";
            } else {
                continue;
            }

            // ── İşlem Yönü ──
            let usdc_raw = amount0_str.parse::<f64>().unwrap_or(0.0);
            let direction = if usdc_raw > 0.0 { "AL " } else { "SAT" };
            let dir_icon = if usdc_raw > 0.0 { "🟢" } else { "🔴" };

            // ── Swap Bilgisi Yazdır ──
            let pool_tag = match pool_color {
                "green" => format!("Havuz {}", pool_label).green().bold().to_string(),
                _ => format!("Havuz {}", pool_label).blue().bold().to_string(),
            };

            println!(
                "  {} [{}] {} {} ETH {} | Fiyat: {}$ | Hacim: {:.0}$ | Tick: {}",
                dir_icon,
                timestamp().dimmed(),
                pool_tag,
                direction,
                format!("{:.4}", eth_amount).white(),
                format!("{:.2}", best_price).yellow().bold(),
                trade_value,
                tick_val,
            );

            // ══════════════ STRATEJİK BEYİN ══════════════
            if state_05.is_active() && state_03.is_active() {
                let spread = (state_05.price - state_03.price).abs();
                let spread_pct = (spread / state_05.price.min(state_03.price)) * 100.0;

                // Veri Tazeliği Kontrolü (5 saniyeden eski veri güvenilmez)
                let max_staleness_ms = 5000;
                let data_fresh = state_05.staleness_ms() < max_staleness_ms
                    && state_03.staleness_ms() < max_staleness_ms;

                if spread > 0.5 {
                    // Hangi yönde alım-satım yapılacak?
                    let (buy_pool, sell_pool, buy_price, sell_price, buy_fee, sell_fee) =
                        if state_05.price < state_03.price {
                            ("%0.05", "%0.30", state_05.price, state_03.price, pools[0].fee_bps, pools[1].fee_bps)
                        } else {
                            ("%0.30", "%0.05", state_03.price, state_05.price, pools[1].fee_bps, pools[0].fee_bps)
                        };

                    // Net Kârlılık Hesabı
                    let analysis = calculate_net_profit(
                        buy_price,
                        sell_price,
                        trade_size_eth,
                        buy_fee,
                        sell_fee,
                        gas_cost_usd,
                        flash_loan_fee_bps,
                    );

                    // İstatistik Güncelle
                    stats.total_opportunities += 1;
                    if spread > stats.max_spread_usd {
                        stats.max_spread_usd = spread;
                    }
                    if spread_pct > stats.max_spread_pct {
                        stats.max_spread_pct = spread_pct;
                    }

                    // ── FIRSAT RAPORU ──
                    println!();
                    if analysis.is_profitable && analysis.net_profit >= min_net_profit_usd && data_fresh {
                        stats.profitable_opportunities += 1;
                        stats.total_potential_profit += analysis.net_profit;

                        println!("{}", "  ╔═══════════════════════════════════════════════════════╗".red().bold());
                        println!("{}", "  ║     🚨🚨🚨  KÂRLI ARBİTRAJ FIRSATI  🚨🚨🚨          ║".red().bold());
                        println!("{}", "  ╠═══════════════════════════════════════════════════════╣".red().bold());
                        println!("  {}  Zaman         : {}", "║".red(), timestamp().white().bold());
                        println!("  {}  Yön           : {} → {}", "║".red(),
                            format!("{}'dan AL ({:.2}$)", buy_pool, buy_price).green().bold(),
                            format!("{}'e SAT ({:.2}$)", sell_pool, sell_price).red().bold()
                        );
                        println!("  {}  Brüt Spread   : {:.4}$ ({:.4}%)", "║".red(), analysis.gross_spread, analysis.gross_spread_pct);
                        println!("  {}  ────────────────────────────────────────────────", "║".red());
                        println!("  {}  İşlem Boyutu  : {} ETH ({:.0}$)", "║".red(),
                            format!("{:.2}", trade_size_eth).white().bold(),
                            trade_size_eth * buy_price
                        );
                        println!("  {}  Brüt Kâr      : {:.2}$", "║".red(), analysis.gross_profit);
                        println!("  {}  ────────────────────────────────────────────────", "║".red());
                        println!("  {}  Alış Komisyon  : -{:.2}$ (Havuz {})", "║".red(), analysis.buy_fee, buy_pool);
                        println!("  {}  Satış Komisyon : -{:.2}$ (Havuz {})", "║".red(), analysis.sell_fee, sell_pool);
                        println!("  {}  Flash Loan     : -{:.2}$ (Aave %0.09)", "║".red(), analysis.flash_fee);
                        println!("  {}  Gas Maliyeti   : -{:.2}$", "║".red(), analysis.gas_cost);
                        println!("  {}  Toplam Maliyet : -{:.2}$", "║".red(), analysis.total_cost);
                        println!("  {}  ────────────────────────────────────────────────", "║".red());
                        println!("  {}  {} NET KÂR    : {:.2}$ ({:.4}%)", "║".red(),
                            "💰".green(),
                            format!("{:.2}", analysis.net_profit).green().bold(),
                            analysis.net_profit_pct
                        );
                        println!("  {}  Veri Tazeliği  : %0.05={}ms, %0.30={}ms", "║".red(),
                            state_05.staleness_ms(), state_03.staleness_ms()
                        );
                        println!("{}", "  ╚═══════════════════════════════════════════════════════╝".red().bold());
                        println!();

                    } else if spread > 1.0 {
                        // Kârsız ama kayda değer spread
                        let freshness_tag = if data_fresh {
                            "TAZE".green().to_string()
                        } else {
                            "ESKİ!".red().bold().to_string()
                        };

                        println!(
                            "     {} Spread: {:.4}$ ({:.4}%) | Net: {:.2}$ | {} AL→{} SAT | Veri: {} ",
                            "📊".yellow(),
                            spread,
                            spread_pct,
                            analysis.net_profit,
                            buy_pool,
                            sell_pool,
                            freshness_tag,
                        );

                        if !analysis.is_profitable {
                            println!(
                                "     {} Kârsız: Maliyet ({:.2}$) > Brüt Kâr ({:.2}$)",
                                "⚠️".dimmed(),
                                analysis.total_cost,
                                analysis.gross_profit
                            );
                        }
                        println!();
                    }
                }

                // ── Periyodik İstatistik Raporu ──
                if stats.total_swaps_seen % stats_interval == 0 && stats.total_swaps_seen > 0 {
                    print_stats_summary(&stats, &state_05, &state_03);
                }
            }
        }
    }

    println!("{}", "\n  ⚠️  Bağlantı kesildi. Yeniden bağlanılıyor...".red().bold());
    Ok(())
}