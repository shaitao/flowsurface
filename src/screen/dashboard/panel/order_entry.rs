use exchange::{
    adapter::Venue,
    TickerInfo, Trade,
    depth::Depth,
    order::{
        OrderBookLevel, OrderCancelRequest, OrderCancelResponse, OrderPanelSnapshot, OrderSide,
        OrderSubmitRequest, OrderSubmitResponse, OrderType, WorkingOrder,
    },
};
use iced::{
    Alignment, Element, Length,
    widget::{button, column, container, row, scrollable, text, text_input},
};
use std::time::Instant;

const ORDER_ENTRY_QUOTE_LEVELS: usize = 5;
const QMT_ORDER_LOT_SIZE: f32 = 100.0;

#[derive(Debug, Clone)]
pub enum Message {
    PriceChanged(String),
    QuantityChanged(String),
    QuoteLimitSubmit(OrderSide, f32),
    RefreshPressed,
    SubmitPressed(OrderSide, OrderType),
    CancelPressed(String),
}

#[derive(Debug, Clone)]
pub enum Action {
    RefreshSnapshot,
    Submit(OrderSubmitRequest),
    Cancel(OrderCancelRequest),
}

pub struct OrderEntry {
    ticker_info: TickerInfo,
    selected_side: OrderSide,
    selected_order_type: OrderType,
    price_input: String,
    quantity_input: String,
    best_bid: Option<f32>,
    best_ask: Option<f32>,
    last_price: Option<f32>,
    bid_levels: Vec<OrderBookLevel>,
    ask_levels: Vec<OrderBookLevel>,
    available_cash: Option<f32>,
    position_qty: Option<f32>,
    available_qty: Option<f32>,
    working_orders: Vec<WorkingOrder>,
    status_message: Option<String>,
    error_message: Option<String>,
    is_loading_snapshot: bool,
    is_submitting: bool,
    cancelling_order_id: Option<String>,
    last_tick: Instant,
}

impl OrderEntry {
    pub fn new(ticker_info: TickerInfo) -> Self {
        Self {
            ticker_info,
            selected_side: OrderSide::Buy,
            selected_order_type: OrderType::Limit,
            price_input: String::new(),
            quantity_input: String::new(),
            best_bid: None,
            best_ask: None,
            last_price: None,
            bid_levels: vec![],
            ask_levels: vec![],
            available_cash: None,
            position_qty: None,
            available_qty: None,
            working_orders: vec![],
            status_message: None,
            error_message: None,
            is_loading_snapshot: false,
            is_submitting: false,
            cancelling_order_id: None,
            last_tick: Instant::now(),
        }
    }

    pub fn ticker_info(&self) -> TickerInfo {
        self.ticker_info
    }

    pub fn last_update(&self) -> Instant {
        self.last_tick
    }

    pub fn begin_snapshot_refresh(&mut self) {
        self.is_loading_snapshot = true;
        self.error_message = None;
        self.status_message = Some("Refreshing bridge snapshot...".to_string());
    }

    pub fn apply_snapshot(&mut self, snapshot: OrderPanelSnapshot) {
        let OrderPanelSnapshot {
            symbol: _,
            best_bid,
            best_ask,
            last_price,
            bids,
            asks,
            available_cash,
            position_qty,
            available_qty,
            working_orders,
        } = snapshot;

        if !bids.is_empty() {
            self.bid_levels = normalize_bid_levels(bids);
            self.best_bid = self.bid_levels.first().map(|level| level.price);
        } else if self.bid_levels.is_empty() {
            self.bid_levels = best_bid
                .map(single_bid_level)
                .into_iter()
                .collect::<Vec<_>>();
            self.best_bid = best_bid.or(self.best_bid);
        } else {
            self.best_bid = best_bid.or(self.best_bid);
        }

        if !asks.is_empty() {
            self.ask_levels = normalize_ask_levels(asks);
            self.best_ask = self.ask_levels.first().map(|level| level.price);
        } else if self.ask_levels.is_empty() {
            self.ask_levels = best_ask
                .map(single_ask_level)
                .into_iter()
                .collect::<Vec<_>>();
            self.best_ask = best_ask.or(self.best_ask);
        } else {
            self.best_ask = best_ask.or(self.best_ask);
        }

        self.last_price = last_price.or(self.last_price);
        self.available_cash = available_cash;
        self.position_qty = position_qty;
        self.available_qty = available_qty;
        self.working_orders = working_orders;
        self.is_loading_snapshot = false;
        self.is_submitting = false;
        self.cancelling_order_id = None;
        self.error_message = None;
        self.status_message = Some("Bridge snapshot refreshed".to_string());
        self.last_tick = Instant::now();
    }

