// ============================================================================
//  TYPES — Paylaşılan Tipler, Yapılandırma ve İstatistikler
//  Arbitraj Botu v5.0 — Base Network
// ============================================================================

use alloy::primitives::{Address, U256};
use eyre::Result;
use std::time::Instant;
use std::sync::Arc;
use parking_lot::RwLock;

// ─────────────────────────────────────────────────────────────────────────────
// DEX Türü
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DexType {
    UniswapV3,
    Aerodrome,
}

impl std::fmt::Display for DexType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DexType::UniswapV3 => write!(f, "Uniswap V3"),
            DexType::Aerodrome => write!(f, "Aerodrome"),
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Havuz Yapılandırması
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub address: Address,
    pub name: String,
    pub fee_bps: u32,
    pub fee_fraction: f64,
    pub token0_decimals: u8,
    pub token1_decimals: u8,
    pub dex: DexType,
}

// ─────────────────────────────────────────────────────────────────────────────
// Havuz Anlık Durumu (RAM'de tutulur)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct PoolState {
    /// sqrtPriceX96 (ham U256 değer)
    pub sqrt_price_x96: U256,
    /// sqrtPriceX96 float versiyonu (hızlı hesap için)
    pub sqrt_price_f64: f64,
    /// Mevcut tick
    pub tick: i32,
    /// Anlık likidite (u128)
    pub liquidity: u128,
    /// Likidite float versiyonu (hızlı hesap için)
    pub liquidity_f64: f64,
    /// ETH fiyatı (USDC cinsinden) — ör: 2500.45
    pub eth_price_usd: f64,
    /// Son güncellenen blok numarası
    pub last_block: u64,
    /// Son güncelleme zamanı (yerel)
    pub last_update: Instant,
    /// Havuz başlatıldı mı?
    pub is_initialized: bool,
    /// Havuz bytecode'u (REVM için önbellek)
    pub bytecode: Option<Vec<u8>>,
}

impl Default for PoolState {
    fn default() -> Self {
        Self {
            sqrt_price_x96: U256::ZERO,
            sqrt_price_f64: 0.0,
            tick: 0,
            liquidity: 0,
            liquidity_f64: 0.0,
            eth_price_usd: 0.0,
            last_block: 0,
            last_update: Instant::now(),
            is_initialized: false,
            bytecode: None,
        }
    }
}

impl PoolState {
    /// Havuz aktif mi? (veriler geçerli mi?)
    pub fn is_active(&self) -> bool {
        self.is_initialized && self.eth_price_usd > 0.0 && self.liquidity > 0
    }

    /// Verinin yaşı (milisaniye)
    pub fn staleness_ms(&self) -> u128 {
        self.last_update.elapsed().as_millis()
    }
}

/// Thread-safe havuz durumu
pub type SharedPoolState = Arc<RwLock<PoolState>>;

// ─────────────────────────────────────────────────────────────────────────────
// Arbitraj Fırsatı
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ArbitrageOpportunity {
    /// Ucuz havuz indeksi (buradan al)
    pub buy_pool_idx: usize,
    /// Pahalı havuz indeksi (buraya sat)
    pub sell_pool_idx: usize,
    /// Newton-Raphson ile hesaplanan optimal WETH miktarı
    pub optimal_amount_weth: f64,
    /// Beklenen net kâr (USD)
    pub expected_profit_usd: f64,
    /// Alış fiyatı (ucuz havuz ETH/USDC)
    pub buy_price: f64,
    /// Satış fiyatı (pahalı havuz ETH/USDC)  
    pub sell_price: f64,
    /// Spread yüzdesi
    pub spread_pct: f64,
    /// Newton-Raphson yakınsadı mı?
    pub nr_converged: bool,
    /// Newton-Raphson iterasyon sayısı
    pub nr_iterations: u32,
}

// ─────────────────────────────────────────────────────────────────────────────
// REVM Simülasyon Sonucu
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SimulationResult {
    /// Simülasyon başarılı mı?
    pub success: bool,
    /// Kullanılan gas
    pub gas_used: u64,
    /// Hata mesajı (varsa)
    pub error: Option<String>,
}

// ─────────────────────────────────────────────────────────────────────────────
// Bot Yapılandırması (.env tabanlı)
// ─────────────────────────────────────────────────────────────────────────────

