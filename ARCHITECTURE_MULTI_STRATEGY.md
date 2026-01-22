# 多策略價格 Feed 架構

## 概述

已將 RTDS 價格流整合到 `Client` 中，使用 **broadcast channel** 實現雙軌全局價格 feed（Crypto + Chainlink），所有策略共享同一個 RTDS WebSocket 連接。

## 架構設計

### 之後（全局 Broadcast - Crypto + Chainlink）

```
┌──────────────────────────────────────────────────────┐
│                    Client                            │
│  ┌────────────────────────────────────────────────┐  │
│  │  Global Price Feeds (tokio::spawn)             │  │
│  │  ┌──────────────────┐  ┌──────────────────┐   │  │
│  │  │  Crypto Feed     │  │  Chainlink Feed  │   │  │
│  │  │ (ALL cryptocurs) │  │  (ALL chainlink) │   │  │
│  │  └────────┬─────────┘  └────────┬─────────┘   │  │
│  │           ↓                     ↓              │  │
│  │  BroadcastSender<CryptoPrice>   BroadcastSender<ChainlinkPrice>
│  └────────────────────────────────────────────────┘  │
│    ↓                               ↓                 │
│  Crypto Channel                   Chainlink Channel  │
│  (capacity: 1000)                 (capacity: 1000)   │
└──────────────────────────────────────────────────────┘
   / | \                                / | \
  ↓  ↓  ↓                              ↓  ↓  ↓
Strat1 Strat2 ... (crypto RX)    Strat1 Strat2 ... (chainlink RX)
```

**優勢**：

- ✅ **2 個 WebSocket 連接**（Crypto + Chainlink）服務所有策略
- ✅ **獨立訂閱** 策略可以只訂閱需要的 feed（兩者或其中之一）
- ✅ **低延遲** 所有策略同時接收同類型價格
- ✅ **高效率** broadcast channel 零複製分發
- ✅ **簡潔** 策略無需關心 Stream 的複雜性

## 代碼變化

### 1. Client 結構擴展 `src/polymarket.rs`

```rust
#[derive(Clone)]
pub struct Client {
    // ... 既有字段 ...
    pub rtds: Arc<RtdsClient>,

    // NEW: Broadcast channels for dual price feeds
    crypto_price_tx: tokio::sync::broadcast::Sender<CryptoPrice>,
    chainlink_price_tx: tokio::sync::broadcast::Sender<ChainlinkPrice>,
    price_feed_task: Arc<tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>>,
}
```

### 2. 新增方法

#### `start_price_feed()`

在 bot 啟動時調用，啟動兩個全局價格監聽任務（Crypto + Chainlink）。

```rust
pub async fn start_price_feed(&self) -> Result<()> {
    // Subscribe to RTDS crypto prices (all)
    // Subscribe to RTDS chainlink prices (all)
    // Run both in parallel using tokio::join!
}
```

**特點**：

- 只啟動一次（checked by `task_guard`）
- 訂閱 **所有** Crypto 幣種（`None` 參數）
- 訂閱 **所有** Chainlink 資產（`None` 參數）
- 各自捕捉錯誤並以 warn 級別記錄
- 兩個任務在後台並行運行

#### `subscribe_prices()`

策略調用此方法獲得 Crypto 廣播接收器。

```rust
pub fn subscribe_prices(&self) -> tokio::sync::broadcast::Receiver<CryptoPrice> {
    self.crypto_price_tx.subscribe()
}
```

#### `subscribe_chainlink_prices()`

策略調用此方法獲得 Chainlink 廣播接收器。

```rust
pub fn subscribe_chainlink_prices(&self) -> tokio::sync::broadcast::Receiver<ChainlinkPrice> {
    self.chainlink_price_tx.subscribe()
}
```

### 3. Strategy 更新 `src/strategy_0x8dxd.rs`

**之前**（直接訂閱 RTDS）：

```rust
let stream = rtds_client
    .subscribe_crypto_prices(Some(vec![symbol]))  // 特定幣種
    .expect("Failed to subscribe rtds");
let mut price_stream = Box::pin(stream);
```

**之後**（使用 broadcast channel）：

