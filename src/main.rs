// ============================================================================
//  ARBITRAJ BOTU v4.0 — "İki Gözlü Canavar"
//  Profesyonel Uniswap V3 Çapraz-Havuz Arbitraj Sistemi
//
//  v4.0 Yenilikler:
//  ✓ Otomatik yeniden bağlanma (exponential backoff)
//  ✓ .env tabanlı yapılandırma (güvenli & esnek)
//  ✓ Akıllı kontrat tetikleme altyapısı
// ============================================================================

use alloy::providers::{Provider, ProviderBuilder, WsConnect};
use alloy::rpc::types::eth::Filter;
use alloy::primitives::{address, Address, U256};
use alloy::sol;
use alloy::sol_types::SolEvent;
use alloy::signers::local::PrivateKeySigner;
use alloy::network::EthereumWallet;
use futures_util::StreamExt;
use eyre::{Result, eyre};
use chrono::Local;
use colored::*;
use std::time::{Duration, Instant};

// ─────────────────────────────────────────────────────────────────────────────
// ABI Tanımları
// ─────────────────────────────────────────────────────────────────────────────

// Uniswap V3 Swap Event
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

// Arbitraj Kontratı Arayüzü
// Not: Bu arayüz, deploy ettiğiniz arbitraj kontratının ABI'sine uygun olmalıdır.
// Kontrat adresi ve private key .env dosyasında tanımlanmalıdır.
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

// ─────────────────────────────────────────────────────────────────────────────
// Bot Yapılandırması (.env tabanlı)
// ─────────────────────────────────────────────────────────────────────────────

/// Tüm yapılandırma .env dosyasından okunur
struct BotConfig {
    /// QuickNode WebSocket RPC URL'si
    rpc_url: String,
    /// Cüzdan private key (opsiyonel — kontrat tetikleme için)
    private_key: Option<String>,
    /// Arbitraj kontrat adresi (opsiyonel — kontrat tetikleme için)
    contract_address: Option<Address>,
    /// İşlem büyüklüğü (ETH)
    trade_size_eth: f64,
    /// Tahmini gas maliyeti ($)
    gas_cost_usd: f64,
    /// Aave V3 flash loan ücreti (basis points)
    flash_loan_fee_bps: f64,
    /// Minimum net kâr eşiği ($)
    min_net_profit_usd: f64,
    /// Kaç swap'ta bir istatistik göster
    stats_interval: u64,
    /// Maksimum yeniden bağlanma denemesi (0 = sınırsız)
    max_retries: u32,
    /// Başlangıç bekleme süresi (saniye)
    initial_retry_delay_secs: u64,
    /// Maksimum bekleme süresi (saniye)
    max_retry_delay_secs: u64,
}

impl BotConfig {
    /// .env dosyasından yapılandırmayı oku
    fn from_env() -> Result<Self> {
        let rpc_url = std::env::var("QUICKNODE_WSS_URL")
            .map_err(|_| eyre!("QUICKNODE_WSS_URL .env dosyasında tanımlanmalıdır!"))?;

        if rpc_url.is_empty() || rpc_url == "wss://your-quicknode-url-here/" {
            return Err(eyre!("QUICKNODE_WSS_URL geçerli bir URL olmalıdır!"));
        }

        let private_key = std::env::var("PRIVATE_KEY")
            .ok()
            .filter(|pk| !pk.is_empty() && pk != "your-private-key-here");

        let contract_address = std::env::var("ARBITRAGE_CONTRACT_ADDRESS")
            .ok()
            .filter(|addr| !addr.is_empty() && addr != "0xYourContractAddress")
            .and_then(|addr| addr.parse::<Address>().ok());

        let trade_size_eth = std::env::var("TRADE_SIZE_ETH")
            .unwrap_or_else(|_| "10.0".into())
            .parse::<f64>()
            .unwrap_or(10.0);

        let gas_cost_usd = std::env::var("GAS_COST_USD")
            .unwrap_or_else(|_| "25.0".into())
            .parse::<f64>()
            .unwrap_or(25.0);

        let flash_loan_fee_bps = std::env::var("FLASH_LOAN_FEE_BPS")
            .unwrap_or_else(|_| "9.0".into())
            .parse::<f64>()
            .unwrap_or(9.0);

        let min_net_profit_usd = std::env::var("MIN_NET_PROFIT_USD")
            .unwrap_or_else(|_| "5.0".into())
            .parse::<f64>()
            .unwrap_or(5.0);

        let stats_interval = std::env::var("STATS_INTERVAL")
            .unwrap_or_else(|_| "50".into())
            .parse::<u64>()
            .unwrap_or(50);

        let max_retries = std::env::var("MAX_RETRIES")
            .unwrap_or_else(|_| "0".into())
            .parse::<u32>()
            .unwrap_or(0);

        Ok(Self {
            rpc_url,
            private_key,
            contract_address,
            trade_size_eth,
            gas_cost_usd,
            flash_loan_fee_bps,
            min_net_profit_usd,
            stats_interval,
            max_retries,
            initial_retry_delay_secs: 2,
            max_retry_delay_secs: 60,
        })
    }