pub struct BotConfig {
    /// WebSocket RPC URL (blok başlığı aboneliği için)
    pub rpc_wss_url: String,
    /// HTTP RPC URL (durum okuma için — gelecekte kullanılabilir)
    #[allow(dead_code)]
    pub rpc_http_url: String,
    /// Private key (kontrat tetikleme için, opsiyonel)
    pub private_key: Option<String>,
    /// Arbitraj kontrat adresi (opsiyonel)
    pub contract_address: Option<Address>,
    /// Tahmini gas maliyeti (USD)
    pub gas_cost_usd: f64,
    /// Flash loan ücreti (basis points)
    pub flash_loan_fee_bps: f64,
    /// Minimum net kâr eşiği (USD)
    pub min_net_profit_usd: f64,
    /// İstatistik gösterme aralığı (blok sayısı)
    pub stats_interval: u64,
    /// Maks yeniden bağlanma denemesi (0 = sınırsız)
    pub max_retries: u32,
    /// Başlangıç bekleme süresi (saniye)
    pub initial_retry_delay_secs: u64,
    /// Maksimum bekleme süresi (saniye)
    pub max_retry_delay_secs: u64,
    /// Veri tazelik eşiği (milisaniye)
    pub max_staleness_ms: u128,
    /// Maksimum flash loan boyutu (WETH)
    pub max_trade_size_weth: f64,
    /// Base zincir ID
    pub chain_id: u64,
}

impl BotConfig {
    /// .env dosyasından yapılandırmayı oku
    pub fn from_env() -> Result<Self> {
        let rpc_wss_url = std::env::var("RPC_WSS_URL")
            .map_err(|_| eyre::eyre!("RPC_WSS_URL .env dosyasında tanımlanmalıdır!"))?;

        if rpc_wss_url.is_empty() || rpc_wss_url.starts_with("wss://your-") {
            return Err(eyre::eyre!("RPC_WSS_URL geçerli bir URL olmalıdır!"));
        }

        let rpc_http_url = std::env::var("RPC_HTTP_URL")
            .map_err(|_| eyre::eyre!("RPC_HTTP_URL .env dosyasında tanımlanmalıdır!"))?;

        if rpc_http_url.is_empty() || rpc_http_url.starts_with("https://your-") {
            return Err(eyre::eyre!("RPC_HTTP_URL geçerli bir URL olmalıdır!"));
        }

        let private_key = std::env::var("PRIVATE_KEY")
            .ok()
            .filter(|pk| !pk.is_empty() && pk != "your-private-key-here");

        let contract_address = std::env::var("ARBITRAGE_CONTRACT_ADDRESS")
            .ok()
            .filter(|addr| !addr.is_empty() && addr != "0xYourContractAddress")
            .and_then(|addr| addr.parse::<Address>().ok());

        let gas_cost_usd = Self::parse_env_f64("GAS_COST_USD", 0.10);
        let flash_loan_fee_bps = Self::parse_env_f64("FLASH_LOAN_FEE_BPS", 5.0);
        let min_net_profit_usd = Self::parse_env_f64("MIN_NET_PROFIT_USD", 0.50);
        let max_trade_size_weth = Self::parse_env_f64("MAX_TRADE_SIZE_WETH", 50.0);

        let stats_interval = std::env::var("STATS_INTERVAL")
            .unwrap_or_else(|_| "10".into())
            .parse::<u64>()
            .unwrap_or(10);

        let max_retries = std::env::var("MAX_RETRIES")
            .unwrap_or_else(|_| "0".into())
            .parse::<u32>()
            .unwrap_or(0);

        let max_staleness_ms = std::env::var("MAX_STALENESS_MS")
            .unwrap_or_else(|_| "2000".into())
            .parse::<u128>()
            .unwrap_or(2000);

        let chain_id = std::env::var("CHAIN_ID")
            .unwrap_or_else(|_| "8453".into())
            .parse::<u64>()
            .unwrap_or(8453);

        Ok(Self {
            rpc_wss_url,
            rpc_http_url,
            private_key,
            contract_address,
            gas_cost_usd,
            flash_loan_fee_bps,
            min_net_profit_usd,
            stats_interval,
            max_retries,
            initial_retry_delay_secs: 2,
            max_retry_delay_secs: 60,
            max_staleness_ms,
            max_trade_size_weth,
            chain_id,
        })
    }

    /// Kontrat tetikleme modu aktif mi?
    pub fn execution_enabled(&self) -> bool {
        self.private_key.is_some() && self.contract_address.is_some()
    }

