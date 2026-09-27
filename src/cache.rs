//! a shared HTTP cache (RFC 9111) for origin responses, and the byte-bounded
//! store that also holds local assets.

use actix_web::http::header::{HeaderMap as RequestHeaders, AUTHORIZATION, COOKIE};
use bytes::Bytes;
use hyper::header::{self, HeaderMap, HeaderName, HeaderValue};
use hyper::{Method, StatusCode};
use moka::sync::Cache;
use moka::Expiry;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

const MIB: u64 = 1024 * 1024;

/// the largest body the store keeps. larger responses stream through.
const MAX_ENTRY_BYTES: u64 = 8 * MIB;

/// the most target URIs whose `Vary` field names the store remembers.
const MAX_INDEXED_URIS: u64 = 65_536;

/// the cache key without the `Vary` part. only responses to GET are stored,
/// so the method is always GET. the scheme is part of the key because an
/// origin can answer http and https differently, such as with a redirect.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct PrimaryKey {
    scheme: String,
    host: String,
    path_and_query: String,
}

impl PrimaryKey {
    pub fn new(scheme: &str, host: &str, path_and_query: &str) -> Self {
        PrimaryKey {
            scheme: scheme.to_ascii_lowercase(),
            host: host.to_ascii_lowercase(),
            path_and_query: path_and_query.to_owned(),
        }
    }

    fn weight(&self) -> usize {
        self.scheme.len() + self.host.len() + self.path_and_query.len()
    }
}

/// the `Vary` field names of the latest stored response for a URI, and the
/// generation that the stored variants' keys carry. a new generation makes
/// every older variant unreachable.
struct Index {
    vary: Vec<HeaderName>,
    generation: u64,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
struct ResponseKey {
    primary: PrimaryKey,
    generation: u64,
    /// the request's values for the index's `Vary` field names, in order
    vary: Vec<Option<String>>,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum Key {
    Asset(PathBuf),
    Response(ResponseKey),
}

#[derive(Clone)]
enum Entry {
    Asset(Arc<Asset>),
    Response(Arc<StoredResponse>),
}

/// a local file as read from disk.
pub struct Asset {
    pub content: Bytes,
    pub etag: String,
    pub modified: SystemTime,
    pub len: u64,
    ttl: Duration,
}

/// response header fields in the order the origin sent them.
pub type Fields = Vec<(HeaderName, HeaderValue)>;

/// an origin response held by the store.
pub struct StoredResponse {
    pub status: StatusCode,
    /// end-to-end fields without `Content-Length` and `Age`
    pub headers: Fields,
    pub body: Bytes,
    stored_at: Instant,
    initial_age: Duration,
    ttl: Duration,
}

impl StoredResponse {
    /// the response's current age (RFC 9111 section 4.2.3).
    pub fn age(&self) -> Duration {
        self.initial_age + self.stored_at.elapsed()
    }

    fn is_fresh(&self) -> bool {
        self.stored_at.elapsed() < self.ttl
    }
}

fn entry_weight(key: &Key, entry: &Entry) -> u32 {
    let key = match key {
        Key::Asset(path) => path.as_os_str().len(),
        Key::Response(key) => {
            key.primary.weight()
                + key
                    .vary
                    .iter()
                    .map(|v| v.as_ref().map_or(0, String::len))
                    .sum::<usize>()
        }
    };
    let value = match entry {
        Entry::Asset(asset) => asset.content.len() + asset.etag.len(),
        Entry::Response(response) => {
            response.body.len()
                + response
                    .headers
                    .iter()
                    .map(|(name, value)| name.as_str().len() + value.len())
                    .sum::<usize>()
        }
    };
    u32::try_from(key + value).unwrap_or(u32::MAX)
}

struct EntryExpiry;

impl Expiry<Key, Entry> for EntryExpiry {
    fn expire_after_create(&self, _: &Key, entry: &Entry, _: Instant) -> Option<Duration> {
        Some(entry_ttl(entry))
    }

    fn expire_after_update(
        &self,
        _: &Key,
        entry: &Entry,
        _: Instant,
        _: Option<Duration>,
    ) -> Option<Duration> {
        Some(entry_ttl(entry))
    }
}

fn entry_ttl(entry: &Entry) -> Duration {
    match entry {
        Entry::Asset(asset) => asset.ttl,
        Entry::Response(response) => response.ttl,
    }
}

/// origin responses and local assets, bounded by bytes and by time.
#[derive(Clone)]
pub struct Store {
    entries: Cache<Key, Entry>,
    index: Cache<PrimaryKey, Arc<Index>>,
    generation: Arc<AtomicU64>,
    ttl: Duration,
    max_entry: u64,
}

impl Store {
    /// a store of at most `size_mb` MiB whose entries live for at most `ttl`.
    pub fn new(size_mb: u64, ttl: Duration) -> Self {
        let capacity = size_mb.saturating_mul(MIB);
        Store {
            entries: Cache::builder()
                .max_capacity(capacity)
                .weigher(entry_weight)
                .expire_after(EntryExpiry)
                .build(),
            index: Cache::builder()
                .max_capacity(MAX_INDEXED_URIS)
                .time_to_live(ttl.max(Duration::from_secs(1)))
                .build(),
            generation: Arc::new(AtomicU64::new(0)),
            ttl,
            max_entry: capacity.min(MAX_ENTRY_BYTES),
        }
    }

