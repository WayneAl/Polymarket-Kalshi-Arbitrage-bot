//! Polymarket CLOB (Central Limit Order Book) order execution client.
//!
//! This module provides high-performance order execution for the Polymarket CLOB,
//! including pre-computed authentication credentials and optimized request handling.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use base64::engine::general_purpose::URL_SAFE;
use base64::Engine;
use ethers::signers::{LocalWallet, Signer};
use ethers::types::transaction::eip712::{Eip712, TypedData};
use ethers::types::H256;
use ethers::types::U256;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;

const USER_AGENT: &str = "py_clob_client";
const MSG_TO_SIGN: &str = "This message attests that I control the given wallet";
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";

// Polygon Mainnet Constants
const CTF_EXCHANGE_ADDR: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
const USDC_E_ADDR: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";

use ethers::abi::{encode, Token};
use ethers::middleware::SignerMiddleware;
use ethers::prelude::*;
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::transaction::eip2718::TypedTransaction;
use std::convert::TryFrom;

// ============================================================================
// PRE-COMPUTED EIP712 CONSTANTS
// ============================================================================

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiCreds {
    #[serde(rename = "apiKey")]
    pub api_key: String,
    #[serde(rename = "secret")]
    pub api_secret: String,
    #[serde(rename = "passphrase")]
    pub api_passphrase: String,
}

// ============================================================================
// PREPARED CREDENTIALS
// ============================================================================

#[derive(Clone)]
pub struct PreparedCreds {
    pub api_key: String,
    hmac_template: HmacSha256,
    api_key_header: HeaderValue,
    passphrase_header: HeaderValue,
}

impl PreparedCreds {
    pub fn from_api_creds(creds: &ApiCreds) -> Result<Self> {
        let decoded_secret = URL_SAFE.decode(&creds.api_secret)?;
        let hmac_template = HmacSha256::new_from_slice(&decoded_secret)
            .map_err(|e| anyhow!("Invalid HMAC key: {}", e))?;

        let api_key_header = HeaderValue::from_str(&creds.api_key)
            .map_err(|e| anyhow!("Invalid API key for header: {}", e))?;
        let passphrase_header = HeaderValue::from_str(&creds.api_passphrase)
            .map_err(|e| anyhow!("Invalid passphrase for header: {}", e))?;

        Ok(Self {
            api_key: creds.api_key.clone(),
            hmac_template,
            api_key_header,
            passphrase_header,
        })
    }

    /// Sign message using prewarmed HMAC
    #[inline]
    pub fn sign(&self, message: &[u8]) -> Vec<u8> {
        let mut mac = self.hmac_template.clone();
        mac.update(message);
        mac.finalize().into_bytes().to_vec()
    }

    /// Sign and return base64 (for L2 headers)
    #[inline]
    pub fn sign_b64(&self, message: &[u8]) -> String {
        URL_SAFE.encode(self.sign(message))
    }

    /// Get cached API key header
    #[inline]
    pub fn api_key_header(&self) -> HeaderValue {
        self.api_key_header.clone()
    }

    /// Get cached passphrase header
    #[inline]
    pub fn passphrase_header(&self) -> HeaderValue {
        self.passphrase_header.clone()
    }
}

fn add_default_headers(headers: &mut HeaderMap) {
    headers.insert("User-Agent", HeaderValue::from_static(USER_AGENT));
    headers.insert("Accept", HeaderValue::from_static("*/*"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));
    headers.insert("Content-Type", HeaderValue::from_static("application/json"));
}

