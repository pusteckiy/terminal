//! Multiplex public Hyperliquid feeds: one socket for all selected spot/perp markets.
use super::*;
use std::collections::{HashMap, HashSet};
use tokio::sync::watch;

const CHANNELS: [&str; 3] = ["l2Book", "bbo", "trades"];
type Subscription = (Market, &'static str);

fn plan(sent: &HashSet<Subscription>, desired: &[Market]) -> VecDeque<(Subscription, bool)> {
    let wanted: HashSet<_> = desired
        .iter()
        .flat_map(|market| CHANNELS.map(|channel| (market.clone(), channel)))
        .collect();
    let mut commands: Vec<_> = sent
        .difference(&wanted)
        .cloned()
        .map(|key| (key, false))
        .chain(wanted.difference(sent).cloned().map(|key| (key, true)))
        .collect();
    commands.sort_by(|a, b| (a.1, &a.0.0.symbol, a.0.1).cmp(&(b.1, &b.0.0.symbol, b.0.1)));
    commands.into()
}

pub(crate) async fn run(state: AppState, mut control: watch::Receiver<Vec<Market>>) {
    let mut failures = 0_u32;
    loop {
        if control.borrow().is_empty() && control.changed().await.is_err() {
            return;
        }
        if control.borrow().is_empty() {
            continue;
        }
        let connected_at = tokio::time::Instant::now();
        let result = stream(&state, &mut control).await;
        if result.is_ok() {
            failures = 0;
            continue;
        }
        eprintln!("Hyperliquid public stream: {}", result.unwrap_err());
        if connected_at.elapsed() > Duration::from_secs(30) {
            failures = 0;
        }
        failures = failures.saturating_add(1);
        let markets = control.borrow().clone();
        for market in markets {
            state.disconnected(&market).await;
        }
        tokio::time::sleep(Duration::from_secs((2_u64.pow(failures.min(5))).min(30))).await;
    }
}

async fn stream(state: &AppState, control: &mut watch::Receiver<Vec<Market>>) -> Result<(), Error> {
    let (mut socket, _) = connect_public("wss://api.hyperliquid.xyz/ws").await?;
    let mut desired = control.borrow_and_update().clone();
    let mut sent = HashSet::new();
    let mut pending = plan(&sent, &desired);
    // 25 outbound commands/s, with room under 2000 messages/min for heartbeats/accounts.
    let mut commands = tokio::time::interval(Duration::from_millis(40));
    commands.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(30));
    heartbeat.tick().await;
    loop {
        if desired.is_empty() && pending.is_empty() {
            return Ok(());
        }
        tokio::select! {
            changed = control.changed() => {
                if changed.is_err() { return Ok(()); }
                desired = control.borrow_and_update().clone();
                // Rebuild from commands actually sent, so rapid switching cancels stale work.
                pending = plan(&sent, &desired);
            }
            _ = commands.tick(), if !pending.is_empty() => {
                let ((market, channel), subscribe) = pending.pop_front().unwrap();
                socket.send(Message::Text(json!({"method":if subscribe {"subscribe"} else {"unsubscribe"},"subscription":{"type":channel,"coin":market.symbol}}).to_string().into())).await?;
                if subscribe { sent.insert((market, channel)); }
                else { sent.remove(&(market, channel)); }
            }
            _ = heartbeat.tick() => {
                socket.send(Message::Text(r#"{"method":"ping"}"#.into())).await?;
            }
            frame = socket.next() => {
                match frame {
                    Some(Ok(Message::Text(text))) => {
                        let value: Value = serde_json::from_str(&text)?;
                        publish(state, &desired, &value).await?;
                    }
                    Some(Ok(Message::Ping(data))) => socket.send(Message::Pong(data)).await?,
                    Some(Ok(Message::Close(_))) | None => return Err("Hyperliquid public socket closed".into()),
                    Some(Err(error)) => return Err(error.into()),
                    _ => {}
                }
            }
        }
    }
}

async fn publish(state: &AppState, desired: &[Market], value: &Value) -> Result<(), Error> {
    if value["channel"] == "error" {
        return Err(format!("subscription rejected: {}", value["data"]).into());
    }
    if value["channel"] == "trades" {
        // A frame may contain multiple coins; retain every item and its venue timestamp.
        let mut groups = HashMap::<Market, Vec<Value>>::new();
        for row in value["data"].as_array().map_or(&[][..], Vec::as_slice) {
            if let Some(market) = desired
                .iter()
                .find(|market| Some(market.symbol.as_str()) == row["coin"].as_str())
            {
                groups.entry(market.clone()).or_default().push(row.clone());
            }
        }
        for (market, rows) in groups {
            let trades = parse_trades(
                Exchange::Hyperliquid,
                &json!({"channel":"trades","data":rows}),
            );
            if !trades.is_empty() {
                state.publish_trades(&market, trades).await;
            }
        }
        return Ok(());
    }
    let Some(market) = desired
        .iter()
        .find(|market| Some(market.symbol.as_str()) == value["data"]["coin"].as_str())
    else {
        return Ok(());
    };
    if let Some(quote) = parse_hyperliquid_bbo(value) {
        state.publish_best_bid_ask(market, quote).await;
    }
    if value["channel"] == "l2Book" {
        let mut book = BookAccumulator::default();
        if !book.replace(
            &value["data"]["levels"][0],
            &value["data"]["levels"][1],
            None,
        ) || book.is_crossed()
        {
            return Err("invalid Hyperliquid book".into());
        }
        state
            .publish_book(
                market,
                book.book(integer(&value["data"]["time"]).unwrap_or_else(now_ms)),
            )
            .await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn switching_cancels_unsent_work_and_preserves_existing_subscriptions() {
        let btc = Market::for_exchange(Exchange::Hyperliquid);
        let mut eth = btc.clone();
        eth.symbol = "ETH".into();
        let sent = HashSet::from([(btc.clone(), "bbo")]);
        let queue = plan(&sent, &[btc.clone(), eth]);
        assert_eq!(queue.len(), 5);
        assert!(queue.iter().all(|(_, subscribe)| *subscribe));
        assert!(
            plan(&sent, &[btc])
                .iter()
                .all(|((_, channel), _)| *channel != "bbo")
        );
        assert!(!plan(&sent, &[]).front().unwrap().1);
    }

    #[tokio::test]
    async fn shared_frames_route_each_trade_to_its_exact_market() {
        let state = crate::tests::test_state();
        let btc = Market::for_exchange(Exchange::Hyperliquid);
        let mut spot = btc.clone();
        spot.kind = MarketKind::Spot;
        spot.symbol = "@1".into();
        let mut xyz = btc.clone();
        xyz.symbol = "xyz:NVDA".into();
        let mut flx = btc.clone();
        flx.symbol = "flx:NVDA".into();
        publish(
            &state,
            &[btc.clone(), spot.clone(), xyz.clone(), flx.clone()],
            &json!({"channel":"trades","data":[
                {"coin":"BTC","px":"100","sz":"2","time":1000,"side":"B"},
                {"coin":"@1","px":"0.5","sz":"3","time":1001,"side":"A"},
                {"coin":"xyz:NVDA","px":"180","sz":"1","time":1002,"side":"B"},
                {"coin":"flx:NVDA","px":"181","sz":"4","time":1003,"side":"A"}
            ]}),
        )
        .await
        .unwrap();
        for (market, price) in [(&btc, "100"), (&spot, "0.5"), (&xyz, "180"), (&flx, "181")] {
            assert!(
                matches!(state.snapshot(market).await, terminal_core::ServerMessage::Snapshot {last_price: Some(last), ..} if last.price == price)
            );
        }
    }
}