    /// the largest body, in bytes, that the store keeps.
    pub fn max_entry(&self) -> u64 {
        self.max_entry
    }

    fn enabled(&self) -> bool {
        self.max_entry > 0 && !self.ttl.is_zero()
    }

    pub fn entry_count(&self) -> u64 {
        self.entries.run_pending_tasks();
        self.entries.entry_count()
    }

    pub fn weighted_size(&self) -> u64 {
        self.entries.run_pending_tasks();
        self.entries.weighted_size()
    }

    pub fn asset(&self, path: &Path) -> Option<Arc<Asset>> {
        match self.entries.get(&Key::Asset(path.to_path_buf()))? {
            Entry::Asset(asset) => Some(asset),
            Entry::Response(_) => None,
        }
    }

    pub fn insert_asset(
        &self,
        path: PathBuf,
        content: Bytes,
        etag: String,
        modified: SystemTime,
    ) -> Arc<Asset> {
        let asset = Arc::new(Asset {
            len: content.len() as u64,
            content,
            etag,
            modified,
            ttl: self.ttl,
        });
        if self.enabled() && asset.len <= self.max_entry {
            self.entries
                .insert(Key::Asset(path), Entry::Asset(asset.clone()));
            self.entries.run_pending_tasks();
        }
        asset
    }

    /// a fresh stored response for `primary` that matches the request's
    /// `Vary` fields (RFC 9111 section 4.1) and is no older than `max_age`.
    pub fn lookup(
        &self,
        primary: &PrimaryKey,
        request: &RequestHeaders,
        max_age: Option<Duration>,
    ) -> Option<Arc<StoredResponse>> {
        let index = self.index.get(primary)?;
        let key = Key::Response(ResponseKey {
            primary: primary.clone(),
            generation: index.generation,
            vary: vary_values(&index.vary, request),
        });
        let Entry::Response(response) = self.entries.get(&key)? else {
            return None;
        };
        if !response.is_fresh() {
            return None;
        }
        if max_age.is_some_and(|max_age| response.age() > max_age) {
            return None;
        }
        Some(response)
    }

    /// stores `body` under `primary` and the request's values for the
    /// response's `Vary` field names.
    pub fn insert_response(
        &self,
        primary: PrimaryKey,
        vary: Vary,
        policy: &Storable,
        status: StatusCode,
        headers: Fields,
        body: Bytes,
    ) {
        if !self.enabled() || body.len() as u64 > self.max_entry {
            return;
        }
        // a response whose Vary names differ from the stored one's replaces
        // every stored variant
        let generation = match self.index.get(&primary) {
            Some(index) if index.vary == vary.names => index.generation,
            _ => self.generation.fetch_add(1, Ordering::Relaxed),
        };
        self.index.insert(
            primary.clone(),
            Arc::new(Index {
                vary: vary.names,
                generation,
            }),
        );
        let key = Key::Response(ResponseKey {
            primary,
            generation,
            vary: vary.values,
        });
        let response = StoredResponse {
            status,
            headers,
            body,
            stored_at: Instant::now(),
            initial_age: policy.initial_age,
            ttl: policy.ttl.min(self.ttl),
        };
        self.entries
            .insert(key, Entry::Response(Arc::new(response)));
        self.entries.run_pending_tasks();
    }