    pub fn apply_submit_result(&mut self, response: OrderSubmitResponse) {
        self.is_submitting = false;
        self.error_message = None;
        self.status_message = Some(
            response
                .message
                .unwrap_or_else(|| format!("Order {} {}", response.order_id, response.status)),
        );
        self.last_tick = Instant::now();
    }

    pub fn apply_cancel_result(&mut self, response: OrderCancelResponse) {
        self.cancelling_order_id = None;
        self.error_message = None;
        self.status_message = Some(
            response
                .message
                .unwrap_or_else(|| format!("Order {} {}", response.order_id, response.status)),
        );
        self.last_tick = Instant::now();
    }

    pub fn apply_request_error(&mut self, message: String) {
        self.is_loading_snapshot = false;
        self.is_submitting = false;
        self.cancelling_order_id = None;
        self.error_message = Some(message);
        self.status_message = None;
        self.last_tick = Instant::now();
    }

    pub fn insert_depth(&mut self, depth: &Depth) {
        self.bid_levels = depth
            .bids
            .iter()
            .rev()
            .take(ORDER_ENTRY_QUOTE_LEVELS)
            .map(|(price, qty)| OrderBookLevel {
                price: price.to_f32(),
                quantity: qty.to_f32_lossy(),
            })
            .collect();
        self.ask_levels = depth
            .asks
            .iter()
            .take(ORDER_ENTRY_QUOTE_LEVELS)
            .map(|(price, qty)| OrderBookLevel {
                price: price.to_f32(),
                quantity: qty.to_f32_lossy(),
            })
            .collect();
        self.best_bid = self.bid_levels.first().map(|level| level.price);
        self.best_ask = self.ask_levels.first().map(|level| level.price);
        self.last_tick = Instant::now();
    }

    pub fn insert_trades(&mut self, trades: &[Trade]) {
        if let Some(trade) = trades.last() {
            self.last_price = Some(trade.price.to_f32());
            self.last_tick = Instant::now();
        }
    }

    pub fn update(&mut self, message: Message) -> Option<Action> {
        match message {
            Message::PriceChanged(value) => {
                self.price_input = value;
            }
            Message::QuantityChanged(value) => {
                self.quantity_input = value;
            }
            Message::QuoteLimitSubmit(side, price) => {
                if self.is_submitting {
                    return None;
                }
                self.price_input = self.format_price(Some(price));
                self.selected_side = side;
                self.selected_order_type = OrderType::Limit;

                let Some(request) = self.build_submit_request(side, OrderType::Limit) else {
                    return None;
                };

                self.is_submitting = true;
                self.error_message = None;
                self.status_message = Some(format!("Submitting {} Limit @ {}...", side, self.format_price(Some(price))));
                return Some(Action::Submit(request));
            }
            Message::RefreshPressed => {
                if self.is_loading_snapshot {
                    return None;
                }
                self.begin_snapshot_refresh();
                return Some(Action::RefreshSnapshot);
            }
            Message::SubmitPressed(side, order_type) => {
                if self.is_submitting {
                    return None;
                }

                self.selected_side = side;
                self.selected_order_type = order_type;

                let Some(request) = self.build_submit_request(side, order_type) else {
                    return None;
                };

                self.is_submitting = true;
                self.error_message = None;
                self.status_message = Some(format!("Submitting {} {}...", side, order_type));
                return Some(Action::Submit(request));
            }
            Message::CancelPressed(order_id) => {
                if self.cancelling_order_id.is_some() {
                    return None;
                }
                self.cancelling_order_id = Some(order_id.clone());
                self.error_message = None;
                self.status_message = Some(format!("Cancelling order {order_id}..."));
                return Some(Action::Cancel(OrderCancelRequest { order_id }));
            }
        }

        None
    }

    pub fn view(&self) -> Element<'_, Message> {
        let market_buttons = row![
            order_button(
                "Buy Market",
                self.selected_side == OrderSide::Buy
                    && self.selected_order_type == OrderType::Market,
                !self.is_submitting,
                Some(Message::SubmitPressed(OrderSide::Buy, OrderType::Market)),
            ),
            iced::widget::space::horizontal(),
            order_button(
                "Sell Market",
                self.selected_side == OrderSide::Sell
                    && self.selected_order_type == OrderType::Market,
                !self.is_submitting,
                Some(Message::SubmitPressed(OrderSide::Sell, OrderType::Market)),
            ),
        ];

        let ask_ladder = quote_ladder_section(
            "Asks",
            &self.ask_levels,
            self.ticker_info,
            OrderSide::Buy,
            true,
        );

        let price_row = row![
            text_input("Price", &self.price_input)
                .on_input(Message::PriceChanged)
                .padding(4)
                .width(Length::Fixed(80.0)),
            order_button(
                "Sell Limit",
                self.selected_side == OrderSide::Sell
                    && self.selected_order_type == OrderType::Limit,
                !self.is_submitting,
                Some(Message::SubmitPressed(OrderSide::Sell, OrderType::Limit)),
            ),
        ]
        .spacing(4)
        .align_y(Alignment::Center);

