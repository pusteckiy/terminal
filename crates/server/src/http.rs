//! Shared public REST transport. Metrics contain endpoint paths, never query values or wallets.
use serde::Serialize;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Client {
    inner: reqwest::Client,
    stats: Arc<Mutex<BTreeMap<String, Stats>>>,
    gates: Arc<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<Gate>>>>>,
}

#[derive(Default, Clone, Serialize)]
pub struct Stats {
    requests: u64,
    errors: u64,
    rate_limited: u64,
    header_ms_total: u64,
    header_ms_max: u64,
    wait_ms_total: u64,
}

struct Gate {
    next: tokio::time::Instant,
    blocked_until: tokio::time::Instant,
}
impl Default for Gate {
    fn default() -> Self {
        let now = tokio::time::Instant::now();
        Self {
            next: now,
            blocked_until: now,
        }
    }
}

async fn wait_for_slot(gate: &tokio::sync::Mutex<Gate>, spacing: Duration) {
    loop {
        let mut state = gate.lock().await;
        let now = tokio::time::Instant::now();
        let available = state.next.max(state.blocked_until);
        if available <= now {
            state.next = now + spacing;
            return;
        }
        drop(state);
        // Recheck after waking: a concurrent 429 can extend the shared cooldown.
        tokio::time::sleep_until(available).await;
    }
}

fn request_spacing(request: &reqwest::Request) -> Duration {
    let host = request.url().host_str().unwrap_or("");
    let path = request.url().path();
    let limit = request
        .url()
        .query_pairs()
        .find(|(key, _)| key == "limit")
        .and_then(|(_, value)| value.parse::<u64>().ok())
        .unwrap_or(100);
    let ms = match host {
        "api.hyperliquid.xyz" => 3000, // Reserve 30 weight/request; <=600 weight/min.
        "mainnet.zklighter.elliot.ai" | "api.kraken.com" => 1100,
        "api.pacifica.fi" => 8000, // Reserve 12 credits/request, <=90 credits/min unauthenticated.
        "api.binance.com" => {
            let weight = if path.ends_with("/depth") {
                match limit {
                    0..=100 => 5,
                    101..=500 => 25,
                    501..=1000 => 50,
                    _ => 250,
                }
            } else if path.ends_with("/exchangeInfo") {
                20
            } else {
                2
            };
            weight * 34 // <1800 weight/min (budget includes deep snapshots).
        }
        "fapi.binance.com" => {
            let weight = if path.ends_with("/depth") {
                20
            } else if path.ends_with("/exchangeInfo") {
                1
            } else {
                2
            };
            weight * 67 // <900 weight/min.
        }
        "fapi.asterdex.com" | "sapi.asterdex.com" => {
            let weight = if path.ends_with("/depth") { 20 } else { 5 };
            weight * 67 // Budget expensive snapshots too, not just request count.
        }
        _ => 250, // Conservative public-read pacing, shared by all markets on a host.
    };
    Duration::from_millis(ms)
}

fn cooldown(response: &reqwest::Response) -> Option<Duration> {
    if !matches!(response.status().as_u16(), 418 | 429)
        && !(response.status().as_u16() == 405
            && response.url().host_str() == Some("mainnet.zklighter.elliot.ai"))
    {
        return None;
    }
    let seconds = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value.parse::<u64>().ok().or_else(|| {
                httpdate::parse_http_date(value).ok().map(|date| {
                    date.duration_since(std::time::SystemTime::now())
                        .unwrap_or_default()
                        .as_secs()
                        .saturating_add(1)
                })
            })
        });
    Some(Duration::from_secs(
        seconds
            .unwrap_or(if response.status().as_u16() == 418 {
                120
            } else {
                60
            })
            .max(1),
    ))
}

pub struct Request {
    inner: reqwest::RequestBuilder,
    client: Client,
}