    /// makes every stored response for `primary` unreachable (RFC 9111
    /// section 4.4).
    pub fn invalidate(&self, primary: &PrimaryKey) {
        self.index.invalidate(primary);
    }
}

/// the `Vary` field names of a response and the request's values for them.
pub struct Vary {
    names: Vec<HeaderName>,
    values: Vec<Option<String>>,
}

fn vary_values(names: &[HeaderName], request: &RequestHeaders) -> Vec<Option<String>> {
    names
        .iter()
        .map(|name| {
            let values: Vec<_> = request
                .get_all(name)
                .map(|v| String::from_utf8_lossy(v.as_bytes()).trim().to_owned())
                .collect();
            (!values.is_empty()).then(|| values.join(", "))
        })
        .collect()
}

/// `Cache-Control` directives (RFC 9111 section 5.2), with lowercased names.
#[derive(Default)]
pub struct Directives {
    list: Vec<(String, Option<String>)>,
    /// a field value that is not visible ASCII
    malformed: bool,
}

impl Directives {
    pub fn parse<'a>(values: impl IntoIterator<Item = &'a HeaderValue>) -> Self {
        let mut directives = Directives::default();
        for value in values {
            let Ok(value) = value.to_str() else {
                directives.malformed = true;
                continue;
            };
            for part in split_outside_quotes(value) {
                let (name, argument) = match part.split_once('=') {
                    Some((name, argument)) => {
                        let argument = argument.trim();
                        let argument = argument
                            .strip_prefix('"')
                            .and_then(|a| a.strip_suffix('"'))
                            .unwrap_or(argument);
                        (name, Some(argument.to_owned()))
                    }
                    None => (part, None),
                };
                let name = name.trim().to_ascii_lowercase();
                if !name.is_empty() {
                    directives.list.push((name, argument));
                }
            }
        }
        directives
    }

    pub fn has(&self, name: &str) -> bool {
        self.list.iter().any(|(n, _)| n == name)
    }

