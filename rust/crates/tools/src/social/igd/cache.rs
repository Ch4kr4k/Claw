//! Persists the discovered GraphQL persisted-query `doc_id` so most `/igd`
//! calls skip `discovery::discover_graphql_operation` entirely (Instagram
//! ships a new web build, and therefore a new `doc_id`, only occasionally —
//! re-discovering it from scratch on every single call would mean
//! downloading and scanning dozens of JS bundles for every post). Lives at
//! a path under `Config::herald_data_dir` the caller picks (see
//! `Herald::handle_igd`) — no new `paths.json` entry needed, same "a file
//! under an already-configured data dir" shape `Herald`'s own
//! `state.json` already uses.

use std::collections::HashMap;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use super::types::{CachedOperation, GraphQlOperation};

async fn load_cache_map(cache_file: &Path) -> HashMap<String, CachedOperation> {
    tokio::fs::read_to_string(cache_file)
        .await
        .ok()
        .and_then(|contents| serde_json::from_str(&contents).ok())
        .unwrap_or_default()
}

async fn save_cache_map(cache_file: &Path, map: &HashMap<String, CachedOperation>) {
    if let Ok(json) = serde_json::to_string_pretty(map) {
        let _ = tokio::fs::write(cache_file, json).await;
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub async fn read_operation_cache(
    cache_file: &Path,
    operation_name: &str,
) -> Option<CachedOperation> {
    load_cache_map(cache_file).await.remove(operation_name)
}

pub async fn save_operation_cache(cache_file: &Path, operation: &GraphQlOperation) {
    let mut map = load_cache_map(cache_file).await;
    map.insert(
        operation.operation_name.clone(),
        CachedOperation {
            operation_name: operation.operation_name.clone(),
            doc_id: operation.doc_id.clone(),
            provided_variables: operation.provided_variables.clone(),
            saved_at: now_unix(),
        },
    );
    save_cache_map(cache_file, &map).await;
}

pub async fn delete_cached_operation(cache_file: &Path, operation_name: &str) {
    let mut map = load_cache_map(cache_file).await;
    if map.remove(operation_name).is_some() {
        save_cache_map(cache_file, &map).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::social::test_support::TempDir;

    #[tokio::test]
    async fn round_trips_a_cached_operation() {
        let dir = TempDir::new("igd-cache-roundtrip");
        let cache_file = dir.join("cache.json");
        let op = GraphQlOperation {
            operation_name: "PolarisPostRootQuery".into(),
            doc_id: "12345".into(),
            provided_variables: Some(vec!["__relay_internal__pv__Xrelayprovider".into()]),
            discovered_from_bundle: "https://static.cdninstagram.com/x.js".into(),
            discovered_at: 0,
        };
        save_operation_cache(&cache_file, &op).await;

        let cached = read_operation_cache(&cache_file, "PolarisPostRootQuery")
            .await
            .unwrap();
        assert_eq!(cached.doc_id, "12345");
        assert_eq!(
            cached.provided_variables,
            Some(vec!["__relay_internal__pv__Xrelayprovider".to_string()])
        );
    }

    #[tokio::test]
    async fn legacy_cache_entry_without_provided_variables_still_loads() {
        let dir = TempDir::new("igd-cache-legacy");
        let cache_file = dir.join("cache.json");
        tokio::fs::write(
            &cache_file,
            r#"{"PolarisPostRootQuery":{"operation_name":"PolarisPostRootQuery","doc_id":"1","saved_at":0}}"#,
        )
        .await
        .unwrap();
        let cached = read_operation_cache(&cache_file, "PolarisPostRootQuery")
            .await
            .unwrap();
        assert_eq!(cached.doc_id, "1");
        assert!(cached.provided_variables.is_none());
    }

    #[tokio::test]
    async fn missing_cache_file_reads_as_none() {
        let dir = TempDir::new("igd-cache-missing");
        let cache_file = dir.join("does-not-exist.json");
        assert!(read_operation_cache(&cache_file, "PolarisPostRootQuery")
            .await
            .is_none());
    }

    #[tokio::test]
    async fn delete_removes_only_the_named_operation() {
        let dir = TempDir::new("igd-cache-delete");
        let cache_file = dir.join("cache.json");
        let op = GraphQlOperation {
            operation_name: "PolarisPostRootQuery".into(),
            doc_id: "12345".into(),
            provided_variables: Some(vec!["__relay_internal__pv__Xrelayprovider".into()]),
            discovered_from_bundle: "https://static.cdninstagram.com/x.js".into(),
            discovered_at: 0,
        };
        save_operation_cache(&cache_file, &op).await;
        delete_cached_operation(&cache_file, "PolarisPostRootQuery").await;
        assert!(read_operation_cache(&cache_file, "PolarisPostRootQuery")
            .await
            .is_none());
    }
}