    /// .env'den f64 oku
    fn parse_env_f64(key: &str, default: f64) -> f64 {
        std::env::var(key)
            .unwrap_or_else(|_| default.to_string())
            .parse::<f64>()
            .unwrap_or(default)
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Havuz Adresleri (.env tabanlı)
// ─────────────────────────────────────────────────────────────────────────────

/// .env dosyasından havuz yapılandırmalarını oku
pub fn load_pool_configs_from_env() -> Result<Vec<PoolConfig>> {
    let pool_a_addr = std::env::var("POOL_A_ADDRESS")
        .map_err(|_| eyre::eyre!("POOL_A_ADDRESS .env dosyasında tanımlanmalıdır!"))?
        .parse::<Address>()
        .map_err(|e| eyre::eyre!("POOL_A_ADDRESS geçersiz adres: {}", e))?;

    let pool_a_name = std::env::var("POOL_A_NAME")
        .unwrap_or_else(|_| "Havuz A".into());

    let pool_a_fee_bps = std::env::var("POOL_A_FEE_BPS")
        .unwrap_or_else(|_| "5".into())
        .parse::<u32>()
        .unwrap_or(5);

    let pool_a_dex = match std::env::var("POOL_A_DEX")
        .unwrap_or_else(|_| "uniswap".into())
        .to_lowercase()
        .as_str()
    {
        "aerodrome" => DexType::Aerodrome,
        _ => DexType::UniswapV3,
    };

    let pool_b_addr = std::env::var("POOL_B_ADDRESS")
        .map_err(|_| eyre::eyre!("POOL_B_ADDRESS .env dosyasında tanımlanmalıdır!"))?
        .parse::<Address>()
        .map_err(|e| eyre::eyre!("POOL_B_ADDRESS geçersiz adres: {}", e))?;

    let pool_b_name = std::env::var("POOL_B_NAME")
        .unwrap_or_else(|_| "Havuz B".into());

    let pool_b_fee_bps = std::env::var("POOL_B_FEE_BPS")
        .unwrap_or_else(|_| "100".into())
        .parse::<u32>()
        .unwrap_or(100);

    let pool_b_dex = match std::env::var("POOL_B_DEX")
        .unwrap_or_else(|_| "aerodrome".into())
        .to_lowercase()
        .as_str()
    {
        "uniswap" => DexType::UniswapV3,
        _ => DexType::Aerodrome,
    };

    // Token decimal bilgileri (USDC=6, WETH=18 — Base Network standart)
    let token0_decimals = std::env::var("TOKEN0_DECIMALS")
        .unwrap_or_else(|_| "6".into())
        .parse::<u8>()
        .unwrap_or(6);

    let token1_decimals = std::env::var("TOKEN1_DECIMALS")
        .unwrap_or_else(|_| "18".into())
        .parse::<u8>()
        .unwrap_or(18);

    Ok(vec![
        PoolConfig {
            address: pool_a_addr,
            name: pool_a_name,
            fee_bps: pool_a_fee_bps,
            fee_fraction: pool_a_fee_bps as f64 / 10_000.0,
            token0_decimals,
            token1_decimals,
            dex: pool_a_dex,
        },
        PoolConfig {
            address: pool_b_addr,
            name: pool_b_name,
            fee_bps: pool_b_fee_bps,
            fee_fraction: pool_b_fee_bps as f64 / 10_000.0,
            token0_decimals,
            token1_decimals,
            dex: pool_b_dex,
        },
    ])
}

// ─────────────────────────────────────────────────────────────────────────────
// Arbitraj İstatistikleri
// ─────────────────────────────────────────────────────────────────────────────

pub struct ArbitrageStats {
    pub total_blocks_processed: u64,
    pub total_opportunities: u64,
    pub profitable_opportunities: u64,
    pub executed_trades: u64,
    pub failed_simulations: u64,
    pub max_spread_pct: f64,
    pub max_profit_usd: f64,
    pub total_potential_profit: f64,
    pub session_start: Instant,
}

impl ArbitrageStats {
    pub fn new() -> Self {
        Self {
            total_blocks_processed: 0,
            total_opportunities: 0,
            profitable_opportunities: 0,
            executed_trades: 0,
            failed_simulations: 0,
            max_spread_pct: 0.0,
            max_profit_usd: 0.0,
            total_potential_profit: 0.0,
            session_start: Instant::now(),
        }
    }

    pub fn uptime_str(&self) -> String {
        let secs = self.session_start.elapsed().as_secs();
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        format!("{:02}:{:02}:{:02}", h, m, s)
    }
}