    /// the first occurrence's delta-seconds (RFC 9111 section 4.2.1). an
    /// invalid value reads as 0, so the response is stale. a value too large
    /// to hold saturates (section 1.2.2).
    pub fn seconds(&self, name: &str) -> Option<Duration> {
        let (_, argument) = self.list.iter().find(|(n, _)| n == name)?;
        let argument = argument.as_deref().unwrap_or("");
        if argument.is_empty() || !argument.bytes().all(|b| b.is_ascii_digit()) {
            return Some(Duration::ZERO);
        }
        Some(Duration::from_secs(argument.parse().unwrap_or(u64::MAX)))
    }
}

/// splits a field value on commas that are not inside a quoted string.
fn split_outside_quotes(value: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut start = 0;
    let mut quoted = false;
    let mut escaped = false;
    for (i, c) in value.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            ',' if !quoted => {
                parts.push(&value[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&value[start..]);
    parts
}

/// what the proxy may do with the cache for one request.
pub struct RequestPolicy {
    /// whether a stored response may answer the request
    pub may_serve: bool,
    /// the oldest stored response the client accepts
    pub max_age: Option<Duration>,
    /// whether the origin's response may be stored
    may_store: bool,
    has_credentials: bool,
}

impl RequestPolicy {
    pub fn new(method: &Method, headers: &RequestHeaders) -> Self {
        let directives = Directives::parse(headers.get_all(header::CACHE_CONTROL));
        // a shared cache that stores GET responses can answer HEAD from them
        // (RFC 9110 section 9.3.2). request no-cache means the client wants
        // the origin's answer (RFC 9111 section 5.2.1.4), and no-store means
        // nothing about this exchange is kept (section 5.2.1.5).
        let cacheable_method = *method == Method::GET || *method == Method::HEAD;
        let no_store = directives.malformed || directives.has("no-store");
        RequestPolicy {
            may_serve: cacheable_method && !no_store && !directives.has("no-cache"),
            max_age: directives.seconds("max-age"),
            may_store: *method == Method::GET && !no_store,
            has_credentials: headers.contains_key(AUTHORIZATION) || headers.contains_key(COOKIE),
        }
    }
}

/// a response that the store may keep, and for how long.
pub struct Storable {
    ttl: Duration,
    initial_age: Duration,
}

/// status codes that are cacheable by default (RFC 9110 section 15.1).
/// 206 is left out because the store does not combine partial content.
const CACHEABLE_BY_DEFAULT: [u16; 11] = [200, 203, 204, 300, 301, 308, 404, 405, 410, 414, 501];

/// Cache-Control directives that stop a shared cache storing a response. a
/// shared cache must not store private responses, qualified or not (RFC 9111
/// section 5.2.2.7). no-cache would need revalidation on every use (section
/// 5.2.2.4), and this cache does not revalidate.
const UNSTORABLE_DIRECTIVES: [&str; 3] = ["no-store", "private", "no-cache"];

/// whether a shared cache may store the origin's response to a request
/// (RFC 9111 section 3), and the request's `Vary` values for its key.
pub fn storable(
    request_policy: &RequestPolicy,
    request: &RequestHeaders,
    status: StatusCode,
    response: &HeaderMap,
) -> Option<(Storable, Vary)> {
    if !request_policy.may_store {
        return None;
    }
    let directives = storable_response_directives(status, response)?;

    // RFC 9111 section 3.5 also allows s-maxage and must-revalidate to lift
    // this rule. only an explicit public does here, and the same rule covers
    // Cookie, which also identifies a user.
    if request_policy.has_credentials && !directives.has("public") {
        return None;
    }

    let names = vary_names(response)?;
    // the origin's Age, without the apparent age from its Date, so that a
    // skewed origin clock does not shorten or lengthen freshness (RFC 9111
    // section 4.2.3)
    let initial_age = response
        .get(header::AGE)
        .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
        .map_or(Duration::ZERO, Duration::from_secs);
    let ttl = freshness_lifetime(&directives, response)?.checked_sub(initial_age)?;
    if ttl.is_zero() {
        return None;
    }

    let values = vary_values(&names, request);
    Some((Storable { ttl, initial_age }, Vary { names, values }))
}

/// the response's Cache-Control directives, or `None` when the response
/// itself rules out storing whatever the request was.
fn storable_response_directives(status: StatusCode, response: &HeaderMap) -> Option<Directives> {
    // a stored Set-Cookie would hand one client's cookie to every client, so
    // it is never stored, even under public
    if !CACHEABLE_BY_DEFAULT.contains(&status.as_u16()) || response.contains_key(header::SET_COOKIE)
    {
        return None;
    }
    let directives = Directives::parse(response.get_all(header::CACHE_CONTROL));
    let unstorable =
        directives.malformed || UNSTORABLE_DIRECTIVES.iter().any(|d| directives.has(d));
    (!unstorable).then_some(directives)
}

/// the sorted field names that `Vary` lists, or `None` for `Vary: *`, which
/// never matches a later request (RFC 9111 section 4.1), or an invalid name.
fn vary_names(response: &HeaderMap) -> Option<Vec<HeaderName>> {
    let mut names = Vec::new();
    for value in response.get_all(header::VARY) {
        for name in value.to_str().ok()?.split(',').map(str::trim) {
            match name {
                "" => {}
                "*" => return None,
                _ => names.push(HeaderName::from_bytes(name.as_bytes()).ok()?),
            }
        }
    }
    names.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    names.dedup();
    Some(names)
}

/// the freshness lifetime from s-maxage, max-age or Expires, in that order
/// (RFC 9111 section 4.2.1). `None` when the origin gave none, because this
/// cache does not use heuristic freshness.
fn freshness_lifetime(directives: &Directives, response: &HeaderMap) -> Option<Duration> {
    if let Some(lifetime) = directives
        .seconds("s-maxage")
        .or_else(|| directives.seconds("max-age"))
    {
        return Some(lifetime);
    }

    let expires = response.get(header::EXPIRES)?;
    // an invalid Expires, such as "0", means already expired (RFC 9111
    // section 5.3)
    let Some(expires) = http_date(expires) else {
        return Some(Duration::ZERO);
    };
    let date = response
        .get(header::DATE)
        .and_then(http_date)
        .unwrap_or_else(SystemTime::now);
    Some(expires.duration_since(date).unwrap_or(Duration::ZERO))
}

fn http_date(value: &HeaderValue) -> Option<SystemTime> {
    httpdate::parse_http_date(value.to_str().ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn directives(value: &'static str) -> Directives {
        Directives::parse([&HeaderValue::from_static(value)])
    }

    #[test]
    fn directive_names_ignore_case_and_space() {
        let d = directives(" Public , MAX-AGE=60");
        assert!(d.has("public"));
        assert_eq!(d.seconds("max-age"), Some(Duration::from_secs(60)));
    }

    #[test]
    fn quoted_commas_do_not_split_directives() {
        let d = directives("no-cache=\"set-cookie, x-foo\", max-age=5");
        assert!(d.has("no-cache"));
        assert!(!d.has("x-foo\""));
        assert_eq!(d.seconds("max-age"), Some(Duration::from_secs(5)));
    }

    #[test]
    fn invalid_delta_seconds_is_zero() {
        assert_eq!(
            directives("max-age=abc").seconds("max-age"),
            Some(Duration::ZERO)
        );
        assert_eq!(
            directives("max-age=-1").seconds("max-age"),
            Some(Duration::ZERO)
        );
        assert_eq!(
            directives("max-age").seconds("max-age"),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn first_duplicate_directive_wins() {
        let d = directives("max-age=5, max-age=500");
        assert_eq!(d.seconds("max-age"), Some(Duration::from_secs(5)));
    }

    #[test]
    fn huge_delta_seconds_saturates() {
        let d = directives("max-age=99999999999999999999999");
        assert_eq!(d.seconds("max-age"), Some(Duration::from_secs(u64::MAX)));
    }

    #[test]
    fn quoted_argument_is_unquoted() {
        let d = directives("max-age=\"7\"");
        assert_eq!(d.seconds("max-age"), Some(Duration::from_secs(7)));
    }

    #[test]
    fn non_ascii_field_value_is_malformed() {
        let value = HeaderValue::from_bytes(b"max-age=60, \xff").unwrap();
        assert!(Directives::parse([&value]).malformed);
    }
}
