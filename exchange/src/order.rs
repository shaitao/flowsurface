use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OrderSide {
    Buy,
    Sell,
}

impl std::fmt::Display for OrderSide {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Buy => write!(f, "Buy"),
            Self::Sell => write!(f, "Sell"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OrderType {
    #[default]
    Limit,
    Market,
}

impl std::fmt::Display for OrderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Limit => write!(f, "Limit"),
            Self::Market => write!(f, "Market"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderSubmitRequest {
    pub side: OrderSide,
    pub order_type: OrderType,
    pub price: Option<f32>,
    pub quantity: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderCancelRequest {
    pub order_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkingOrder {
    pub order_id: String,
    pub side: OrderSide,
    pub order_type: OrderType,
    pub price: Option<f32>,
    pub quantity: f32,
    pub filled_quantity: f32,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderPanelSnapshot {
    pub symbol: String,
    pub best_bid: Option<f32>,
    pub best_ask: Option<f32>,
    pub last_price: Option<f32>,
    pub available_cash: Option<f32>,
    pub position_qty: Option<f32>,
    pub available_qty: Option<f32>,
    pub working_orders: Vec<WorkingOrder>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderSubmitResponse {
    pub order_id: String,
    pub status: String,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderCancelResponse {
    pub order_id: String,
    pub status: String,
    pub message: Option<String>,
}
