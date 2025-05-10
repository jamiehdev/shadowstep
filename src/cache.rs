use bytes::Bytes;
use http::{HeaderMap, StatusCode};
use moka::future::Cache;
use std::sync::Arc;
use std::time::Duration;

/// cached response containing status code, headers, and body
#[derive(Clone)]
pub struct CachedResponse {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Bytes,
}

/// cache for cdn responses with configurable size and ttl
pub struct CdnCache {
    cache: Cache<String, Arc<CachedResponse>>,
}

impl CdnCache {
    /// creates a new cdn cache with specified size limit (in mb) and ttl (in seconds)
    pub fn new(size_mb: u64, ttl_seconds: u64) -> Self {
        // convert mb to estimated item count (rough approximation)
        // assumption: average cached item is ~10kb including headers and metadata
        let max_capacity = (size_mb * 1024 * 1024) / (10 * 1024);
        
        // create moka cache with time-based expiration
        let cache = Cache::builder()
            .max_capacity(max_capacity)
            .time_to_live(Duration::from_secs(ttl_seconds))
            .build();
        
        Self { cache }
    }
    
    /// retrieves a cached response by key
    pub async fn get(&self, key: &str) -> Option<Arc<CachedResponse>> {
        self.cache.get(key).await
    }
    
    /// inserts a response into the cache
    pub async fn insert(
        &self, 
        key: String, 
        status: StatusCode,
        headers: HeaderMap,
        body: Bytes
    ) {
        let cached_response = Arc::new(CachedResponse {
            status,
            headers,
            body,
        });
        
        self.cache.insert(key, cached_response).await;
    }
    
    /// returns the number of items in the cache
    pub async fn len(&self) -> u64 {
        self.cache.entry_count()
    }
    
    /// invalidates a specific cache entry
    pub async fn invalidate(&self, key: &str) {
        self.cache.invalidate(key).await;
    }
    
    /// clears the entire cache
    pub async fn clear(&self) {
        self.cache.invalidate_all();
    }
} 