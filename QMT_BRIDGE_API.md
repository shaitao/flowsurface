# QMT Bridge API

这份文档对应当前 QMT bridge 暴露的 HTTP/WebSocket 接口（行情 + 交易）。

- Base URL: `QMT_BRIDGE_BASE`
- 默认值: `http://127.0.0.1:8765`
- Content-Type:
  - `GET`: 无要求
  - `POST`: `application/json`
  - 可选二进制: `application/msgpack` + `Content-Encoding: zstd`
- `symbol` 格式: `600309.SH` / `000001.SZ`
- 数值类型: JSON `number`

## 0. 健康检查

`GET /healthz`

### 200 Response

```json
{
  "ok": true,
  "activeSymbol": null,
  "activeSymbols": ["600309.SH", "300763.SZ"]
}
```

## 1. 行情 - Tick 历史

`GET /api/v1/ticks?symbol=600309.SH&start=1772501400000&end=1772521200000`

### Query 参数

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `symbol` | `string` | 是 | 股票代码，带市场后缀 |
| `start` | `number|string` | 是 | 起始时间（毫秒时间戳或 `YYYYMMDDHHMMSS`） |
| `end` | `number|string` | 是 | 结束时间（毫秒时间戳或 `YYYYMMDDHHMMSS`） |

### 200 Response

```json
{
  "ok": true,
  "items": [
    {
      "type": "tick",
      "source": "qmt",
      "symbol": "600309.SH",
      "time": 1772501401000,
      "lastPrice": 84.95,
      "open": 84.8,
      "high": 85.1,
      "low": 84.7,
      "lastClose": 84.7,
      "amount": 123456.0,
      "volume": 1200,
      "pvolume": 1200,
      "stockStatus": 0,
      "openInt": 0,
      "transactionNum": 3,
      "lastSettlementPrice": null,
      "settlementPrice": null,
      "pe": null,
      "askPrice": [84.95, 84.96, 84.97, 84.98, 84.99],
      "bidPrice": [84.94, 84.93, 84.92, 84.91, 84.9],
      "askVol": [900, 1200, 400, 500, 200],
      "bidVol": [1200, 800, 650, 300, 100]
    }
  ]
}
```

## 2. 行情 - K 线历史

`GET /api/v1/klines?symbol=600309.SH&period=1m&start=1772501400000&end=1772521200000`

### Query 参数

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `symbol` | `string` | 是 | 股票代码，带市场后缀 |
| `period` | `string` | 是 | `1m` / `5m` / `15m` / `30m` / `60m` / `1h` / `1d` / `1w` / `1mon` / `1q` / `1hy` / `1y` |
| `start` | `number|string` | 是 | 起始时间（毫秒时间戳或 `YYYYMMDDHHMMSS`） |
| `end` | `number|string` | 是 | 结束时间（毫秒时间戳或 `YYYYMMDDHHMMSS`） |

### 200 Response

```json
{
  "ok": true,
  "items": [
    {
      "time": 1772501400000,
      "open": 84.8,
      "high": 85.1,
      "low": 84.7,
      "close": 84.95,
      "volume": 12000,
      "amount": 1234567.0,
      "preClose": 84.7
    }
  ]
}
```

## 3. 交易日列表

`GET /api/v1/trading_days?venue=SSH&start=1772501400000&end=1772521200000`

### Query 参数

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `venue` | `string` | 是 | `SSH` / `SSZ` |
| `start` | `number|string` | 是 | 起始时间 |
| `end` | `number|string` | 是 | 结束时间 |

### 200 Response

```json
{
  "ok": true,
  "items": ["20260408", "20260409"]
}
```

## 4. 搜索标的

`GET /api/v1/search?venue=SSH&query=6003&limit=20`

### Query 参数

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `venue` | `string` | 是 | `SSH` / `SSZ` |
| `query` | `string` | 是 | 搜索关键词 |
| `limit` | `number` | 否 | 返回数量上限 |

### 200 Response

```json
{
  "ok": true,
  "items": [
    { "symbol": "600309.SH", "displayName": "万华化学", "minTicksize": 0.01, "minQty": 1 }
  ]
}
```

## 5. 标的名称列表

`GET /api/v1/symbols`

可选 `?venue=SSH` 或 `?venue=SSZ` 做市场过滤。

### 200 Response

```json
{
  "ok": true,
  "items": [
    { "symbol": "600309.SH", "displayName": "万华化学" }
  ]
}
```

## 6. 标的基础信息（含市值）

`GET /api/v1/instruments?symbols=600309.SH,000001.SZ`

### 200 Response

```json
{
  "ok": true,
  "items": [
    {
      "symbol": "600309.SH",
      "displayName": "万华化学",
      "exchangeId": "SH",
      "exchangeCode": "XSHG",
      "openDate": "20010108",
      "priceTick": 0.01,
      "volumeMultiple": 1,
      "floatShares": 123456789.0,
      "totalShares": 234567890.0,
      "lastPrice": 84.95,
      "marketCap": 19999999999.0,
      "floatMarketCap": 10000000000.0
    }
  ]
}
```

## 7. 板块列表

`GET /api/v1/sectors`

### 200 Response

```json
{
  "ok": true,
  "items": ["沪深A股", "上证A股", "深证A股", "创业板", "科创板", "沪深转债", "上证转债", "深证转债"]
}
```

## 8. 板块成分股

`GET /api/v1/sector_symbols?sector=科创板`

### Query 参数

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `sector` | `string` | 是 | 板块名称（来自 `/api/v1/sectors`） |

### 200 Response

```json
{
  "ok": true,
  "items": ["688001.SH", "688002.SH"]
}
```

## 9. 实时 Tick 订阅

`GET /ws/tick?symbol=600309.SH`

- WebSocket 连接成功后，服务端推送 tick 事件
- 返回体为 JSON 或 msgpack（依赖客户端请求）

## 10. 交易 - 下单面板快照

`GET /api/v1/order/panel?symbol=600309.SH`

### Query 参数

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `symbol` | `string` | 是 | 股票代码，带市场后缀 |

### 200 Response

```json
{
  "symbol": "600309.SH",
  "bestBid": 84.94,
  "bestAsk": 84.95,
  "lastPrice": 84.95,
  "bids": [
    { "price": 84.94, "quantity": 1200 }
  ],
  "asks": [
    { "price": 84.95, "quantity": 900 }
  ],
  "availableCash": 128734.55,
  "positionQty": 3000,
  "availableQty": 3000,
  "workingOrders": []
}
```

## 11. 交易 - 下单

`POST /api/v1/order/place`

### Request Body

```json
{
  "symbol": "600309.SH",
  "side": "buy",
  "orderType": "limit",
  "price": 84.95,
  "quantity": 1000
}
```

### 200 Response

```json
{
  "orderId": "202604090002",
  "status": "submitted",
  "message": "order accepted"
}
```

## 12. 交易 - 撤单

`POST /api/v1/order/cancel`

### Request Body

```json
{
  "symbol": "600309.SH",
  "orderId": "202604090002"
}
```

### 200 Response

```json
{
  "orderId": "202604090002",
  "status": "cancel_submitted",
  "message": "cancel accepted"
}
```

## 错误处理约定

当前前端对非 `2xx` 的处理比较简单:

- 只要 HTTP status 不是 `2xx`，就认为请求失败
- 前端会直接展示响应 body 文本

建议:

- 失败时返回明确的 HTTP 错误码
- body 用可读文本或短 JSON
