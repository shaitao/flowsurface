use exchange::{
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

#[derive(Debug, Clone)]
pub enum Message {
    PriceChanged(String),
    QuantityChanged(String),
    QuotePriceSelected(f32),
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
            Message::QuotePriceSelected(price) => {
                self.selected_order_type = OrderType::Limit;
                self.price_input = self.format_price(Some(price));
                self.error_message = None;
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
        let quote_summary = row![
            metric_box("Bid", self.format_price(self.best_bid)),
            metric_box("Ask", self.format_price(self.best_ask)),
            metric_box("Last", self.format_price(self.last_price)),
        ]
        .spacing(8);

        let account_summary = row![
            metric_box("Cash", format_optional_number(self.available_cash)),
            metric_box("Position", format_optional_number(self.position_qty)),
            metric_box("Available", format_optional_number(self.available_qty)),
        ]
        .spacing(8);

        let quick_buttons = row![
            order_button(
                "Buy Market",
                self.selected_side == OrderSide::Buy
                    && self.selected_order_type == OrderType::Market,
                !self.is_submitting,
                Some(Message::SubmitPressed(OrderSide::Buy, OrderType::Market)),
            ),
            order_button(
                "Buy Limit",
                self.selected_side == OrderSide::Buy
                    && self.selected_order_type == OrderType::Limit,
                !self.is_submitting,
                Some(Message::SubmitPressed(OrderSide::Buy, OrderType::Limit)),
            ),
            order_button(
                "Sell Market",
                self.selected_side == OrderSide::Sell
                    && self.selected_order_type == OrderType::Market,
                !self.is_submitting,
                Some(Message::SubmitPressed(OrderSide::Sell, OrderType::Market)),
            ),
            order_button(
                "Sell Limit",
                self.selected_side == OrderSide::Sell
                    && self.selected_order_type == OrderType::Limit,
                !self.is_submitting,
                Some(Message::SubmitPressed(OrderSide::Sell, OrderType::Limit)),
            ),
        ]
        .spacing(8);

        let controls = row![
            text_input("Price", &self.price_input)
                .on_input(Message::PriceChanged)
                .padding(8),
            text_input("Quantity", &self.quantity_input)
                .on_input(Message::QuantityChanged)
                .padding(8),
            button(text(if self.is_loading_snapshot {
                "Refreshing..."
            } else {
                "Refresh"
            }))
            .width(Length::Shrink)
            .on_press_maybe((!self.is_loading_snapshot).then_some(Message::RefreshPressed)),
        ]
        .spacing(8)
        .align_y(Alignment::Center);

        let quote_ladder = row![
            quote_column("Ask 5", &self.ask_levels, self.ticker_info),
            quote_column("Bid 5", &self.bid_levels, self.ticker_info),
        ]
        .spacing(8);

        let status_line = self
            .status_message
            .as_ref()
            .map(|message| text(message.clone()).size(13));
        let error_line = self
            .error_message
            .as_ref()
            .map(|message| text(format!("Error: {message}")).size(13));

        let working_orders: Element<_> = if self.working_orders.is_empty() {
            container(text("No working orders").size(13))
                .padding(8)
                .width(Length::Fill)
                .into()
        } else {
            let rows = self
                .working_orders
                .iter()
                .fold(column![].spacing(6), |column, order| {
                    let is_cancelling =
                        self.cancelling_order_id.as_deref() == Some(order.order_id.as_str());
                    let status = if is_cancelling {
                        "Cancelling..."
                    } else {
                        order.status.as_str()
                    };

                    column.push(
                        container(
                            row![
                                column![
                                    text(format!("{} {}", order.side, order.order_type)).size(13),
                                    text(format!(
                                        "Px {}  Qty {}  Fill {}  {}",
                                        self.format_price(order.price),
                                        format_plain_number(order.quantity),
                                        format_plain_number(order.filled_quantity),
                                        status,
                                    ))
                                    .size(12),
                                    text(order.order_id.clone()).size(11),
                                ]
                                .spacing(2)
                                .width(Length::Fill),
                                button(text(if is_cancelling {
                                    "Cancelling..."
                                } else {
                                    "Cancel"
                                }))
                                .on_press_maybe(
                                    (!is_cancelling
                                        && self.cancelling_order_id.is_none()
                                        && !self.is_submitting)
                                        .then_some(Message::CancelPressed(order.order_id.clone(),)),
                                )
                            ]
                            .align_y(Alignment::Center)
                            .spacing(8),
                        )
                        .padding(8),
                    )
                });

            scrollable(rows).height(Length::Fill).into()
        };

        let content = column![
            quote_summary,
            account_summary,
            quote_ladder,
            text("Click a quote to fill the limit price. Market buttons ignore the price field.")
                .size(12),
            controls,
            quick_buttons,
            text(format!(
                "Min tick {}  Min qty {}",
                f32::from(self.ticker_info.min_ticksize),
                f32::from(self.ticker_info.min_qty)
            ))
            .size(12),
            status_line
                .map(Element::from)
                .unwrap_or_else(|| container(text("")).into()),
            error_line
                .map(Element::from)
                .unwrap_or_else(|| container(text("")).into()),
            text("Working Orders").size(14),
            working_orders,
        ]
        .spacing(10)
        .padding(12);

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
                self.apply_request_error("Quantity must be a positive number".to_string());
                return None;
            }
        };

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
    container(column![text(label).size(12), text(value).size(15)].spacing(4))
        .padding(8)
        .width(Length::FillPortion(1))
        .into()
}

fn quote_column<'a>(
    label: &'a str,
    levels: &[OrderBookLevel],
    ticker_info: TickerInfo,
) -> Element<'a, Message> {
    let mut content = column![text(label).size(12)].spacing(6);

    if levels.is_empty() {
        content = content.push(container(text("—").size(13)).padding(8).width(Length::Fill));
    } else {
        for level in levels {
            let price = exchange::unit::Price::from_f32(level.price)
                .round_to_min_tick(ticker_info.min_ticksize)
                .to_string(ticker_info.min_ticksize);
            let quantity = format_plain_number(level.quantity);

            content = content.push(
                button(
                    row![text(price).size(13), text(quantity).size(12),]
                        .spacing(8)
                        .width(Length::Fill)
                        .align_y(Alignment::Center),
                )
                .width(Length::Fill)
                .on_press(Message::QuotePriceSelected(level.price)),
            );
        }
    }

    container(content)
        .padding(8)
        .width(Length::FillPortion(1))
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