```rust
let mut price_rx = self.client.subscribe_prices();
let asset_lower = self.asset.to_lowercase();

while let Ok(price) = price_rx.recv().await {
    // Filter locally for this strategy's asset
    if !price.symbol.to_lowercase().starts_with(&asset_lower) {
        continue;
    }
    // Process...
}
```

**改進**：

- ✅ 無需 `Box::pin()`
- ✅ 無需理解 Stream trait
- ✅ 簡潔的 broadcast 接收 API
- ✅ 本地過濾（極快）vs 網路過濾

### 4. Bot 初始化 `src/bin/bot.rs`

```rust
// Create client
let poly_client = Client::new(...)
    .await?;

// Start global price feed (NEW)
poly_client.start_price_feed().await?;

// Now all strategies spawned will share this feed
```

## Broadcast Channel 的優勢

| 特性       | 直接訂閱 RTDS     | Broadcast Channel（雙軌）      |
| ---------- | ----------------- | ------------------------------ |
| 連接數     | N（策略數）       | 2（Crypto + Chainlink）        |
| 資料複製   | 每個策略一份      | 零複製共享                     |
| API 複雜度 | 需要 `Box::pin()` | 簡單 RX 語法                   |
| 過濾位置   | 網路側（伺服器）  | 本地（CPU 緩存）               |
| 訂閱延遲   | 策略啟動時+300ms  | 立即（已有 1000 消息緩衝）     |
| 獨立訂閱   | 必須訂閱所有      | 可選：Crypto、Chainlink 或兩者 |
| 錯誤恢復   | 需逐個重連        | 全局管理                       |

## 實現細節

### Pin 與 Async Stream

`Box::pin()` 是必要的，因為：

1. Rust futures 需要穩定的記憶體位置（知道地址）
2. `StreamExt::next()` 方法簽名要求 `Pin<&mut Self>`
3. `Box` 在堆上分配，地址穩定
4. `pin()` 包裝指針，禁止移動（Move）

**為什麼現在不需要？**

- Broadcast `Receiver` 已內部實現 pinning
- `recv()` 方法返回 `Future`，無需手動 pin
- 更高級的抽象，隱藏複雜性

### Broadcast Channel 容量

```rust
let (price_tx, _) = tokio::sync::broadcast::channel(1000);
```

- **1000 消息緩衝**：策略可以短時間內超時而不遺漏
- **權衡**：更大的緩衝 = 更多記憶體，但通常每個價格 ≈ 100 bytes
- **典型吞吐量**：RTDS 給出 ~10-50 updates/sec（取決於波動性）

## 故障排查

### 場景 1：策略收不到價格

```rust
// ❌ Wrong - start_price_feed 未被調用
let client = Client::new(...).await?;
let strat = Strategy0x8dxd::new(client, ...).await;
// 價格 RX 永遠不會收到任何東西

// ✅ Correct
let client = Client::new(...).await?;
client.start_price_feed().await?;  // START FIRST
let strat = Strategy0x8dxd::new(client, ...).await;
```

### 場景 2：RTDS 連接斷開

- 全局任務偵測並記錄 `[PRICE_FEED] Stream error`
- 廣播停止發送新消息（策略 RX `recv()` 返回 error）
- 當前無自動重連機制（可在未來添加）

### 場景 3：Broadcast buffer 滿了

```rust
// 隱式行為：最舊的消息被拋棄
// 但這種情況不太可能（1000 容量 + 快速消費）
```

## 未來改進

1. **自動重連**：若 RTDS 斷開，嘗試重新訂閱
2. **可配置過濾**：在 broadcast 層面按幣種過濾，減少接收器工作
3. **Metrics**：記錄每秒消息數、延遲、掉落率
4. **策略級別的快照**：提供最新價格快照，無需等待下一個 tick

## 總結

多策略方案通過將 RTDS 訂閱升級為全局共享機制，實現：

- **資源效率**：1 個連接 vs N 個連接
- **代碼簡潔性**：移除 `Box::pin()`、Stream 複雜性
- **可擴展性**：添加新策略無額外成本
- **可維護性**：集中管理 price feed 錯誤和重連邏輯