#[inline(always)]
fn current_unix_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn clob_auth_digest(chain_id: u64, address_str: &str, timestamp: u64, nonce: u64) -> Result<H256> {
    let typed_json = json!({
        "types": {
            "EIP712Domain": [
                {"name": "name", "type": "string"},
                {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"}
            ],
            "ClobAuth": [
                {"name": "address", "type": "address"},
                {"name": "timestamp", "type": "string"},
                {"name": "nonce", "type": "uint256"},
                {"name": "message", "type": "string"}
            ]
        },
        "primaryType": "ClobAuth",
        "domain": { "name": "ClobAuthDomain", "version": "1", "chainId": chain_id },
        "message": { "address": address_str, "timestamp": timestamp.to_string(), "nonce": nonce, "message": MSG_TO_SIGN }
    });
    let typed: TypedData = serde_json::from_value(typed_json)?;
    Ok(typed.encode_eip712()?.into())
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct OrderArgs {
    pub token_id: String,
    pub price: f64,
    pub size: f64,
    pub side: String,
    pub fee_rate_bps: Option<i64>,
    pub nonce: Option<i64>,
    pub expiration: Option<String>,
    pub taker: Option<String>,
}

/// Order data for EIP712 signing (references to avoid clones in hot path)
struct OrderData<'a> {
    maker: &'a str,
    taker: &'a str,
    token_id: &'a str,
    maker_amount: &'a str,
    taker_amount: &'a str,
    side: i32,
    fee_rate_bps: &'a str,
    nonce: &'a str,
    signer: &'a str,
    expiration: &'a str,
    signature_type: i32,
    salt: u128,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderStruct {
    pub salt: u128,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    #[serde(rename = "tokenId")]
    pub token_id: String,
    #[serde(rename = "makerAmount")]
    pub maker_amount: String,
    #[serde(rename = "takerAmount")]
    pub taker_amount: String,
    pub expiration: String,
    pub nonce: String,
    #[serde(rename = "feeRateBps")]
    pub fee_rate_bps: String,
    pub side: i32,
    #[serde(rename = "signatureType")]
    pub signature_type: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct SignedOrder {
    pub order: OrderStruct,
    pub signature: String,
}

impl SignedOrder {
    pub fn post_body(&self, owner: &str, order_type: &str) -> String {
        let side_str = if self.order.side == 0 { "BUY" } else { "SELL" };
        let mut buf = String::with_capacity(512);
        buf.push_str(r#"{"order":{"salt":"#);
        buf.push_str(&self.order.salt.to_string());
        buf.push_str(r#","maker":""#);
        buf.push_str(&self.order.maker);
        buf.push_str(r#"","signer":""#);
        buf.push_str(&self.order.signer);
        buf.push_str(r#"","taker":""#);
        buf.push_str(&self.order.taker);
        buf.push_str(r#"","tokenId":""#);
        buf.push_str(&self.order.token_id);
        buf.push_str(r#"","makerAmount":""#);
        buf.push_str(&self.order.maker_amount);
        buf.push_str(r#"","takerAmount":""#);
        buf.push_str(&self.order.taker_amount);
        buf.push_str(r#"","expiration":""#);
        buf.push_str(&self.order.expiration);
        buf.push_str(r#"","nonce":""#);
        buf.push_str(&self.order.nonce);
        buf.push_str(r#"","feeRateBps":""#);
        buf.push_str(&self.order.fee_rate_bps);
        buf.push_str(r#"","side":""#);
        buf.push_str(side_str);
        buf.push_str(r#"","signatureType":"#);
        buf.push_str(&self.order.signature_type.to_string());
        buf.push_str(r#","signature":""#);
        buf.push_str(&self.signature);
        buf.push_str(r#""},"owner":""#);
        buf.push_str(owner);
        buf.push_str(r#"","orderType":""#);
        buf.push_str(order_type);
        buf.push_str(r#""}"#);
        buf
    }
}

#[inline(always)]
fn generate_seed() -> u128 {
    (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos()
        % u128::from(u32::MAX)) as u128
}

// ============================================================================
// ORDER CALCULATIONS
// ============================================================================

/// Convert f64 price (0.0-1.0) to basis points (0-10000)
/// e.g., 0.65 -> 6500
#[inline(always)]
pub fn price_to_bps(price: f64) -> u64 {
    ((price * 10000.0).round() as i64).max(0) as u64
}

/// Convert f64 size to micro-units (6 decimal places)
/// e.g., 100.5 -> 100_500_000
#[inline(always)]
pub fn size_to_micro(size: f64) -> u64 {
    ((size * 1_000_000.0).floor() as i64).max(0) as u64
}

/// Truncate amount to specific decimal places (assuming 6 base decimals)
/// e.g. 2 decimals -> round down to nearest 10000 (0.01)
/// 4 decimals -> round down to nearest 100 (0.0001)
#[inline(always)]
fn truncate_to_decimals(amount: u128, decimals: u32) -> u128 {
    let step = 10_u128.pow(6 - decimals);
    (amount / step) * step
}

/// BUY order calculation
/// Input: size in micro-units, price in basis points
/// Output: (side=0, maker_amount, taker_amount) in token decimals (6 dp)
/// ENFORCES: Maker (USDC) max 2 decimals, Taker (Shares) max 4 decimals
#[inline(always)]
pub fn get_order_amounts_buy(size_micro: u64, price_bps: u64) -> (i32, u128, u128) {
    // For BUY: taker = size (what we receive), maker = size * price (what we pay)

    // Taker (Shares): Truncate to 4 decimals
    let taker_raw = size_micro as u128; // Taker is Size
    let taker = truncate_to_decimals(taker_raw, 4);

    // Maker (USDC): Calculate based on TRUNCATED Taker amount to ensure consistency?
    // Or just truncate final maker?
    // Usually: Maker = Taker * Price.
    let maker_raw = (taker * price_bps as u128) / 10000;
    let maker = truncate_to_decimals(maker_raw, 2);

    // Re-verify relationship?
    // If we truncate Maker, effective price might change slightly.
    // But CLOB demands this precision.
    (0, maker, taker)
}

/// SELL order calculation
/// Input: size in micro-units, price in basis points
/// Output: (side=1, maker_amount, taker_amount) in token decimals (6 dp)
/// ENFORCES: Maker (Shares) max 4 decimals, Taker (USDC) max 2 decimals
#[inline(always)]
pub fn get_order_amounts_sell(size_micro: u64, price_bps: u64) -> (i32, u128, u128) {
    // For SELL: maker = size (what we give), taker = size * price (what we receive)

    // Maker (Shares): Truncate to 4 decimals
    let maker_raw = size_micro as u128;
    let maker = truncate_to_decimals(maker_raw, 4);

    // Taker (USDC): Calculate based on TRUNCATED Maker amount
    let taker_raw = (maker * price_bps as u128) / 10000;
    let taker = truncate_to_decimals(taker_raw, 2);

    (1, maker, taker)
}

/// Validate price is within allowed range for tick=0.01
#[inline(always)]
pub fn price_valid(price_bps: u64) -> bool {
    // For tick=0.01: price must be >= 0.01 (100 bps) and <= 0.99 (9900 bps)
    price_bps >= 100 && price_bps <= 9900
}

fn order_typed_data(chain_id: u64, exchange: &str, data: &OrderData<'_>) -> Result<TypedData> {
    let typed_json = json!({
        "types": {
            "EIP712Domain": [
                {"name": "name", "type": "string"},
                {"name": "version", "type": "string"},
                {"name": "chainId", "type": "uint256"},
                {"name": "verifyingContract", "type": "address"}
            ],
            "Order": [
                {"name":"salt","type":"uint256"},
                {"name":"maker","type":"address"},
                {"name":"signer","type":"address"},
                {"name":"taker","type":"address"},
                {"name":"tokenId","type":"uint256"},
                {"name":"makerAmount","type":"uint256"},
                {"name":"takerAmount","type":"uint256"},
                {"name":"expiration","type":"uint256"},
                {"name":"nonce","type":"uint256"},
                {"name":"feeRateBps","type":"uint256"},
                {"name":"side","type":"uint8"},
                {"name":"signatureType","type":"uint8"}
            ]
        },
        "primaryType": "Order",
        "domain": { "name": "Polymarket CTF Exchange", "version": "1", "chainId": chain_id, "verifyingContract": exchange },
        "message": {
            "salt": U256::from(data.salt),
            "maker": data.maker,
            "signer": data.signer,
            "taker": data.taker,
            "tokenId": U256::from_dec_str(data.token_id)?,
            "makerAmount": U256::from_dec_str(data.maker_amount)?,
            "takerAmount": U256::from_dec_str(data.taker_amount)?,
            "expiration": U256::from_dec_str(data.expiration)?,
            "nonce": U256::from_dec_str(data.nonce)?,
            "feeRateBps": U256::from_dec_str(data.fee_rate_bps)?,
            "side": data.side,
            "signatureType": data.signature_type,
        }
    });
    Ok(serde_json::from_value(typed_json)?)
}

fn get_exchange_address(chain_id: u64, neg_risk: bool) -> Result<String> {
    match (chain_id, neg_risk) {
        (137, true) => Ok("0xC5d563A36AE78145C45a50134d48A1215220f80a".into()),
        (137, false) => Ok("0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E".into()),
        (80002, true) => Ok("0xd91E80cF2E7be2e162c6513ceD06f1dD0dA35296".into()),
        (80002, false) => Ok("0xdFE02Eb6733538f8Ea35D585af8DE5958AD99E40".into()),
        _ => Err(anyhow!("unsupported chain")),
    }
}

// ============================================================================
// ORDER TYPES FOR FAK/FOK
// ============================================================================

/// Order type for Polymarket
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum PolyOrderType {
    /// Good Till Cancelled (default)
    GTC,
    /// Good Till Time
    GTD,
    /// Fill Or Kill - must fill entirely or cancel
    FOK,
    /// Fill And Kill - fill what you can, cancel rest
    FAK,
}

impl PolyOrderType {
    pub fn as_str(&self) -> &'static str {
        match self {
            PolyOrderType::GTC => "GTC",
            PolyOrderType::GTD => "GTD",
            PolyOrderType::FOK => "FOK",
            PolyOrderType::FAK => "FAK",
        }
    }
}

// ============================================================================
// GET ORDER RESPONSE
// ============================================================================

/// Response from GET /data/order/{order_id}
#[derive(Debug, Clone, Deserialize)]
#[allow(dead_code)]
pub struct PolymarketOrderResponse {
    pub id: String,
    pub status: String,
    pub market: Option<String>,
    pub outcome: Option<String>,
    pub price: String,
    pub side: String,
    pub size_matched: String,
    pub original_size: String,
    pub maker_address: Option<String>,
    pub asset_id: Option<String>,
    #[serde(default)]
    pub associate_trades: Vec<serde_json::Value>,
    #[serde(default)]
    pub created_at: Option<serde_json::Value>, // Can be string or integer
    #[serde(default)]
    pub expiration: Option<serde_json::Value>, // Can be string or integer
    #[serde(rename = "type")]
    pub order_type: Option<String>,
    pub owner: Option<String>,
}

// ============================================================================
// ASYNC CLIENT
// ============================================================================

/// Async Polymarket client for execution
pub struct PolymarketAsyncClient {
    host: String,
    rpc_url: String,
    chain_id: u64,
    http: reqwest::Client, // Async client with connection pooling
    wallet: Arc<LocalWallet>,
    funder: String,
    wallet_address_str: String,
    address_header: HeaderValue,
}

impl PolymarketAsyncClient {
    pub fn new(
        host: &str,
        rpc_url: &str,
        chain_id: u64,
        private_key: &str,
        funder: &str,
    ) -> Result<Self> {
        let wallet = private_key.parse::<LocalWallet>()?.with_chain_id(chain_id);
        let wallet_address_str = format!("{:?}", wallet.address());
        let address_header = HeaderValue::from_str(&wallet_address_str)
            .map_err(|e| anyhow!("Invalid wallet address for header: {}", e))?;

        // Build async client with connection pooling and keepalive
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            .tcp_keepalive(std::time::Duration::from_secs(30))
            .tcp_nodelay(true)
            .timeout(std::time::Duration::from_secs(10))
            .build()?;

        Ok(Self {
            host: host.trim_end_matches('/').to_string(),
            rpc_url: rpc_url.to_string(),
            chain_id,
            http,
            wallet: Arc::new(wallet),
            funder: funder.to_string(),
            wallet_address_str,
            address_header,
        })
    }

    /// Build L1 headers for authentication (derive-api-key)
    /// wallet.sign_hash() is CPU-bound (~1ms), safe to call in async context
    fn build_l1_headers(&self, nonce: u64) -> Result<HeaderMap> {
        let timestamp = current_unix_ts();
        let digest = clob_auth_digest(self.chain_id, &self.wallet_address_str, timestamp, nonce)?;
        let sig = self.wallet.sign_hash(digest)?;
        let mut headers = HeaderMap::new();
        headers.insert("POLY_ADDRESS", self.address_header.clone());
        headers.insert(
            "POLY_SIGNATURE",
            HeaderValue::from_str(&format!("0x{}", sig))?,
        );
        headers.insert(
            "POLY_TIMESTAMP",
            HeaderValue::from_str(&timestamp.to_string())?,
        );
        headers.insert("POLY_NONCE", HeaderValue::from_str(&nonce.to_string())?);
        add_default_headers(&mut headers);
        Ok(headers)
    }

    /// Derive API credentials from L1 wallet signature
    pub async fn derive_api_key(&self, nonce: u64) -> Result<ApiCreds> {
        let url = format!("{}/auth/derive-api-key", self.host);
        let headers = self.build_l1_headers(nonce)?;
        let resp = self.http.get(&url).headers(headers).send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("derive-api-key failed: {} {}", status, body));
        }
        Ok(resp.json().await?)
    }

    /// Build L2 headers for authenticated requests
    fn build_l2_headers(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        creds: &PreparedCreds,
    ) -> Result<HeaderMap> {
        let timestamp = current_unix_ts();
        let mut message = format!("{}{}{}", timestamp, method, path);
        if let Some(b) = body {
            message.push_str(b);
        }

        let sig_b64 = creds.sign_b64(message.as_bytes());

        let mut headers = HeaderMap::with_capacity(9);
        headers.insert("POLY_ADDRESS", self.address_header.clone());
        headers.insert("POLY_SIGNATURE", HeaderValue::from_str(&sig_b64)?);
        headers.insert(
            "POLY_TIMESTAMP",
            HeaderValue::from_str(&timestamp.to_string())?,
        );
        headers.insert("POLY_API_KEY", creds.api_key_header());
        headers.insert("POLY_PASSPHRASE", creds.passphrase_header());
        add_default_headers(&mut headers);
        Ok(headers)
    }

    /// Post order
    pub async fn post_order_async(
        &self,
        body: String,
        creds: &PreparedCreds,
    ) -> Result<reqwest::Response> {
        let path = "/order";
        let url = format!("{}{}", self.host, path);
        let headers = self.build_l2_headers("POST", path, Some(&body), creds)?;

        let resp = self
            .http
            .post(&url)
            .headers(headers)
            .body(body)
            .send()
            .await?;

        Ok(resp)
    }

    /// Get order by ID
    pub async fn get_order_async(
        &self,
        order_id: &str,
        creds: &PreparedCreds,
    ) -> Result<PolymarketOrderResponse> {
        let path = format!("/data/order/{}", order_id);
        let url = format!("{}{}", self.host, path);
        let headers = self.build_l2_headers("GET", &path, None, creds)?;

        let resp = self.http.get(&url).headers(headers).send().await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("get_order failed {}: {}", status, body));
        }

        Ok(resp.json().await?)
    }

    /// Check neg_risk for token - with caching
    pub async fn check_neg_risk(&self, token_id: &str) -> Result<bool> {
        let url = format!("{}/neg-risk?token_id={}", self.host, token_id);
        let resp = self
            .http
            .get(&url)
            .header("User-Agent", USER_AGENT)
            .send()
            .await?;

        let val: serde_json::Value = resp.json().await?;
        Ok(val["neg_risk"].as_bool().unwrap_or(false))
    }

    #[allow(dead_code)]
    pub fn wallet_address(&self) -> &str {
        &self.wallet_address_str
    }

    #[allow(dead_code)]
    pub fn funder(&self) -> &str {
        &self.funder
    }

    #[allow(dead_code)]
    pub fn wallet(&self) -> &LocalWallet {
        &self.wallet
    }
}

/// Shared async client wrapper for use in execution engine
pub struct SharedAsyncClient {
    inner: Arc<PolymarketAsyncClient>,
    creds: PreparedCreds,
    chain_id: u64,
    /// Pre-cached neg_risk lookups
    neg_risk_cache: std::sync::RwLock<HashMap<String, bool>>,
}

impl SharedAsyncClient {
    pub fn new(client: PolymarketAsyncClient, creds: PreparedCreds, chain_id: u64) -> Self {
        Self {
            inner: Arc::new(client),
            creds,
            chain_id,
            neg_risk_cache: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Load neg_risk cache from JSON file (output of build_sports_cache.py)
    pub fn load_cache(&self, path: &str) -> Result<usize> {
        let data = std::fs::read_to_string(path)?;
        let map: HashMap<String, bool> = serde_json::from_str(&data)?;
        let count = map.len();
        let mut cache = self.neg_risk_cache.write().unwrap();
        *cache = map;
        Ok(count)
    }

    /// Execute FAK buy order -
    pub async fn buy_fak(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
        fee: bool,
    ) -> Result<PolyFillAsync> {
        debug_assert!(!token_id.is_empty(), "token_id must not be empty");
        debug_assert!(price > 0.0 && price < 1.0, "price must be 0 < p < 1");
        debug_assert!(size >= 1.0, "size must be >= 1");
        self.execute_order(token_id, price, size, "BUY", fee).await
    }

    /// Execute FAK sell order -
    pub async fn sell_fak(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
        fee: bool,
    ) -> Result<PolyFillAsync> {
        debug_assert!(!token_id.is_empty(), "token_id must not be empty");
        debug_assert!(price > 0.0 && price < 1.0, "price must be 0 < p < 1");
        debug_assert!(size >= 1.0, "size must be >= 1");
        self.execute_order(token_id, price, size, "SELL", fee).await
    }

    async fn execute_order(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
        side: &str,
        fee: bool,
    ) -> Result<PolyFillAsync> {
        // Check neg_risk cache first
        let neg_risk = {
            let cache = self.neg_risk_cache.read().unwrap();
            cache.get(token_id).copied()
        };

        let neg_risk = match neg_risk {
            Some(nr) => nr,
            None => {
                let nr = self.inner.check_neg_risk(token_id).await?;
                let mut cache = self.neg_risk_cache.write().unwrap();
                cache.insert(token_id.to_string(), nr);
                nr
            }
        };

        // Build signed order
        let signed = self.build_signed_order(token_id, price, size, side, neg_risk, fee)?;
        // Owner must be the API key (not wallet address or funder!)
        let body = signed.post_body(&self.creds.api_key, PolyOrderType::FAK.as_str());

        // Post order
        let resp = self.inner.post_order_async(body, &self.creds).await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("Polymarket order failed {}: {}", status, body));
        }

        let resp_json: serde_json::Value = resp.json().await?;
        let order_id = resp_json["orderID"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        // Query fill status
        let order_info = self.inner.get_order_async(&order_id, &self.creds).await?;
        let filled_size: f64 = order_info.size_matched.parse().unwrap_or(0.0);
        let order_price: f64 = order_info.price.parse().unwrap_or(price);

        tracing::debug!(
            "[POLY-ASYNC] FAK {} {}: status={}, filled={:.2}/{:.2}, price={:.4}",
            side,
            order_id,
            order_info.status,
            filled_size,
            size,
            order_price
        );

        Ok(PolyFillAsync {
            order_id,
            filled_size,
            fill_cost: filled_size * order_price,
        })
    }

    /// Build a signed order
    fn build_signed_order(
        &self,
        token_id: &str,
        price: f64,
        size: f64,
        side: &str,
        neg_risk: bool,
        fee: bool,
    ) -> Result<SignedOrder> {
        let price_bps = price_to_bps(price);
        let size_micro = size_to_micro(size);

        if !price_valid(price_bps) {
            return Err(anyhow!(
                "price {} ({}bps) outside allowed range",
                price,
                price_bps
            ));
        }

        let (side_code, maker_amt, taker_amt) = if side.eq_ignore_ascii_case("BUY") {
            get_order_amounts_buy(size_micro, price_bps)
        } else if side.eq_ignore_ascii_case("SELL") {
            get_order_amounts_sell(size_micro, price_bps)
        } else {
            return Err(anyhow!("side must be BUY or SELL"));
        };

        let salt = generate_seed();
        let maker_amount_str = maker_amt.to_string();
        let taker_amount_str = taker_amt.to_string();

        let fee_rate_bps = if fee { "1000" } else { "0" };

        // Use references for EIP712 signing
        let data = OrderData {
            maker: &self.inner.funder,
            taker: ZERO_ADDRESS,
            token_id,
            maker_amount: &maker_amount_str,
            taker_amount: &taker_amount_str,
            side: side_code,
            fee_rate_bps,
            nonce: "0",
            signer: &self.inner.wallet_address_str,
            expiration: "0",
            signature_type: 0,
            salt,
        };
        let exchange = get_exchange_address(self.chain_id, neg_risk)?;
        let typed = order_typed_data(self.chain_id, &exchange, &data)?;
        let digest = typed.encode_eip712()?;

        let sig = self.inner.wallet.sign_hash(H256::from(digest))?;

        // Only allocate strings once for the final OrderStruct (serialization needs owned)
        Ok(SignedOrder {
            order: OrderStruct {
                salt,
                maker: self.inner.funder.clone(),
                signer: self.inner.wallet_address_str.clone(),
                taker: ZERO_ADDRESS.to_string(),
                token_id: token_id.to_string(),
                maker_amount: maker_amount_str,
                taker_amount: taker_amount_str,
                expiration: "0".to_string(),
                nonce: "0".to_string(),
                fee_rate_bps: fee_rate_bps.to_string(),
                side: side_code,
                signature_type: 0,
            },
            signature: format!("0x{}", sig),
        })
    }
    /// Redeem winning positions from CTF Exchange
    /// Uses stored RPC URL
    pub async fn redeem_positions(
        &self,
        condition_id: &str,
        index_sets: Vec<U256>,
    ) -> Result<H256> {
        // 1. Setup Provider
        let provider = Provider::<Http>::try_from(&self.inner.rpc_url)?;
        let provider = Arc::new(provider);

        // 2. Setup Signer (Wallet)
        // Use wallet() getter to clone from Arc
        let wallet = self.inner.wallet().clone().with_chain_id(self.chain_id);
        let client = SignerMiddleware::new(Arc::clone(&provider), wallet);
        let client = Arc::new(client);

        // 3. Prepare Contract Arguments
        // function redeemPositions(address collateralToken, bytes32 parentCollectionId, bytes32 conditionId, uint[] indexSets)

        let ctf_addr: Address = "0x4d97dcd97ec945f40cf65f87097ace5ea0476045".parse()?;
        let collateral: Address = USDC_E_ADDR.parse()?;

        // condition_id is hex string
        let condition_id_bytes = ethers::utils::hex::decode(condition_id.trim_start_matches("0x"))?;

        if condition_id_bytes.len() != 32 {
            return Err(anyhow!("Invalid condition_id length"));
        }
        let condition_id_token = Token::FixedBytes(condition_id_bytes);

        let parent_collection_id = Token::FixedBytes(vec![0u8; 32]);

        // indexSets provided by caller
        let index_sets_tokens: Vec<Token> = index_sets.into_iter().map(Token::Uint).collect();
        let index_sets_array = Token::Array(index_sets_tokens);

        // 4. Encode Function Data
        // Selector for redeemPositions(address,bytes32,bytes32,uint256[]) = 0x8f757270
        let func_sig = "redeemPositions(address,bytes32,bytes32,uint256[])";
        let selector = ethers::utils::id(func_sig);
        let mut data = selector[0..4].to_vec();

        let args = encode(&[
            Token::Address(collateral),
            parent_collection_id,
            condition_id_token,
            index_sets_array,
        ]);
        data.extend(args);

        // 5. Build TransactionRequest then convert to TypedTransaction to set EIP-1559 fields
        let tx_req = TransactionRequest::new()
            .to(ctf_addr)
            .data(data.clone())
            .value(U256::zero());

        let typed: TypedTransaction = tx_req.into();

        // Send and return hash (don't wait for confirmation here to keep it async/fast)
        let pending_tx = client.send_transaction(typed, None).await?;
        let tx_hash = pending_tx.tx_hash();

        tracing::info!("[Redeem] Transaction sent: {:?}", tx_hash);

        Ok(tx_hash)
    }

    /// Fetch all positions for the current wallet from Data API
    /// Returns a Map of Condition IDs to their redeemable IndexSets (Outcome Masks).
    pub async fn get_redeemable_conditions(&self) -> Result<HashMap<String, Vec<U256>>> {
        let wallet = self.inner.wallet_address_str.clone();
        // https://data-api.polymarket.com/positions?user=0x...
        let wallet_clean = wallet.trim_matches('"');
        let url = format!(
            "https://data-api.polymarket.com/positions?user={}",
            wallet_clean
        );

        let resp = self.inner.http.get(&url).send().await?;

        if !resp.status().is_success() {
            return Err(anyhow!("Failed to fetch positions: {}", resp.status()));
        }

        let value: serde_json::Value = resp.json().await?;
        // API returns a list of positions directly or nested?
        // Usually list of objects.
        let positions = value
            .as_array()
            .ok_or_else(|| anyhow!("Invalid positions response format"))?;

        let mut redeemable_map: HashMap<String, Vec<U256>> = HashMap::new();

        for pos in positions {
            // Check if size > 0
            if let Some(redeemable) = pos.get("redeemable").and_then(|v| v.as_bool()) {
                if redeemable {
                    if let Some(condition_id) = pos.get("conditionId").and_then(|v| v.as_str()) {
                        // Get outcomeIndex to determine indexSet
                        // outcomeIndex 0 -> 1 (1 << 0)
                        // outcomeIndex 1 -> 2 (1 << 1)
                        if let Some(outcome_index) =
                            pos.get("outcomeIndex").and_then(|v| v.as_u64())
                        {
                            let index_set = U256::from(1) << outcome_index;

                            redeemable_map
                                .entry(condition_id.to_string())
                                .or_default()
                                .push(index_set);
                        }
                    }
                }
            }
        }

        // Dedup index sets per condition just in case
        for sets in redeemable_map.values_mut() {
            sets.sort();
            sets.dedup();
        }

        Ok(redeemable_map)
    }

    /// Auto-Redeem all held positions
    /// Iterates through all redeemable positions and attempts to redeem them.
    pub async fn auto_redeem_positions(&self) -> Result<()> {
        let conditions_map = self.get_redeemable_conditions().await?;
        tracing::info!(
            "[AutoRedeem] Found {} redeemable conditions. Attempting to redeem...",
            conditions_map.len()
        );

        for (condition_id, index_sets) in conditions_map {
            tracing::info!(
                "[AutoRedeem] Redeeming condition {} with indexSets {:?}",
                condition_id,
                index_sets
            );
            match self.redeem_positions(&condition_id, index_sets).await {
                Ok(tx) => {
                    tracing::info!(
                        "[AutoRedeem] Redeemed condition {}: Tx {}",
                        condition_id,
                        tx
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "[AutoRedeem] Failed to redeem condition {}: {}",
                        condition_id,
                        e
                    );
                }
            }
        }
        Ok(())
    }
}

/// Async fill result
#[derive(Debug, Clone)]
pub struct PolyFillAsync {
    pub order_id: String,
    pub filled_size: f64,
    pub fill_cost: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use tracing::Level;

    #[tokio::test]
    async fn test_auto_redeem_positions() -> Result<()> {
        // Initialize logging
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::from_default_env().add_directive(Level::INFO.into()),
            )
            .init();

        dotenvy::dotenv().ok();

        // Setup credentials from env
        let private_key = std::env::var("POLY_PRIVATE_KEY").context("POLY_PRIVATE_KEY not set")?;
        let funder = std::env::var("POLY_FUNDER").context("POLY_FUNDER not set")?;
        let ws_url = std::env::var("POLYGON_WS_URL").unwrap_or_default();
        let rpc_url = std::env::var("POLYGON_RPC_URL").unwrap_or(ws_url);

        if rpc_url.is_empty() {
            panic!("Neither POLYGON_RPC_URL nor POLYGON_WS_URL set");
        }

        println!("Initializing client with RPC/WS: {}", rpc_url);

        let poly_client = PolymarketAsyncClient::new(
            "https://clob.polymarket.com",
            &rpc_url,
            137, // Polygon Mainnet
            &private_key,
            &funder,
        )?;

        // Derive API Key
        let api_creds = poly_client.derive_api_key(0).await?;
        let prepared_creds = PreparedCreds::from_api_creds(&api_creds)?;

        // Create Shared Client
        let shared_client = SharedAsyncClient::new(poly_client, prepared_creds, 137);

        println!("Starting auto-redeem...");
        // Run auto_redeem
        shared_client.auto_redeem_positions().await?;
        println!("Auto-redeem completed.");

        Ok(())
    }
}