impl Client {
    pub fn new() -> Self {
        Self {
            inner: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(3))
                .timeout(Duration::from_secs(10))
                .build()
                .expect("HTTP client"),
            stats: Arc::default(),
            gates: Arc::default(),
        }
    }
    pub fn get(&self, url: impl reqwest::IntoUrl) -> Request {
        Request {
            inner: self.inner.get(url),
            client: self.clone(),
        }
    }
    pub fn post(&self, url: impl reqwest::IntoUrl) -> Request {
        Request {
            inner: self.inner.post(url),
            client: self.clone(),
        }
    }
    pub fn metrics(&self) -> BTreeMap<String, Stats> {
        self.stats.lock().unwrap().clone()
    }
}

impl Request {
    pub fn query<T: Serialize + ?Sized>(mut self, query: &T) -> Self {
        self.inner = self.inner.query(query);
        self
    }
    pub fn json<T: Serialize + ?Sized>(mut self, body: &T) -> Self {
        self.inner = self.inner.json(body);
        self
    }
    pub async fn send(self) -> Result<reqwest::Response, reqwest::Error> {
        let request = self.inner.build()?;
        let key = format!(
            "{}{}",
            request.url().host_str().unwrap_or("unknown"),
            request.url().path()
        );
        let host = request.url().host_str().unwrap_or("unknown").to_owned();
        let gate = self
            .client
            .gates
            .lock()
            .unwrap()
            .entry(host)
            .or_default()
            .clone();
        let waiting = Instant::now();
        wait_for_slot(&gate, request_spacing(&request)).await;
        let waited = waiting.elapsed().as_millis() as u64;
        let started = Instant::now();
        let result = self.client.inner.execute(request).await;
        let elapsed = started.elapsed().as_millis() as u64;
        if let Ok(response) = &result
            && let Some(delay) = cooldown(response)
        {
            let mut state = gate.lock().await;
            state.blocked_until = state.blocked_until.max(tokio::time::Instant::now() + delay);
        }
        let mut stats = self.client.stats.lock().unwrap();
        let stats = stats.entry(key).or_default();
        stats.requests += 1;
        stats.errors += u64::from(
            result
                .as_ref()
                .map_or(true, |response| !response.status().is_success()),
        );
        stats.rate_limited += u64::from(
            result
                .as_ref()
                .is_ok_and(|response| matches!(response.status().as_u16(), 418 | 429)),
        );
        stats.header_ms_total += elapsed;
        stats.header_ms_max = stats.header_ms_max.max(elapsed);
        stats.wait_ms_total += waited;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn queued_request_rechecks_a_shared_cooldown() {
        let gate = Arc::new(tokio::sync::Mutex::new(Gate::default()));
        wait_for_slot(&gate, Duration::from_secs(1)).await;
        let queued = {
            let gate = gate.clone();
            tokio::spawn(async move {
                wait_for_slot(&gate, Duration::from_secs(1)).await;
            })
        };
        tokio::task::yield_now().await;
        let now = tokio::time::Instant::now();
        gate.lock().await.blocked_until = now + Duration::from_secs(60);
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert!(
            !queued.is_finished(),
            "another market must not bypass a 429 cooldown"
        );
        tokio::time::advance(Duration::from_secs(58)).await;
        queued.await.unwrap();
    }

    #[test]
    fn weighted_depth_cost_and_retry_after_are_respected() {
        let client = reqwest::Client::new();
        let request = client
            .get("https://api.binance.com/api/v3/depth?limit=5000")
            .build()
            .unwrap();
        assert_eq!(request_spacing(&request), Duration::from_millis(8500));
        for (header, minimum) in [
            ("120".to_owned(), 120),
            (
                httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(180)),
                179,
            ),
        ] {
            let response: reqwest::Response = axum::http::Response::builder()
                .status(429)
                .header("retry-after", header)
                .body("")
                .unwrap()
                .into();
            assert!(cooldown(&response).unwrap().as_secs() >= minimum);
        }
    }
}
