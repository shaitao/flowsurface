use super::*;
use tokio::sync::{Mutex, broadcast};

const SHARED_TICK_CHANNEL_CAPACITY: usize = 1_024;
const SHARED_TICK_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const SHARED_TICK_IDLE_POLL_INTERVAL: Duration = Duration::from_secs(1);

static SHARED_TICK_STREAMS: LazyLock<
    Mutex<FxHashMap<Ticker, broadcast::Sender<SharedTickStreamEvent>>>,
> = LazyLock::new(|| Mutex::new(FxHashMap::default()));

#[derive(Debug, Clone)]
pub(super) enum SharedTickStreamEvent {
    Connected,
    Tick(QmtTick),
    Disconnected(String),
}

pub(super) async fn subscribe_shared_tick_stream(
    ticker_info: TickerInfo,
) -> broadcast::Receiver<SharedTickStreamEvent> {
    let ticker = ticker_info.ticker;
    let mut streams = SHARED_TICK_STREAMS.lock().await;

    if let Some(sender) = streams.get(&ticker) {
        return sender.subscribe();
    }

    let (sender, receiver) = broadcast::channel(SHARED_TICK_CHANNEL_CAPACITY);
    streams.insert(ticker, sender.clone());
    drop(streams);

    tokio::spawn(run_shared_tick_stream(ticker_info, sender));

    receiver
}

async fn run_shared_tick_stream(
    ticker_info: TickerInfo,
    sender: broadcast::Sender<SharedTickStreamEvent>,
) {
    let ticker = ticker_info.ticker;

    loop {
        if maybe_stop_shared_tick_stream(ticker, &sender).await {
            return;
        }

        let (domain, url) = match qmt_bridge_ws_url("/ws/tick", &[("symbol", ticker.to_string())]) {
            Ok(parts) => parts,
            Err(error) => {
                let _ = sender.send(SharedTickStreamEvent::Disconnected(error.to_string()));
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
        };

        match connect_ws(&domain, &url).await {
            Ok(mut websocket) => {
                let _ = sender.send(SharedTickStreamEvent::Connected);

                loop {
                    match tokio::time::timeout(
                        SHARED_TICK_IDLE_POLL_INTERVAL,
                        websocket.read_frame(),
                    )
                    .await
                    {
                        Err(_) => {
                            if sender.receiver_count() == 0 {
                                break;
                            }
                        }
                        Ok(Ok(message)) => match message.opcode {
                            OpCode::Text | OpCode::Binary => {
                                match decode_bridge_ws_message(message.opcode, &message.payload[..])
                                {
                                    Ok(BridgeWsMessage::Tick(tick)) => {
                                        cache_live_tick(ticker, &tick);
                                        let _ = sender.send(SharedTickStreamEvent::Tick(tick));
                                    }
                                    Ok(BridgeWsMessage::Status(status))
                                        if status.error.is_some()
                                            || status.phase.as_deref()
                                                == Some("callback_error") =>
                                    {
                                        let message =
                                            status.error.or(status.phase).unwrap_or_else(|| {
                                                "QMT bridge reported an error".to_string()
                                            });
                                        let _ = sender
                                            .send(SharedTickStreamEvent::Disconnected(message));
                                        break;
                                    }
                                    Ok(BridgeWsMessage::Status(_)) => {}
                                    Err(error) => {
                                        let _ = sender.send(SharedTickStreamEvent::Disconnected(
                                            format!("Invalid QMT tick payload: {error}"),
                                        ));
                                        break;
                                    }
                                }
                            }
                            OpCode::Close => {
                                let _ = sender.send(SharedTickStreamEvent::Disconnected(
                                    "QMT websocket closed".to_string(),
                                ));
                                break;
                            }
                            _ => {}
                        },
                        Ok(Err(error)) => {
                            let _ = sender.send(SharedTickStreamEvent::Disconnected(format!(
                                "QMT websocket read failed: {error}"
                            )));
                            break;
                        }
                    }
                }
            }
            Err(error) => {
                let _ = sender.send(SharedTickStreamEvent::Disconnected(error.to_string()));
            }
        }

        if sender.receiver_count() > 0 {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

async fn maybe_stop_shared_tick_stream(
    ticker: Ticker,
    sender: &broadcast::Sender<SharedTickStreamEvent>,
) -> bool {
    if sender.receiver_count() != 0 {
        return false;
    }

    tokio::time::sleep(SHARED_TICK_IDLE_TIMEOUT).await;

    if sender.receiver_count() != 0 {
        return false;
    }

    let mut streams = SHARED_TICK_STREAMS.lock().await;
    if sender.receiver_count() != 0 {
        return false;
    }

    streams.remove(&ticker);
    true
}

fn decode_bridge_ws_message(
    opcode: OpCode,
    payload: &[u8],
) -> Result<BridgeWsMessage, AdapterError> {
    match opcode {
        OpCode::Text => {
            serde_json::from_slice(payload).map_err(|e| AdapterError::ParseError(e.to_string()))
        }
        OpCode::Binary => {
            let decompressed = qmt_decompress_zstd(payload)?;
            qmt_decode_msgpack(&decompressed)
        }
        _ => Err(AdapterError::ParseError(format!(
            "unsupported websocket opcode: {opcode:?}"
        ))),
    }
}
