use std::{
    collections::HashMap,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};
use terminal_core::{Exchange, MarketKind, SymbolInfo};
use tokio::sync::Mutex;

type Key = (Exchange, MarketKind);
#[derive(Default)]
struct Entry {
    value: Option<Vec<SymbolInfo>>,
    error: Option<String>,
    expires: Option<Instant>,
}

#[derive(Default)]
pub struct Cache {
    entries: Mutex<HashMap<Key, Arc<Mutex<Entry>>>>,
}

impl Cache {
    pub async fn get_or_fetch<F, Fut>(&self, key: Key, fetch: F) -> Result<Vec<SymbolInfo>, String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Vec<SymbolInfo>, String>>,
    {
        let entry = self.entries.lock().await.entry(key).or_default().clone();
        // Only consumers of this venue/market type wait for its in-flight fetch.
        let mut entry = entry.lock().await;
        if entry
            .expires
            .is_some_and(|expires| Instant::now() < expires)
        {
            return entry
                .value
                .clone()
                .ok_or_else(|| entry.error.clone().unwrap_or_default());
        }
        match fetch().await {
            Ok(value) => {
                entry.value = Some(value.clone());
                entry.error = None;
                entry.expires = Some(Instant::now() + Duration::from_secs(900));
                Ok(value)
            }
            Err(error) => {
                // Preserve the last good catalog, but prevent an error stampede.
                entry.error = Some(error.clone());
                entry.expires = Some(Instant::now() + Duration::from_secs(30));
                entry.value.clone().ok_or(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[tokio::test]
    async fn parallel_consumers_share_one_catalog_request() {
        let cache = Arc::new(Cache::default());
        let requests = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..16 {
            let cache = cache.clone();
            let requests = requests.clone();
            tasks.push(tokio::spawn(async move {
                cache
                    .get_or_fetch((Exchange::Gate, MarketKind::Perp), || async {
                        requests.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(20)).await;
                        Ok(Vec::new())
                    })
                    .await
                    .unwrap();
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        println!(
            "catalog network calls for 16 consumers: {}",
            requests.load(Ordering::SeqCst)
        );
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }
}
