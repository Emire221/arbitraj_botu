// ============================================================================
//  MATH — Uniswap V3 / Aerodrome CL Matematik Motoru + Newton-Raphson
//  
//  Bu modül saf matematiksel hesaplamalar yapar. Hiçbir ağ bağımlılığı yoktur.
//  Tüm hesaplar f64 hassasiyetinde RAM üzerinde < 0.1ms'de tamamlanır.
// ============================================================================

use crate::types::PoolState;

// ─────────────────────────────────────────────────────────────────────────────
// Sabitler
// ─────────────────────────────────────────────────────────────────────────────

/// 2^96 — sqrtPriceX96 çözümleme sabiti
const Q96: f64 = 79_228_162_514_264_337_593_543_950_336.0;

/// WETH decimals (10^18)
const WETH_DECIMALS: f64 = 1_000_000_000_000_000_000.0;

/// USDC decimals (10^6)
const USDC_DECIMALS: f64 = 1_000_000.0;

// ─────────────────────────────────────────────────────────────────────────────
// Fiyat Hesaplama
// ─────────────────────────────────────────────────────────────────────────────

/// sqrtPriceX96'dan ETH fiyatını (USDC cinsinden) hesapla
///
/// Formül:
///   sqrt_price = sqrtPriceX96 / 2^96
///   price_ratio = sqrt_price^2  (token0 / token1 oranı, decimal dahil)
///   eth_price = 1 / (price_ratio * 10^(token0_dec - token1_dec))
///
/// Örnek:
///   USDC/WETH havuzunda (token0=USDC 6dec, token1=WETH 18dec)
///   sqrtPriceX96 ≈ 3.54e24 → ETH ≈ $2000
pub fn sqrt_price_x96_to_eth_price(
    sqrt_price_x96: f64,
    token0_decimals: u8,
    token1_decimals: u8,
) -> f64 {
    if sqrt_price_x96 <= 0.0 {
        return 0.0;
    }

    // 1) Ham fiyat oranı: (sqrtPriceX96 / 2^96)^2 = token1/token0 raw fiyatı
    let sqrt_price = sqrt_price_x96 / Q96;
    let price_ratio = sqrt_price * sqrt_price;

    // 2) Ondalık düzeltme: token0 ve token1 decimal farkını uygula
    //    decimal_adjustment = 10^(token0_dec - token1_dec)
    //    adjusted_price = price_ratio * decimal_adjustment
    //    Bu bize "1 token0 kaç token1 eder" (decimal-düzeltilmiş) verir
    let decimal_diff = token0_decimals as i32 - token1_decimals as i32;
    let decimal_adjustment = 10.0_f64.powi(decimal_diff);
    let adjusted_price = price_ratio * decimal_adjustment;

    if adjusted_price == 0.0 {
        return 0.0;
    }

    // 3) Eğer token0 decimal < token1 decimal ise (ör: USDC(6) / WETH(18)),
    //    bu fiyat "1 USDC kaç WETH eder" anlamına gelir (~0.0003).
    //    Bize gereken "1 WETH kaç USDC eder" olduğundan tersini almalıyız.
    if token0_decimals < token1_decimals {
        1.0 / adjusted_price
    } else {
        adjusted_price
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// V3 Swap Çıktı Hesaplamaları
// ─────────────────────────────────────────────────────────────────────────────

/// Token1 (WETH) gönder → Token0 (USDC) al
/// (Concentrated Liquidity — tek tick aralığı yaklaşımı)
///
/// Formül:
///   effective_amount = amount_in * (1 - fee)
///   sqrt_price_new = sqrt_price + (effective_amount * 10^18) / liquidity
///   amount_out = liquidity * (1/sqrt_price - 1/sqrt_price_new) / 10^6
///
/// # Parametreler
/// - `sqrt_price_f64`: sqrtPriceX96 ham değeri (f64)
/// - `liquidity`: havuz likiditesi (f64)
/// - `amount_in_weth`: giriş WETH miktarı (ör: 2.5 = 2.5 ETH)
/// - `fee_fraction`: ücret kesri (ör: 0.0005 = %0.05)
pub fn swap_weth_to_usdc(
    sqrt_price_f64: f64,
    liquidity: f64,
    amount_in_weth: f64,
    fee_fraction: f64,
) -> f64 {
    if sqrt_price_f64 <= 0.0 || liquidity <= 0.0 || amount_in_weth <= 0.0 {
        return 0.0;
    }

    let sqrt_price = sqrt_price_f64 / Q96;

    // Fee düş
    let effective_amount = amount_in_weth * (1.0 - fee_fraction);

    // Token1 (WETH) gönderildiğinde sqrtPrice ARTAR
    // Δ(sqrtPrice) = Δy / L  (y = token1 raw)
    let amount_in_raw = effective_amount * WETH_DECIMALS;
    let sqrt_price_new = sqrt_price + amount_in_raw / liquidity;

    if sqrt_price_new <= 0.0 {
        return 0.0;
    }

    // Token0 (USDC) çıktısı
    // Δx = L * (1/√P_old - 1/√P_new)  [token0 raw]
    let amount_out_raw = liquidity * (1.0 / sqrt_price - 1.0 / sqrt_price_new);

    // USDC decimal düzeltme
    (amount_out_raw / USDC_DECIMALS).abs()
}

/// Token0 (USDC) gönder → Token1 (WETH) al
/// (Concentrated Liquidity — tek tick aralığı yaklaşımı)
///
/// Formül:
///   effective_amount = amount_in * (1 - fee)
///   sqrt_price_new = L * sqrt_price / (L + effective_amount * 10^6 * sqrt_price)
///   amount_out = L * (sqrt_price - sqrt_price_new) / 10^18
///
/// # Parametreler
/// - `sqrt_price_f64`: sqrtPriceX96 ham değeri (f64)
/// - `liquidity`: havuz likiditesi (f64)
/// - `amount_in_usdc`: giriş USDC miktarı (ör: 5000.0 = 5000 USDC)
/// - `fee_fraction`: ücret kesri (ör: 0.0005 = %0.05)
pub fn swap_usdc_to_weth(
    sqrt_price_f64: f64,
    liquidity: f64,
    amount_in_usdc: f64,
    fee_fraction: f64,
) -> f64 {
    if sqrt_price_f64 <= 0.0 || liquidity <= 0.0 || amount_in_usdc <= 0.0 {
        return 0.0;
    }

    let sqrt_price = sqrt_price_f64 / Q96;

    // Fee düş
    let effective_amount = amount_in_usdc * (1.0 - fee_fraction);

    // Token0 (USDC) gönderildiğinde sqrtPrice AZALIR
    // Δ(1/√P) = Δx / L  (x = token0 raw)
    // √P_new = L * √P / (L + Δx_raw * √P)
    let amount_in_raw = effective_amount * USDC_DECIMALS;
    let denominator = liquidity + amount_in_raw * sqrt_price;

    if denominator <= 0.0 {
        return 0.0;
    }

    let sqrt_price_new = liquidity * sqrt_price / denominator;

    // Token1 (WETH) çıktısı
    // Δy = L * (√P_old - √P_new)  [token1 raw]
    let amount_out_raw = liquidity * (sqrt_price - sqrt_price_new);

    // WETH decimal düzeltme
    (amount_out_raw / WETH_DECIMALS).abs()
}

// ─────────────────────────────────────────────────────────────────────────────
// Arbitraj Kâr Hesaplama
// ─────────────────────────────────────────────────────────────────────────────

/// İki havuz arasında arbitraj kârını hesapla
///
/// Strateji (Flash Loan Arbitrajı):
///   1. Flash loan ile `amount_in_weth` WETH borç al
///   2. Pahalı havuzda WETH → USDC sat (yüksek fiyat)
///   3. Ucuz havuzda USDC → WETH al (düşük fiyat)
///   4. Flash loan'u geri öde (WETH + ücret)
///   5. Kalan = Kâr
///
/// # Dönüş
/// Net kâr (USD cinsinden). Negatif = zarar.
pub fn compute_arbitrage_profit(
    amount_in_weth: f64,
    sell_pool: &PoolState,
    sell_fee_fraction: f64,
    buy_pool: &PoolState,
    buy_fee_fraction: f64,
    gas_cost_usd: f64,
    flash_loan_fee_bps: f64,
    eth_price_usd: f64,
) -> f64 {
    if amount_in_weth <= 0.0 {
        return f64::NEG_INFINITY;
    }

    // 1. WETH'i pahalı havuzda sat → USDC al
    let usdc_received = swap_weth_to_usdc(
        sell_pool.sqrt_price_f64,
        sell_pool.liquidity_f64,
        amount_in_weth,
        sell_fee_fraction,
    );

    if usdc_received <= 0.0 {
        return f64::NEG_INFINITY;
    }

    // 2. USDC'yi ucuz havuzda kullan → WETH geri al
    let weth_received = swap_usdc_to_weth(
        buy_pool.sqrt_price_f64,
        buy_pool.liquidity_f64,
        usdc_received,
        buy_fee_fraction,
    );

    if weth_received <= 0.0 {
        return f64::NEG_INFINITY;
    }

    // 3. Flash loan geri ödemesi (WETH + ücret)
    let flash_loan_fee_rate = flash_loan_fee_bps / 10_000.0;
    let flash_loan_repay = amount_in_weth * (1.0 + flash_loan_fee_rate);

    // 4. Net WETH kârı
    let weth_profit = weth_received - flash_loan_repay;

    // 5. USD cinsinden net kâr (gas maliyeti düşülmüş)
    weth_profit * eth_price_usd - gas_cost_usd
}

// ─────────────────────────────────────────────────────────────────────────────
// Newton-Raphson Türev Hesaplayıcı
// ─────────────────────────────────────────────────────────────────────────────

/// Kâr fonksiyonunun birinci türevi (sayısal merkezi fark)
///
/// f'(x) ≈ [f(x+h) - f(x-h)] / (2h)
fn profit_derivative(
    amount_in_weth: f64,
    sell_pool: &PoolState,
    sell_fee: f64,
    buy_pool: &PoolState,
    buy_fee: f64,
    gas_cost_usd: f64,
    flash_loan_fee_bps: f64,
    eth_price_usd: f64,
) -> f64 {
    let h = (amount_in_weth * 1e-7).max(1e-10);

    let f_plus = compute_arbitrage_profit(
        amount_in_weth + h,
        sell_pool, sell_fee, buy_pool, buy_fee,
        gas_cost_usd, flash_loan_fee_bps, eth_price_usd,
    );
    let f_minus = compute_arbitrage_profit(
        amount_in_weth - h,
        sell_pool, sell_fee, buy_pool, buy_fee,
        gas_cost_usd, flash_loan_fee_bps, eth_price_usd,
    );

    (f_plus - f_minus) / (2.0 * h)
}

/// Kâr fonksiyonunun ikinci türevi (sayısal)
///
/// f''(x) ≈ [f'(x+h) - f'(x-h)] / (2h)
fn profit_second_derivative(
    amount_in_weth: f64,
    sell_pool: &PoolState,
    sell_fee: f64,
    buy_pool: &PoolState,
    buy_fee: f64,
    gas_cost_usd: f64,
    flash_loan_fee_bps: f64,
    eth_price_usd: f64,
) -> f64 {
    let h = (amount_in_weth * 1e-5).max(1e-8);

    let fp_plus = profit_derivative(
        amount_in_weth + h,
        sell_pool, sell_fee, buy_pool, buy_fee,
        gas_cost_usd, flash_loan_fee_bps, eth_price_usd,
    );
    let fp_minus = profit_derivative(
        amount_in_weth - h,
        sell_pool, sell_fee, buy_pool, buy_fee,
        gas_cost_usd, flash_loan_fee_bps, eth_price_usd,
    );

    (fp_plus - fp_minus) / (2.0 * h)
}

// ─────────────────────────────────────────────────────────────────────────────
// Newton-Raphson Optimizasyonu
// ─────────────────────────────────────────────────────────────────────────────

/// Newton-Raphson sonucu
#[derive(Debug, Clone)]
pub struct OptimalAmountResult {
    /// Optimal WETH miktarı (ör: 2.3415)
    pub optimal_amount: f64,
    /// Beklenen kâr (USD)
    pub expected_profit: f64,
    /// Algoritma yakınsadı mı?
    pub converged: bool,
    /// İterasyon sayısı
    pub iterations: u32,
}

/// Newton-Raphson ile optimal flash loan miktarını bul
///
/// Kâr fonksiyonu concave (içbükey) olduğu için f'(x)=0 noktası max kâr noktasıdır.
///
/// Algoritma:
///   1. Kaba tarama: 20 noktada kârı hesapla, en iyi başlangıç noktasını bul
///   2. Newton-Raphson ince ayar: f'(x) = 0 noktasını bul
///      x_{n+1} = x_n - f'(x_n) / f''(x_n)
///   3. Yakınsama toleransı: 10^-8 WETH
///
/// # Parametreler
/// - `sell_pool`: Pahalı havuz (WETH satılacak)
/// - `buy_pool`: Ucuz havuz (USDC ile WETH alınacak)
/// - `max_amount_weth`: Flash loan limiti
pub fn find_optimal_amount(
    sell_pool: &PoolState,
    sell_fee: f64,
    buy_pool: &PoolState,
    buy_fee: f64,
    gas_cost_usd: f64,
    flash_loan_fee_bps: f64,
    eth_price_usd: f64,
    max_amount_weth: f64,
) -> OptimalAmountResult {
    let max_iterations: u32 = 50;
    let tolerance = 1e-8;
    let min_amount = 0.0001; // Minimum 0.0001 WETH

    // ─── AŞAMA 1: Kaba Tarama (Golden Section benzeri) ─────────────
    let mut best_amount = 0.0;
    let mut best_profit = f64::NEG_INFINITY;
    let scan_steps = 30;

    for i in 1..=scan_steps {
        let fraction = i as f64 / scan_steps as f64;
        // Logaritmik dağılım (küçük miktarlara daha çok ağırlık)
        let amount = min_amount + (max_amount_weth - min_amount) * fraction * fraction;

        let profit = compute_arbitrage_profit(
            amount,
            sell_pool, sell_fee, buy_pool, buy_fee,
            gas_cost_usd, flash_loan_fee_bps, eth_price_usd,
        );

        if profit > best_profit {
            best_profit = profit;
            best_amount = amount;
        }
    }

    // Kaba taramada kârlı nokta bulunamadıysa
    if best_profit <= f64::NEG_INFINITY + 1.0 || best_amount <= 0.0 {
        return OptimalAmountResult {
            optimal_amount: 0.0,
            expected_profit: best_profit.max(0.0),
            converged: false,
            iterations: 0,
        };
    }

    // ─── AŞAMA 2: Newton-Raphson İnce Ayar ────────────────────────
    let mut x = best_amount;
    let mut converged = false;
    let mut final_iterations: u32 = 0;

    for i in 0..max_iterations {
        final_iterations = i + 1;

        // Birinci türev: f'(x) — kârın artış oranı
        let f_prime = profit_derivative(
            x, sell_pool, sell_fee, buy_pool, buy_fee,
            gas_cost_usd, flash_loan_fee_bps, eth_price_usd,
        );

        // İkinci türev: f''(x) — kârın eğriliği
        let f_double_prime = profit_second_derivative(
            x, sell_pool, sell_fee, buy_pool, buy_fee,
            gas_cost_usd, flash_loan_fee_bps, eth_price_usd,
        );

        // İkinci türev sıfıra çok yakınsa Newton adımı güvenilmez
        if f_double_prime.abs() < 1e-20 {
            break;
        }

        // Newton adımı: x_{n+1} = x_n - f'(x_n) / f''(x_n)
        let step = f_prime / f_double_prime;
        let x_new = (x - step).clamp(min_amount, max_amount_weth);

        // Yakınsama kontrolü
        if (x_new - x).abs() < tolerance {
            converged = true;
            x = x_new;
            break;
        }

        x = x_new;
    }

    // Final kâr hesapla
    let final_profit = compute_arbitrage_profit(
        x, sell_pool, sell_fee, buy_pool, buy_fee,
        gas_cost_usd, flash_loan_fee_bps, eth_price_usd,
    );

    OptimalAmountResult {
        optimal_amount: x,
        expected_profit: final_profit,
        converged,
        iterations: final_iterations,
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Testler
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;
    use std::time::Instant;

    /// Test havuz durumu oluştur (ETH ≈ $2000)
    fn make_test_pool(eth_price: f64) -> PoolState {
        // ETH fiyatına karşılık gelen sqrtPriceX96:
        // price_ratio = 1 / (eth_price * 10^-12) = 10^12 / eth_price
        // sqrt_price = sqrt(price_ratio) = sqrt(10^12 / eth_price)
        // sqrtPriceX96 = sqrt_price * 2^96
        let price_ratio = 1.0e12 / eth_price;
        let sqrt_price = price_ratio.sqrt();
        let sqrt_price_x96 = sqrt_price * Q96;

        // Gerçekçi likidite: ~10^18 (mainnet V3 derin havuz seviyesi)
        let liquidity: u128 = 1_000_000_000_000_000_000;

        PoolState {
            sqrt_price_x96: U256::ZERO,
            sqrt_price_f64: sqrt_price_x96,
            tick: 0,
            liquidity,
            liquidity_f64: liquidity as f64,
            eth_price_usd: eth_price,
            last_block: 0,
            last_update: Instant::now(),
            is_initialized: true,
            bytecode: None,
        }
    }

    #[test]
    fn test_sqrt_price_to_eth_price() {
        // sqrtPriceX96 for ETH ≈ $2000
        let pool = make_test_pool(2000.0);
        let price = sqrt_price_x96_to_eth_price(pool.sqrt_price_f64, 6, 18);
        assert!(
            (price - 2000.0).abs() < 1.0,
            "ETH fiyatı ~2000 olmalı, hesaplanan: {:.2}",
            price
        );
    }

    #[test]
    fn test_swap_weth_to_usdc() {
        let pool = make_test_pool(2000.0);
        // 1 WETH → ~2000 USDC (fee dahil)
        let usdc_out = swap_weth_to_usdc(
            pool.sqrt_price_f64,
            pool.liquidity_f64,
            1.0,    // 1 WETH
            0.0005, // %0.05 fee
        );
        assert!(
            usdc_out > 1900.0 && usdc_out < 2100.0,
            "1 WETH ≈ 2000 USDC olmalı, hesaplanan: {:.2}",
            usdc_out
        );
    }

    #[test]
    fn test_swap_usdc_to_weth() {
        let pool = make_test_pool(2000.0);
        // 2000 USDC → ~1 WETH (fee dahil)
        let weth_out = swap_usdc_to_weth(
            pool.sqrt_price_f64,
            pool.liquidity_f64,
            2000.0,  // 2000 USDC
            0.0005,  // %0.05 fee
        );
        assert!(
            weth_out > 0.90 && weth_out < 1.10,
            "2000 USDC ≈ 1 WETH olmalı, hesaplanan: {:.6}",
            weth_out
        );
    }

    #[test]
    fn test_newton_raphson_finds_optimum() {
        // Ucuz havuz (ETH $1980) ve pahalı havuz (ETH $2020) — %2 spread
        let buy_pool = make_test_pool(1980.0);  // Ucuz: buradan WETH al
        let sell_pool = make_test_pool(2020.0); // Pahalı: buraya WETH sat

        let result = find_optimal_amount(
            &sell_pool, 0.0005,   // Pahalı havuz fee (%0.05)
            &buy_pool, 0.01,     // Ucuz havuz fee (%1.0)
            0.10,                // Gas maliyeti
            5.0,                 // Flash loan fee bps
            2000.0,              // ETH fiyatı referans
            10.0,                // Max trade size
        );

        println!(
            "Optimal miktar: {:.6} WETH, Beklenen kâr: {:.4} USD, İterasyon: {}, Yakınsadı: {}",
            result.optimal_amount, result.expected_profit, result.iterations, result.converged
        );

        // Kârlı bir fırsat bulunmalı
        assert!(result.expected_profit > 0.0, "Kâr pozitif olmalı");
        assert!(result.optimal_amount > 0.0, "Optimal miktar > 0 olmalı");
    }
}