        let qty_row = row![
            text_input(self.quantity_input_placeholder(), &self.quantity_input)
                .on_input(Message::QuantityChanged)
                .padding(4)
                .width(Length::Fixed(80.0)),
            order_button(
                "Buy Limit",
                self.selected_side == OrderSide::Buy
                    && self.selected_order_type == OrderType::Limit,
                !self.is_submitting,
                Some(Message::SubmitPressed(OrderSide::Buy, OrderType::Limit)),
            ),
        ]
        .spacing(4)
        .align_y(Alignment::Center);

        let bid_ladder = quote_ladder_section(
            "Bids",
            &self.bid_levels,
            self.ticker_info,
            OrderSide::Sell,
            false,
        );

        let feedback: Element<_> = if let Some(err) = &self.error_message {
            text(format!("Error: {err}")).size(12).into()
        } else if let Some(msg) = &self.status_message {
            text(msg.clone()).size(12).into()
        } else {
            text("").size(12).into()
        };

        let account_row = row![
            metric_box("Cash", format_optional_number(self.available_cash)),
            metric_box("Pos", format_optional_number(self.position_qty)),
            metric_box("Avail", format_optional_number(self.available_qty)),
            button(text(if self.is_loading_snapshot {
                "..."
            } else {
                "Refresh"
            }))
            .width(Length::Shrink)
            .on_press_maybe((!self.is_loading_snapshot).then_some(Message::RefreshPressed)),
        ]
        .spacing(4)
        .align_y(Alignment::Center);

        let working_orders: Element<_> = if self.working_orders.is_empty() {
            container(text("No working orders").size(12))
                .padding(4)
                .width(Length::Fill)
                .into()
        } else {
            let rows = self
                .working_orders
                .iter()
                .fold(column![].spacing(2), |column, order| {
                    let is_cancelling =
                        self.cancelling_order_id.as_deref() == Some(order.order_id.as_str());
                    let status = if is_cancelling {
                        "Cancelling..."
                    } else {
                        order.status.as_str()
                    };

                    column.push(
                        row![
                            text(format!(
                                "{} {} Px:{} Qty:{} Fill:{} {}",
                                order.side,
                                order.order_type,
                                self.format_price(order.price),
                                format_plain_number(order.quantity),
                                format_plain_number(order.filled_quantity),
                                status,
                            ))
                            .size(11)
                            .width(Length::Fill),
                            button(text(if is_cancelling { "..." } else { "Cancel" }).size(11))
                                .on_press_maybe(
                                    (!is_cancelling
                                        && self.cancelling_order_id.is_none()
                                        && !self.is_submitting)
                                        .then_some(Message::CancelPressed(
                                            order.order_id.clone(),
                                        )),
                                )
                        ]
                        .align_y(Alignment::Center)
                        .spacing(4),
                    )
                });

            scrollable(rows).height(Length::Fill).into()
        };

        let content = column![
            market_buttons,
            ask_ladder,
            price_row,
            qty_row,
            text(self.quantity_hint_text()).size(11),
            bid_ladder,
            feedback,
            account_row,
            text("Working Orders").size(12),
            working_orders,
        ]
        .spacing(4)
        .padding(6);

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn build_submit_request(
        &mut self,
        side: OrderSide,
        order_type: OrderType,
    ) -> Option<OrderSubmitRequest> {
        let quantity = match parse_positive_f32(&self.quantity_input) {
            Some(value) => value,
            None => {
                self.apply_request_error(self.invalid_quantity_message());
                return None;
            }
        };
        let quantity = quantity * self.order_quantity_scale();

        let price = match order_type {
            OrderType::Limit => match parse_positive_f32(&self.price_input) {
                Some(value) => Some(value),
                None => {
                    self.apply_request_error(
                        "Limit orders need a valid positive price".to_string(),
                    );
                    return None;
                }
            },
            OrderType::Market => None,
        };

        Some(OrderSubmitRequest {
            side,
            order_type,
            price,
            quantity,
        })
    }

    fn order_quantity_scale(&self) -> f32 {
        match self.ticker_info.exchange().venue() {
            Venue::SSZ | Venue::SSH => QMT_ORDER_LOT_SIZE,
            Venue::Binance => 1.0,
        }
    }

