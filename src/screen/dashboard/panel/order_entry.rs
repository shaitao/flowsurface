use exchange::{
    TickerInfo, Trade,
    depth::Depth,
    order::{
        OrderCancelRequest, OrderCancelResponse, OrderPanelSnapshot, OrderSide, OrderSubmitRequest,
        OrderSubmitResponse, OrderType, WorkingOrder,
    },
};
use iced::{
    Alignment, Element, Length,
    widget::{button, column, container, row, scrollable, text, text_input},
};
use std::time::Instant;

#[derive(Debug, Clone)]
pub enum Message {
    SideSelected(OrderSide),
    OrderTypeSelected(OrderType),
    PriceChanged(String),
    QuantityChanged(String),
    RefreshPressed,
    SubmitPressed,
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
        self.best_bid = snapshot.best_bid.or(self.best_bid);
        self.best_ask = snapshot.best_ask.or(self.best_ask);
        self.last_price = snapshot.last_price.or(self.last_price);
        self.available_cash = snapshot.available_cash;
        self.position_qty = snapshot.position_qty;
        self.available_qty = snapshot.available_qty;
        self.working_orders = snapshot.working_orders;
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
        self.best_bid = depth.bids.last_key_value().map(|(price, _)| price.to_f32());
        self.best_ask = depth
            .asks
            .first_key_value()
            .map(|(price, _)| price.to_f32());
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
            Message::SideSelected(side) => {
                self.selected_side = side;
            }
            Message::OrderTypeSelected(order_type) => {
                self.selected_order_type = order_type;
            }
            Message::PriceChanged(value) => {
                self.price_input = value;
            }
            Message::QuantityChanged(value) => {
                self.quantity_input = value;
            }
            Message::RefreshPressed => {
                self.begin_snapshot_refresh();
                return Some(Action::RefreshSnapshot);
            }
            Message::SubmitPressed => {
                let quantity = match parse_positive_f32(&self.quantity_input) {
                    Some(value) => value,
                    None => {
                        self.apply_request_error("Quantity must be a positive number".to_string());
                        return None;
                    }
                };

                let price = match self.selected_order_type {
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

                self.is_submitting = true;
                self.error_message = None;
                self.status_message = Some("Submitting order...".to_string());

                return Some(Action::Submit(OrderSubmitRequest {
                    side: self.selected_side,
                    order_type: self.selected_order_type,
                    price,
                    quantity,
                }));
            }
            Message::CancelPressed(order_id) => {
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

        let order_type_label = format!("Type: {}", self.selected_order_type);
        let price_editor: Element<_> = match self.selected_order_type {
            OrderType::Limit => text_input("Price", &self.price_input)
                .on_input(Message::PriceChanged)
                .padding(8)
                .into(),
            OrderType::Market => container(text("Market order uses bridge-side pricing"))
                .padding(8)
                .width(Length::Fill)
                .into(),
        };

        let submit_label = if self.is_submitting {
            "Submitting..."
        } else {
            "Submit"
        };

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
                    let status =
                        if self.cancelling_order_id.as_deref() == Some(order.order_id.as_str()) {
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
                                button(text("Cancel"))
                                    .on_press(Message::CancelPressed(order.order_id.clone()))
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
            row![
                toggle_button("Buy", self.selected_side == OrderSide::Buy)
                    .on_press(Message::SideSelected(OrderSide::Buy)),
                toggle_button("Sell", self.selected_side == OrderSide::Sell)
                    .on_press(Message::SideSelected(OrderSide::Sell)),
                toggle_button("Limit", self.selected_order_type == OrderType::Limit)
                    .on_press(Message::OrderTypeSelected(OrderType::Limit)),
                toggle_button("Market", self.selected_order_type == OrderType::Market)
                    .on_press(Message::OrderTypeSelected(OrderType::Market)),
                button(text("Refresh")).on_press(Message::RefreshPressed),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
            text(order_type_label).size(12),
            row![
                price_editor,
                text_input("Quantity", &self.quantity_input)
                    .on_input(Message::QuantityChanged)
                    .padding(8),
                button(text(submit_label)).on_press(Message::SubmitPressed),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
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

fn toggle_button<'a>(label: &'a str, active: bool) -> iced::widget::Button<'a, Message> {
    let label = if active {
        format!("[{label}]")
    } else {
        label.to_string()
    };
    button(text(label))
}

fn metric_box<'a>(label: &'a str, value: String) -> Element<'a, Message> {
    container(column![text(label).size(12), text(value).size(15)].spacing(4))
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
