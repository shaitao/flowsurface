# QMT Order Bridge API

这份文档对应当前桌面端下单面板实际使用的桥接接口。

- Base URL: `QMT_BRIDGE_BASE`
- 默认值: `http://127.0.0.1:8765`
- Content-Type:
  - `GET`: 无要求
  - `POST`: `application/json`
- `symbol` 格式: `600309.SH` / `000001.SZ`
- 数值类型: JSON `number`
- 枚举序列化:
  - `side`: `buy` / `sell`
  - `orderType`: `limit` / `market`

## 1. 获取下单面板快照

`GET /api/v1/order/panel?symbol=600309.SH`

用途:
- 初始化下单面板
- 刷新资金、持仓、挂单
- 尽量直接给 5 档报价，避免面板刚打开时要等 live depth

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
    { "price": 84.94, "quantity": 1200 },
    { "price": 84.93, "quantity": 800 },
    { "price": 84.92, "quantity": 650 },
    { "price": 84.91, "quantity": 300 },
    { "price": 84.90, "quantity": 100 }
  ],
  "asks": [
    { "price": 84.95, "quantity": 900 },
    { "price": 84.96, "quantity": 1200 },
    { "price": 84.97, "quantity": 400 },
    { "price": 84.98, "quantity": 500 },
    { "price": 84.99, "quantity": 200 }
  ],
  "availableCash": 128734.55,
  "positionQty": 3000,
  "availableQty": 3000,
  "workingOrders": [
    {
      "orderId": "202604090001",
      "side": "buy",
      "orderType": "limit",
      "price": 84.88,
      "quantity": 1000,
      "filledQuantity": 0,
      "status": "submitted"
    }
  ]
}
```

### 字段说明

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `symbol` | `string` | 是 | 返回的标的代码 |
| `bestBid` | `number \| null` | 是 | 买一价，没有可返回 `null` |
| `bestAsk` | `number \| null` | 是 | 卖一价，没有可返回 `null` |
| `lastPrice` | `number \| null` | 是 | 最新价，没有可返回 `null` |
| `bids` | `OrderBookLevel[]` | 是 | 买盘列表，建议最多 5 档，按价格从高到低 |
| `asks` | `OrderBookLevel[]` | 是 | 卖盘列表，建议最多 5 档，按价格从低到高 |
| `availableCash` | `number \| null` | 是 | 可用资金 |
| `positionQty` | `number \| null` | 是 | 当前持仓数量 |
| `availableQty` | `number \| null` | 是 | 可卖/可用数量 |
| `workingOrders` | `WorkingOrder[]` | 是 | 当前未完成委托，没有就返回 `[]` |

`OrderBookLevel`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `price` | `number` | 是 | 档位价格 |
| `quantity` | `number` | 是 | 档位数量 |

`WorkingOrder`

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `orderId` | `string` | 是 | 委托唯一标识 |
| `side` | `buy \| sell` | 是 | 买卖方向 |
| `orderType` | `limit \| market` | 是 | 委托类型 |
| `price` | `number \| null` | 是 | 市价单可返回 `null` |
| `quantity` | `number` | 是 | 原始委托数量 |
| `filledQuantity` | `number` | 是 | 已成交数量 |
| `status` | `string` | 是 | 状态文本，前端直接展示 |

### 实现建议

- `bids/asks` 最好直接返回 5 档。
- 如果暂时拿不到 5 档，也请至少返回 `bestBid/bestAsk`，并把 `bids/asks` 返回 `[]`。
- `workingOrders` 没有时必须返回空数组，不要返回 `null`。

## 2. 下单

`POST /api/v1/order/place`

用途:
- 面板上的 `Buy Market` / `Buy Limit` / `Sell Market` / `Sell Limit`

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

### 字段说明

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `symbol` | `string` | 是 | 股票代码，带市场后缀 |
| `side` | `buy \| sell` | 是 | 买卖方向 |
| `orderType` | `limit \| market` | 是 | 委托类型 |
| `price` | `number \| null` | 是 | 限价单必须给正数；市价单应传 `null` |
| `quantity` | `number` | 是 | 下单数量，正数 |

### 200 Response

```json
{
  "orderId": "202604090002",
  "status": "submitted",
  "message": "order accepted"
}
```

### 字段说明

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `orderId` | `string` | 是 | 委托唯一标识 |
| `status` | `string` | 是 | 状态文本，前端直接展示 |
| `message` | `string \| null` | 是 | 可选补充说明 |

## 3. 撤单

`POST /api/v1/order/cancel`

### Request Body

```json
{
  "symbol": "600309.SH",
  "orderId": "202604090002"
}
```

### 字段说明

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `symbol` | `string` | 是 | 股票代码，带市场后缀 |
| `orderId` | `string` | 是 | 要撤销的委托 ID |

### 200 Response

```json
{
  "orderId": "202604090002",
  "status": "cancel_submitted",
  "message": "cancel accepted"
}
```

### 字段说明

| 字段 | 类型 | 必填 | 说明 |
|---|---|---:|---|
| `orderId` | `string` | 是 | 被撤销的委托 ID |
| `status` | `string` | 是 | 状态文本，前端直接展示 |
| `message` | `string \| null` | 是 | 可选补充说明 |

## 错误处理约定

当前前端对非 `2xx` 的处理比较简单:

- 只要 HTTP status 不是 `2xx`，就认为请求失败
- 前端会直接展示响应 body 文本

所以对端实现时建议:

- 失败时返回明确的 HTTP 错误码
- body 用可读文本，或者短 JSON 文本都可以
- 但要保证 body 对人可读，因为会直接显示到 UI

示例:

```http
HTTP/1.1 400 Bad Request
Content-Type: text/plain

invalid quantity
```

## 当前前端实际依赖

前端当前已经按下面这套约定写死:

- 下单按钮:
  - `Buy Market`
  - `Buy Limit`
  - `Sell Market`
  - `Sell Limit`
- 限价单必须有有效 `price`
- 市价单会发送 `"price": null`
- 撤单入口在 `workingOrders` 列表右侧 `Cancel`
- 点击报价会把报价价格填到价格输入框
  - 这里优先使用 `/api/v1/order/panel` 的 `bids/asks`
  - 如果 snapshot 没给 5 档，面板也会继续吃 live depth 更新

## 建议联调顺序

1. 先实现 `GET /api/v1/order/panel`
2. 再实现 `POST /api/v1/order/place`
3. 最后实现 `POST /api/v1/order/cancel`

这样前端可以先把面板、资金、挂单和 5 档报价跑起来，再联调下单和撤单。