    fn quantity_input_placeholder(&self) -> &'static str {
        if self.order_quantity_scale() == QMT_ORDER_LOT_SIZE {
            "Lots"
        } else {
            "Quantity"
        }
    }

    fn quantity_hint_text(&self) -> String {
        if self.order_quantity_scale() == QMT_ORDER_LOT_SIZE {
            format!(
                "Min tick {}  Quantity input uses lots (1 lot = 100 shares)",
                f32::from(self.ticker_info.min_ticksize),
            )
        } else {
            format!(
                "Min tick {}  Min qty {}",
                f32::from(self.ticker_info.min_ticksize),
                f32::from(self.ticker_info.min_qty)
            )
        }
    }

    fn invalid_quantity_message(&self) -> String {
        if self.order_quantity_scale() == QMT_ORDER_LOT_SIZE {
            "Quantity (lots) must be a positive number".to_string()
        } else {
            "Quantity must be a positive number".to_string()
        }
    }

    fn format_price(&self, value: Option<f32>) -> String {
        value.map_or_else(
            || "—".to_string(),
            |value| {
                exchange::unit::Price::from_f32(value)
                    .round_to_min_tick(self.ticker_info.min_ticksize)
                    .to_string(self.ticker_info.min_ticksize)
            },
        )
    }
}

fn normalize_bid_levels(mut levels: Vec<OrderBookLevel>) -> Vec<OrderBookLevel> {
    levels.sort_by(|left, right| right.price.total_cmp(&left.price));
    levels.truncate(ORDER_ENTRY_QUOTE_LEVELS);
    levels
}

fn normalize_ask_levels(mut levels: Vec<OrderBookLevel>) -> Vec<OrderBookLevel> {
    levels.sort_by(|left, right| left.price.total_cmp(&right.price));
    levels.truncate(ORDER_ENTRY_QUOTE_LEVELS);
    levels
}

fn single_bid_level(price: f32) -> OrderBookLevel {
    OrderBookLevel {
        price,
        quantity: 0.0,
    }
}

fn single_ask_level(price: f32) -> OrderBookLevel {
    OrderBookLevel {
        price,
        quantity: 0.0,
    }
}

fn order_button<'a>(
    label: &'a str,
    active: bool,
    enabled: bool,
    message: Option<Message>,
) -> iced::widget::Button<'a, Message> {
    let label = if active {
        format!("[{label}]")
    } else {
        label.to_string()
    };
    button(text(label)).on_press_maybe(enabled.then_some(message).flatten())
}

fn metric_box<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    container(column![text(label).size(11), text(value).size(13)].spacing(2))
        .padding(4)
        .width(Length::FillPortion(1))
        .into()
}

fn quote_ladder_section<'a>(
    label: &'a str,
    levels: &[OrderBookLevel],
    ticker_info: TickerInfo,
    click_side: OrderSide,
    reverse: bool,
) -> Element<'a, Message> {
    let mut rows = column![].spacing(2).width(Length::Fill);

    if levels.is_empty() {
        rows = rows.push(
            container(text("—").size(12))
                .padding(4)
                .width(Length::Fill),
        );
    } else {
        let ordered: Vec<_> = if reverse {
            levels.iter().rev().collect()
        } else {
            levels.iter().collect()
        };
        for level in ordered {
            let price = exchange::unit::Price::from_f32(level.price)
                .round_to_min_tick(ticker_info.min_ticksize)
                .to_string(ticker_info.min_ticksize);
            let quantity = format_plain_number(level.quantity);

            rows = rows.push(
                button(
                    row![
                        text(price).size(12).width(Length::FillPortion(1)),
                        text(quantity).size(11).width(Length::FillPortion(1)),
                    ]
                    .spacing(6)
                    .width(Length::Fill)
                    .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .on_press(Message::QuoteLimitSubmit(click_side, level.price)),
            );
        }
    }

    column![text(label).size(11), rows]
        .spacing(2)
        .width(Length::Fill)
        .into()
}

fn format_optional_number(value: Option<f32>) -> String {
    value.map_or_else(|| "—".to_string(), format_plain_number)
}

fn format_plain_number(value: f32) -> String {
    let rendered = format!("{value:.4}");
    rendered
        .trim_end_matches('0')
        .trim_end_matches('.')
        .to_string()
}

fn parse_positive_f32(value: &str) -> Option<f32> {
    let parsed = value.trim().parse::<f32>().ok()?;
    (parsed > 0.0).then_some(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use exchange::{Ticker, adapter::Exchange};

    fn qmt_order_entry() -> OrderEntry {
        OrderEntry::new(TickerInfo::new(
            Ticker::new("600309.SH", Exchange::SSH),
            0.01,
            1.0,
            None,
        ))
    }

    #[test]
    fn qmt_market_order_quantity_is_scaled_from_lots_to_shares() {
        let mut order_entry = qmt_order_entry();
        order_entry.quantity_input = "2".to_string();

        let request = order_entry
            .build_submit_request(OrderSide::Buy, OrderType::Market)
            .expect("market order request should be built");

        assert_eq!(request.quantity, 200.0);
    }
}