    /// Kontrat tetikleme modu aktif mi?
    fn execution_enabled(&self) -> bool {
        self.private_key.is_some() && self.contract_address.is_some()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Havuz Yapılandırması
// ─────────────────────────────────────────────────────────────────────────────

struct PoolConfig {
    address: Address,
    name: &'static str,
    fee_bps: f64,
}

// ─────────────────────────────────────────────────────────────────────────────
// Havuz Durumu — Her havuzun anlık durumunu tutar
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Clone)]
struct PoolState {
    price: f64,
    sqrt_price_x96: f64,
    liquidity: f64,
    tick: i32,
    last_update: Instant,
    trade_count: u64,
    total_volume_usd: f64,
    last_trade_size_usd: f64,
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

    fn staleness_ms(&self) -> u128 {
        self.last_update.elapsed().as_millis()
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Arbitraj İstatistikleri
// ─────────────────────────────────────────────────────────────────────────────

struct ArbitrageStats {
    total_opportunities: u64,
    profitable_opportunities: u64,
    max_spread_usd: f64,
    max_spread_pct: f64,
    total_potential_profit: f64,
    session_start: Instant,
    total_swaps_seen: u64,
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
// Kârlılık Analiz Motoru
// ─────────────────────────────────────────────────────────────────────────────

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

fn calculate_net_profit(
    buy_price: f64,
    sell_price: f64,
    trade_size_eth: f64,
    buy_fee_bps: f64,
    sell_fee_bps: f64,
    gas_cost_usd: f64,
    flash_loan_fee_bps: f64,
) -> ProfitAnalysis {
    let gross_spread = sell_price - buy_price;
    let gross_spread_pct = (gross_spread / buy_price) * 100.0;
    let trade_value_usd = trade_size_eth * buy_price;

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

// ─────────────────────────────────────────────────────────────────────────────
// Fiyat Hesaplama Motorları
// ─────────────────────────────────────────────────────────────────────────────

fn sqrt_price_to_eth_price(sqrt_price_x96_str: &str) -> f64 {
    let sqrt_price = sqrt_price_x96_str.parse::<f64>().unwrap_or(0.0);
    if sqrt_price == 0.0 {
        return 0.0;
    }
    let q96: f64 = 2.0_f64.powi(96);
    let price_ratio = (sqrt_price / q96).powi(2);
    let decimal_adjustment = 10.0_f64.powi(12);
    1.0 / (price_ratio * decimal_adjustment)
}

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
// Kontrat Tetikleme Motoru
// ─────────────────────────────────────────────────────────────────────────────

/// Arbitraj kontratını zincir üzerinde tetikler.
/// Bu fonksiyon tokio::spawn ile arka planda çalıştırılır — ana döngüyü bloklamaz.
async fn execute_arbitrage_on_chain(
    rpc_url: String,
    private_key: String,
    contract_address: Address,
    buy_pool_addr: Address,
    sell_pool_addr: Address,
    trade_size_eth: f64,
    min_profit_usd: f64,
    buy_price: f64,
) {
    println!();
    println!("  {} {}", "🚀".yellow(), "KONTRAT TETİKLEME BAŞLATILDI".yellow().bold());
    println!("  {}   Alış Havuzu  : {}", "→".dimmed(), buy_pool_addr);
    println!("  {}   Satış Havuzu : {}", "→".dimmed(), sell_pool_addr);
    println!("  {}   Miktar       : {} ETH", "→".dimmed(), trade_size_eth);

    match execute_arbitrage_inner(
        &rpc_url,
        &private_key,
        contract_address,
        buy_pool_addr,
        sell_pool_addr,
        trade_size_eth,
        min_profit_usd,
        buy_price,
    )
    .await
    {
        Ok(tx_hash) => {
            println!(
                "  {} Arbitraj TX başarılı! Hash: {}",
                "✅".green(),
                tx_hash.green().bold()
            );
        }
        Err(e) => {
            println!(
                "  {} Kontrat tetikleme hatası: {}",
                "❌".red(),
                format!("{}", e).red()
            );
        }
    }
    println!();
}

/// Kontrat tetikleme iç mantığı (hata yönetimi dışarıda)
async fn execute_arbitrage_inner(
    rpc_url: &str,
    private_key: &str,
    contract_address: Address,
    buy_pool_addr: Address,
    sell_pool_addr: Address,
    trade_size_eth: f64,
    min_profit_usd: f64,
    buy_price: f64,
) -> Result<String> {
    // Signer oluştur
    let signer: PrivateKeySigner = private_key
        .parse()
        .map_err(|_| eyre!("Private key formatı geçersiz"))?;
    let wallet = EthereumWallet::from(signer);

    // Signer'lı provider oluştur (ayrı bağlantı — ana stream'i etkilemez)
    let ws = WsConnect::new(rpc_url);
    let provider = ProviderBuilder::new()
        .with_recommended_fillers()
        .wallet(wallet)
        .on_ws(ws)
        .await
        .map_err(|e| eyre!("TX provider bağlantı hatası: {}", e))?;

    // Wei cinsinden miktarlar
    let amount_in_wei = U256::from((trade_size_eth * 1e18) as u128);
    let min_profit_eth = min_profit_usd / buy_price;
    let min_profit_wei = U256::from((min_profit_eth * 1e18) as u128);

    println!(
        "  {}   Amount In    : {} wei",
        "→".dimmed(),
        amount_in_wei
    );
    println!(
        "  {}   Min Profit   : {} wei ({:.4} ETH)",
        "→".dimmed(),
        min_profit_wei,
        min_profit_eth
    );

    // Kontrat instance oluştur ve çağır
    let contract = IArbitrageExecutor::new(contract_address, &provider);
    let call_builder = contract.executeArbitrage(
        buy_pool_addr,
        sell_pool_addr,
        amount_in_wei,
        min_profit_wei,
    );

    println!("  {} TX gönderiliyor...", "📤".yellow());
    let pending_tx = call_builder
        .send()
        .await
        .map_err(|e| eyre!("TX gönderme hatası: {}", e))?;

    let tx_hash = format!("{:?}", pending_tx.tx_hash());
    println!("  {} TX yayınlandı: {}", "📡".blue(), &tx_hash);

    // TX onayını bekle (timeout: 120 saniye)
    match tokio::time::timeout(Duration::from_secs(120), pending_tx.get_receipt()).await {
        Ok(Ok(receipt)) => {
            println!(
                "  {} TX onaylandı! Blok: #{}",
                "✅".green(),
                receipt.block_number.unwrap_or_default()
            );
        }
        Ok(Err(e)) => {
            println!("  {} TX onay hatası: {}", "⚠️".yellow(), e);
        }
        Err(_) => {
            println!("  {} TX onay zaman aşımı (120s)", "⏰".yellow());
        }
    }

    Ok(tx_hash)
}

// ─────────────────────────────────────────────────────────────────────────────
// Terminal Çıktı Yardımcıları
// ─────────────────────────────────────────────────────────────────────────────

fn timestamp() -> String {
    Local::now().format("%H:%M:%S%.3f").to_string()
}

fn print_banner(config: &BotConfig) {
    println!(
        "{}",
        "╔══════════════════════════════════════════════════════════════╗"
            .cyan()
            .bold()
    );
    println!(
        "{}",
        "║        ARBITRAJ BOTU v4.0 — İki Gözlü Canavar               ║"
            .cyan()
            .bold()
    );
    println!(
        "{}",
        "║   Uniswap V3 Çapraz-Havuz Arbitraj Sistemi                  ║"
            .cyan()
            .bold()
    );
    println!(
        "{}",
        "╚══════════════════════════════════════════════════════════════╝"
            .cyan()
            .bold()
    );
    println!();
    println!(
        "  {} Motor         : {}",
        "▸".cyan(),
        "Rust + Alloy (Ultra-Düşük Gecikme)".white()
    );
    println!(
        "  {} Ağ            : {}",
        "▸".cyan(),
        "Ethereum Mainnet (WebSocket)".white()
    );
    println!(
        "  {} Protokol      : {}",
        "▸".cyan(),
        "Uniswap V3 — USDC/WETH".white()
    );
    println!(
        "  {} Strateji      : {}",
        "▸".cyan(),
        "Çapraz-Havuz Spread Arbitrajı".white()
    );
    println!(
        "  {} Flash Loan    : {}",
        "▸".cyan(),
        "Aave V3 (%0.09 Komisyon)".white()
    );
    println!(
        "  {} Başlangıç     : {}",
        "▸".cyan(),
        Local::now()
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
            .yellow()
    );
    println!(
        "  {} Mod           : {}",
        "▸".cyan(),
        if config.execution_enabled() {
            "CANLI (Kontrat Tetikleme Aktif)".green().bold().to_string()
        } else {
            "GÖZLEM (Sadece İzleme)".yellow().bold().to_string()
        }
    );
    println!(
        "  {} Yeniden Bağ.  : {}",
        "▸".cyan(),
        "Otomatik (Exponential Backoff)".white()
    );
    println!();
}

fn print_pool_header(pools: &[PoolConfig]) {
    println!(
        "{}",
        "  ┌─────────────────────────────────────────────────────────┐".dimmed()
    );
    println!(
        "  {} {}",
        "│".dimmed(),
        "Gözetlenen Havuzlar:".white().bold()
    );
    for p in pools {
        println!(
            "  {}   {} {} (Ücret: %{:.2})",
            "│".dimmed(),
            "👁".green(),
            p.name,
            p.fee_bps / 100.0
        );
        println!("  {}     {}", "│".dimmed(), format!("{}", p.address).dimmed());
    }
    println!(
        "{}",
        "  └─────────────────────────────────────────────────────────┘".dimmed()
    );
    println!();
}

fn print_stats_summary(stats: &ArbitrageStats, state_05: &PoolState, state_03: &PoolState) {
    println!();
    println!(
        "{}",
        "  ┌───── OTURUM İSTATİSTİKLERİ ─────────────────────────────┐".yellow()
    );
    println!(
        "  {}  Çalışma Süresi      : {}",
        "│".yellow(),
        stats.uptime_str().white().bold()
    );
    println!(
        "  {}  Toplam Swap          : {}",
        "│".yellow(),
        format!("{}", stats.total_swaps_seen).white()
    );
    println!(
        "  {}  Fırsat (Brüt)       : {}",
        "│".yellow(),
        format!("{}", stats.total_opportunities).white()
    );
    println!(
        "  {}  Fırsat (Net Kârlı)  : {}",
        "│".yellow(),
        if stats.profitable_opportunities > 0 {
            format!("{}", stats.profitable_opportunities)
                .green()
                .bold()
                .to_string()
        } else {
            format!("{}", stats.profitable_opportunities)
                .dimmed()
                .to_string()
        }
    );
    println!(
        "  {}  Maks. Spread         : {:.4}$ ({:.4}%)",
        "│".yellow(),
        stats.max_spread_usd,
        stats.max_spread_pct
    );
    println!(
        "  {}  Pot. Toplam Kâr      : {:.2}$",
        "│".yellow(),
        stats.total_potential_profit
    );
    println!(
        "  {}  Havuz %0.05 Hacim    : {:.0}$",
        "│".yellow(),
        state_05.total_volume_usd
    );
    println!(
        "  {}  Havuz %0.30 Hacim    : {:.0}$",
        "│".yellow(),
        state_03.total_volume_usd
    );
    println!(
        "{}",
        "  └─────────────────────────────────────────────────────────┘".yellow()
    );
    println!();
}

// ─────────────────────────────────────────────────────────────────────────────
// ANA GİRİŞ NOKTASI — Yeniden Bağlanma Döngüsü
// ─────────────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // .env dosyasını yükle
    dotenvy::dotenv().ok();

    // Yapılandırmayı .env'den oku
    let config = BotConfig::from_env()?;

    print_banner(&config);

    let mut retry_count: u32 = 0;
    let mut retry_delay = config.initial_retry_delay_secs;

    // ══════════════ YENİDEN BAĞLANMA DÖNGÜSÜ ══════════════
    loop {
        if retry_count > 0 {
            println!(
                "  {} Yeniden bağlanma denemesi #{} ({} saniye beklendi)",
                "🔄".yellow(),
                retry_count,
                retry_delay
            );
        }

        match run_bot(&config).await {
            Ok(_) => {
                // Stream normal şekilde sona erdi (sunucu bağlantıyı kapattı)
                println!(
                    "\n  {} WebSocket stream sona erdi. Yeniden bağlanılıyor...",
                    "⚠️".yellow()
                );
                // Başarılı bağlantı sonrasında delay'i sıfırla
                retry_delay = config.initial_retry_delay_secs;
            }
            Err(e) => {
                println!(
                    "\n  {} Bağlantı hatası: {}",
                    "❌".red(),
                    format!("{:#}", e).red()
                );
            }
        }

        retry_count += 1;

        // Maksimum deneme kontrolü (0 = sınırsız)
        if config.max_retries > 0 && retry_count >= config.max_retries {
            println!(
                "  {} Maksimum yeniden bağlanma denemesi ({}) aşıldı. Bot kapatılıyor.",
                "🛑".red(),
                config.max_retries
            );
            return Err(eyre!("Maksimum yeniden bağlanma denemesi aşıldı"));
        }

        // Exponential backoff ile bekleme
        println!(
            "  {} {} saniye sonra tekrar denenecek...",
            "⏳".yellow(),
            retry_delay
        );
        tokio::time::sleep(Duration::from_secs(retry_delay)).await;

        // Exponential backoff: 2 → 4 → 8 → 16 → 32 → 60 (max)
        retry_delay = (retry_delay * 2).min(config.max_retry_delay_secs);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// BOT MOTORU — Bağlantı kur, event'leri dinle, fırsatları tespit et
// ─────────────────────────────────────────────────────────────────────────────

async fn run_bot(config: &BotConfig) -> Result<()> {
    // ══════════════ BAĞLANTI ══════════════
    println!(
        "  {} WebSocket bağlantısı kuruluyor...",
        "⏳".yellow()
    );
    let connect_start = Instant::now();

    let ws = WsConnect::new(&config.rpc_url);
    let provider = ProviderBuilder::new().on_ws(ws).await?;

    let connect_ms = connect_start.elapsed().as_millis();
    println!(
        "  {} Bağlantı kuruldu! ({}ms)",
        "✅".green(),
        connect_ms
    );

    // Son blok numarasını al (sağlık kontrolü)
    let block = provider.get_block_number().await?;
    println!(
        "  {} Güncel blok: #{}",
        "🧱".blue(),
        format!("{}", block).white().bold()
    );
    println!();

    // ══════════════ HAVUZ YAPILANDIRMASI ══════════════
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

    // Execution modu bilgisi
    if config.execution_enabled() {
        println!(
            "  {} Kontrat tetikleme: {} (Adres: {})",
            "🚀".green(),
            "AKTİF".green().bold(),
            config.contract_address.unwrap()
        );
    } else {
        println!(
            "  {} Kontrat tetikleme: {} (Sadece gözlem modu)",
            "ℹ️".blue(),
            "DEVRE DIŞI".yellow().bold()
        );
        println!(
            "  {}   .env dosyasında PRIVATE_KEY ve ARBITRAGE_CONTRACT_ADDRESS ayarlayarak aktifleştirin.",
            " ".normal()
        );
    }
    println!();

    // ══════════════ FİLTRE ══════════════
    let pool_addresses: Vec<Address> = pools.iter().map(|p| p.address).collect();
    let filter = Filter::new()
        .address(pool_addresses)
        .event("Swap(address,address,int256,int256,uint160,uint128,int24)");

    let sub = provider.subscribe_logs(&filter).await?;
    let mut stream = sub.into_stream();

    println!(
        "{}",
        "  ══════════════════════════════════════════════════════════".green()
    );
    println!(
        "  {}  CANLI YAYIN BAŞLADI — Swap olayları dinleniyor...",
        "📡".green()
    );
    println!(
        "{}",
        "  ══════════════════════════════════════════════════════════".green()
    );
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

            let price_from_sqrt = sqrt_price_to_eth_price(&sqrt_price_str);
            let price_from_amounts = amounts_to_price(&amount0_str, &amount1_str);

            let best_price = if price_from_sqrt > 100.0 && price_from_sqrt < 100_000.0 {
                price_from_sqrt
            } else {
                price_from_amounts.unwrap_or(0.0)
            };

            if best_price < 100.0 || best_price > 100_000.0 {
                continue;
            }

            // ── İşlem Büyüklüğü ──
            let usdc_amount = amount0_str.parse::<f64>().unwrap_or(0.0).abs() / 1_000_000.0;
            let eth_amount =
                amount1_str.parse::<f64>().unwrap_or(0.0).abs() / 1_000_000_000_000_000_000.0;
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

                let max_staleness_ms = 5000;
                let data_fresh = state_05.staleness_ms() < max_staleness_ms
                    && state_03.staleness_ms() < max_staleness_ms;

                if spread > 0.5 {
                    // Hangi yönde alım-satım yapılacak?
                    let (buy_pool, sell_pool, buy_price, sell_price, buy_fee, sell_fee, buy_pool_addr, sell_pool_addr) =
                        if state_05.price < state_03.price {
                            (
                                "%0.05", "%0.30",
                                state_05.price, state_03.price,
                                pools[0].fee_bps, pools[1].fee_bps,
                                pools[0].address, pools[1].address,
                            )
                        } else {
                            (
                                "%0.30", "%0.05",
                                state_03.price, state_05.price,
                                pools[1].fee_bps, pools[0].fee_bps,
                                pools[1].address, pools[0].address,
                            )
                        };

                    let analysis = calculate_net_profit(
                        buy_price,
                        sell_price,
                        config.trade_size_eth,
                        buy_fee,
                        sell_fee,
                        config.gas_cost_usd,
                        config.flash_loan_fee_bps,
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
                    if analysis.is_profitable
                        && analysis.net_profit >= config.min_net_profit_usd
                        && data_fresh
                    {
                        stats.profitable_opportunities += 1;
                        stats.total_potential_profit += analysis.net_profit;

                        println!();
                        println!(
                            "{}",
                            "  ╔═══════════════════════════════════════════════════════╗"
                                .red()
                                .bold()
                        );
                        println!(
                            "{}",
                            "  ║     🚨🚨🚨  KÂRLI ARBİTRAJ FIRSATI  🚨🚨🚨          ║"
                                .red()
                                .bold()
                        );
                        println!(
                            "{}",
                            "  ╠═══════════════════════════════════════════════════════╣"
                                .red()
                                .bold()
                        );
                        println!(
                            "  {}  Zaman         : {}",
                            "║".red(),
                            timestamp().white().bold()
                        );
                        println!(
                            "  {}  Yön           : {} → {}",
                            "║".red(),
                            format!("{}'dan AL ({:.2}$)", buy_pool, buy_price)
                                .green()
                                .bold(),
                            format!("{}'e SAT ({:.2}$)", sell_pool, sell_price)
                                .red()
                                .bold()
                        );
                        println!(
                            "  {}  Brüt Spread   : {:.4}$ ({:.4}%)",
                            "║".red(),
                            analysis.gross_spread,
                            analysis.gross_spread_pct
                        );
                        println!(
                            "  {}  ────────────────────────────────────────────────",
                            "║".red()
                        );
                        println!(
                            "  {}  İşlem Boyutu  : {} ETH ({:.0}$)",
                            "║".red(),
                            format!("{:.2}", config.trade_size_eth).white().bold(),
                            config.trade_size_eth * buy_price
                        );
                        println!(
                            "  {}  Brüt Kâr      : {:.2}$",
                            "║".red(),
                            analysis.gross_profit
                        );
                        println!(
                            "  {}  ────────────────────────────────────────────────",
                            "║".red()
                        );
                        println!(
                            "  {}  Alış Komisyon  : -{:.2}$ (Havuz {})",
                            "║".red(),
                            analysis.buy_fee,
                            buy_pool
                        );
                        println!(
                            "  {}  Satış Komisyon : -{:.2}$ (Havuz {})",
                            "║".red(),
                            analysis.sell_fee,
                            sell_pool
                        );
                        println!(
                            "  {}  Flash Loan     : -{:.2}$ (Aave %0.09)",
                            "║".red(),
                            analysis.flash_fee
                        );
                        println!(
                            "  {}  Gas Maliyeti   : -{:.2}$",
                            "║".red(),
                            analysis.gas_cost
                        );
                        println!(
                            "  {}  Toplam Maliyet : -{:.2}$",
                            "║".red(),
                            analysis.total_cost
                        );
                        println!(
                            "  {}  ────────────────────────────────────────────────",
                            "║".red()
                        );
                        println!(
                            "  {}  {} NET KÂR    : {:.2}$ ({:.4}%)",
                            "║".red(),
                            "💰",
                            format!("{:.2}", analysis.net_profit).green().bold(),
                            analysis.net_profit_pct
                        );
                        println!(
                            "  {}  Veri Tazeliği  : %0.05={}ms, %0.30={}ms",
                            "║".red(),
                            state_05.staleness_ms(),
                            state_03.staleness_ms()
                        );

                        // ── KONTRAT TETİKLEME ──
                        if config.execution_enabled() {
                            println!(
                                "  {}  Durum          : {}",
                                "║".red(),
                                "🚀 KONTRAT TETİKLENİYOR...".yellow().bold()
                            );
                        } else {
                            println!(
                                "  {}  Durum          : {}",
                                "║".red(),
                                "👁 Gözlem Modu (tetikleme devre dışı)".dimmed()
                            );
                        }

                        println!(
                            "{}",
                            "  ╚═══════════════════════════════════════════════════════╝"
                                .red()
                                .bold()
                        );
                        println!();

                        // ── KONTRAT TETİKLEME (arka planda) ──
                        if let (Some(ref pk), Some(contract_addr)) =
                            (&config.private_key, config.contract_address)
                        {
                            let rpc = config.rpc_url.clone();
                            let pk = pk.clone();
                            let trade_eth = config.trade_size_eth;
                            let min_profit = config.min_net_profit_usd;

                            tokio::spawn(execute_arbitrage_on_chain(
                                rpc,
                                pk,
                                contract_addr,
                                buy_pool_addr,
                                sell_pool_addr,
                                trade_eth,
                                min_profit,
                                buy_price,
                            ));
                        }
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
                if stats.total_swaps_seen % config.stats_interval == 0
                    && stats.total_swaps_seen > 0
                {
                    print_stats_summary(&stats, &state_05, &state_03);
                }
            }
        }
    }

    // Stream sona erdi — reconnection loop yeniden bağlanacak
    Ok(())
}
