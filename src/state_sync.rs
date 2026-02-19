// ============================================================================
//  STATE_SYNC — Yerel Durum Senkronizasyonu
//
//  Event/Mempool dinleme yerine, her yeni blokta hedef havuzların
//  slot0 ve liquidity değerlerini RPC ile okuyarak RAM'e yazar.
//  Tüm fiyat kontrolleri RAM üzerinde yapılır (< 0.001ms).
// ============================================================================

use alloy::primitives::{Address, U256};
use alloy::providers::Provider;
use alloy::transports::Transport;
use alloy::network::Ethereum;
use alloy::sol;
use eyre::Result;
use std::time::Instant;

use crate::math::sqrt_price_x96_to_eth_price;
use crate::types::{PoolConfig, SharedPoolState};

// ─────────────────────────────────────────────────────────────────────────────
// Uniswap V3 / Aerodrome CL Havuz Arayüzü
// Her iki protokol de aynı slot0 + liquidity yapısını kullanır
// ─────────────────────────────────────────────────────────────────────────────

sol! {
    #[sol(rpc)]
    interface IPool {
        function slot0() external view returns (
            uint160 sqrtPriceX96,
            int24 tick,
            uint16 observationIndex,
            uint16 observationCardinality,
            uint16 observationCardinalityNext,
            uint8 feeProtocol,
            bool unlocked
        );

        function liquidity() external view returns (uint128);
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Tek Havuz Durum Senkronizasyonu
// ─────────────────────────────────────────────────────────────────────────────

/// Tek bir havuzun durumunu RPC üzerinden oku ve SharedPoolState'e yaz
///
/// Yapılan işlemler:
///   1. slot0() çağrısı → sqrtPriceX96, tick, unlocked
///   2. liquidity() çağrısı → anlık likidite
///   3. sqrtPriceX96 → ETH/USDC fiyatı dönüşümü
///   4. SharedPoolState'e atomic yazma (RwLock)
pub async fn sync_pool_state<T: Transport + Clone, P: Provider<T, Ethereum> + Sync>(
    provider: &P,
    pool_config: &PoolConfig,
    pool_state: &SharedPoolState,
    block_number: u64,
) -> Result<()> {
    let pool = IPool::new(pool_config.address, provider);

    // slot0 ve liquidity değerlerini oku
    let slot0_result = pool.slot0().call().await;
    let liquidity_result = pool.liquidity().call().await;

    let slot0 = slot0_result
        .map_err(|e| eyre::eyre!("[{}] slot0 okuma hatası: {}", pool_config.name, e))?;
    let liq_response = liquidity_result
        .map_err(|e| eyre::eyre!("[{}] liquidity okuma hatası: {}", pool_config.name, e))?;

    // Değerleri çıkar
    let sqrt_price_x96 = slot0.sqrtPriceX96;
    let tick = slot0.tick;
    let liquidity = liq_response._0;

    // Float dönüşümler (hızlı matematik için)
    let sqrt_price_f64: f64 = {
        // U256 → f64 güvenli dönüşüm (hassasiyet kaybı kabul edilebilir)
        let s = sqrt_price_x96.to_string();
        s.parse::<f64>().unwrap_or(0.0)
    };
    let liquidity_f64: f64 = liquidity.to_string().parse::<f64>().unwrap_or(0.0);

    // ETH fiyatını hesapla
    let eth_price = sqrt_price_x96_to_eth_price(
        sqrt_price_f64,
        pool_config.token0_decimals,
        pool_config.token1_decimals,
    );

    // State güncelle (write lock — çok kısa süreli)
    {
        let mut state = pool_state.write();
        state.sqrt_price_x96 = U256::from(sqrt_price_x96);
        state.sqrt_price_f64 = sqrt_price_f64;
        state.tick = tick;
        state.liquidity = liquidity;
        state.liquidity_f64 = liquidity_f64;
        state.eth_price_usd = eth_price;
        state.last_block = block_number;
        state.last_update = Instant::now();
        state.is_initialized = true;
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Havuz Bytecode Önbellekleme (REVM Simülasyonu İçin)
// ─────────────────────────────────────────────────────────────────────────────

/// Havuz bytecode'unu bir kez oku ve önbelleğe al
/// REVM simülasyonu için kontrat koduna ihtiyaç vardır.
pub async fn cache_pool_bytecode<T: Transport + Clone, P: Provider<T, Ethereum> + Sync>(
    provider: &P,
    pool_config: &PoolConfig,
    pool_state: &SharedPoolState,
) -> Result<()> {
    let code = provider
        .get_code_at(pool_config.address)
        .await
        .map_err(|e| eyre::eyre!("[{}] Bytecode okuma hatası: {}", pool_config.name, e))?;

    let mut state = pool_state.write();
    state.bytecode = Some(code.to_vec());

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Toplu Senkronizasyon
// ─────────────────────────────────────────────────────────────────────────────

/// Tüm havuzların durumunu senkronize et
///
/// Her havuz için hataları ayrı yakalar, bir havuzun hatası diğerini engellemez.
/// Başarı/hata durumları döndürülür.
pub async fn sync_all_pools<T: Transport + Clone, P: Provider<T, Ethereum> + Sync>(
    provider: &P,
    pools: &[PoolConfig],
    states: &[SharedPoolState],
    block_number: u64,
) -> Vec<Result<()>> {
    let mut results = Vec::with_capacity(pools.len());

    for (config, state) in pools.iter().zip(states.iter()) {
        let result = sync_pool_state(provider, config, state, block_number).await;
        results.push(result);
    }

    results
}

/// Tüm havuzların bytecode'larını önbelleğe al (başlangıçta bir kez)
pub async fn cache_all_bytecodes<T: Transport + Clone, P: Provider<T, Ethereum> + Sync>(
    provider: &P,
    pools: &[PoolConfig],
    states: &[SharedPoolState],
) -> Vec<Result<()>> {
    let mut results = Vec::with_capacity(pools.len());

    for (config, state) in pools.iter().zip(states.iter()) {
        let result = cache_pool_bytecode(provider, config, state).await;
        results.push(result);
    }

    results
}

// ─────────────────────────────────────────────────────────────────────────────
// Ek Depolama Yuvası Okuma (REVM Simülasyonu İçin)
// ─────────────────────────────────────────────────────────────────────────────

/// Belirli bir depolama yuvasını oku (REVM veritabanını doldurmak için)
#[allow(dead_code)]
pub async fn read_storage_slot<T: Transport + Clone, P: Provider<T, Ethereum> + Sync>(
    provider: &P,
    address: Address,
    slot: U256,
) -> Result<U256> {
    let value = provider
        .get_storage_at(address, slot)
        .await
        .map_err(|e| eyre::eyre!("Storage slot okuma hatası [{} @ slot {}]: {}", address, slot, e))?;

    Ok(value)
}

/// Birden fazla depolama yuvasını oku
#[allow(dead_code)]
pub async fn read_storage_slots<T: Transport + Clone, P: Provider<T, Ethereum> + Sync>(
    provider: &P,
    address: Address,
    slots: &[U256],
) -> Vec<Result<U256>> {
    let mut results = Vec::with_capacity(slots.len());

    for &slot in slots {
        let result = read_storage_slot(provider, address, slot).await;
        results.push(result);
    }

    results
}
