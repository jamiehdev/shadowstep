use actix_web::http::header::{self, HeaderName};
use actix_web::HttpRequest;
use std::net::IpAddr;

/// request headers through which a client can claim a different address,
/// host, scheme or path prefix. some origin frameworks build links or
/// redirects from them, such as Spring from `X-Forwarded-Prefix`, Rack from
/// `X-Forwarded-Scheme` and some PHP frameworks from `X-Host`, and the cache
/// key does not hold them. shadowstep faces clients directly, so it trusts
/// none of them and removes them all before forwarding.
pub(crate) const CLIENT_FORWARDING_HEADERS: [HeaderName; 13] = [
    header::FORWARDED,
    HeaderName::from_static("x-forwarded-for"),
    HeaderName::from_static("x-forwarded-host"),
    HeaderName::from_static("x-forwarded-proto"),
    HeaderName::from_static("x-forwarded-port"),
    HeaderName::from_static("x-forwarded-server"),
    HeaderName::from_static("x-forwarded-prefix"),
    HeaderName::from_static("x-forwarded-uri"),
    HeaderName::from_static("x-host"),
    HeaderName::from_static("x-forwarded-scheme"),
    HeaderName::from_static("x-original-host"),
    HeaderName::from_static("front-end-https"),
    HeaderName::from_static("x-real-ip"),
];

/// request headers that override the request path in some origin
/// frameworks: IIS URL Rewrite sets them, and Symfony trusted them until
/// CVE-2018-14773. the cache key does not hold them, so a client could have
/// one path's response stored under another. shadowstep removes them before
/// forwarding.
pub(crate) const URL_OVERRIDE_HEADERS: [HeaderName; 2] = [
    HeaderName::from_static("x-original-url"),
    HeaderName::from_static("x-rewrite-url"),
];

/// the client's address, scheme and host, taken only from the connection and
/// the request target. unlike actix's `ConnectionInfo`, it never reads
/// `Forwarded` or `X-Forwarded-*`, which the client controls.
///
/// the origin response cache must key on `scheme` and `host` from here, so
/// that the key matches the `X-Forwarded-Proto` and `X-Forwarded-Host` the
/// origin receives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ClientInfo {
    /// the TCP peer address. `None` only when actix has no peer, as for a
    /// request built with `TestRequest`.
    pub ip: Option<IpAddr>,
    /// `https` on the rustls listener and `http` on the plain one.
    pub scheme: &'static str,
    /// the authority of an absolute-form or HTTP/2 target, else the `Host`
    /// header, else the listener's own host from actix's `AppConfig`.
    pub host: String,
}

impl ClientInfo {
    pub(crate) fn from_request(req: &HttpRequest) -> Self {
        let ip = req.peer_addr().map(|addr| addr.ip());
        let scheme = if req.app_config().secure() {
            "https"
        } else {
            "http"
        };

        // a proxy must ignore `Host` when the target is absolute-form (RFC
        // 9112 section 3.2.2), and HTTP/2 carries the host in `:authority`
        let host = req
            .uri()
            .authority()
            .map(|authority| authority.as_str().to_owned())
            .or_else(|| {
                req.headers()
                    .get(header::HOST)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| req.app_config().host().to_owned());

        ClientInfo { ip, scheme, host }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::test::TestRequest;

    const SPOOFED: [(&str, &str); 4] = [
        ("forwarded", "for=1.2.3.4;host=evil.example;proto=https"),
        ("x-forwarded-for", "1.2.3.4"),
        ("x-forwarded-host", "evil.example"),
        ("x-forwarded-proto", "https"),
    ];

    fn with_spoofed_headers(mut req: TestRequest) -> TestRequest {
        for header in SPOOFED {
            req = req.insert_header(header);
        }
        req
    }

    #[test]
    fn ignores_forwarding_headers() {
        let req = with_spoofed_headers(TestRequest::get().uri("/page"))
            .insert_header(("host", "real.example"))
            .peer_addr("192.0.2.7:50000".parse().unwrap())
            .to_http_request();

        assert_eq!(
            ClientInfo::from_request(&req),
            ClientInfo {
                ip: Some("192.0.2.7".parse().unwrap()),
                scheme: "http",
                host: "real.example".to_owned(),
            }
        );
    }

    #[test]
    fn absolute_form_authority_wins_over_host_header() {
        let req = TestRequest::get()
            .uri("http://target.example/page")
            .insert_header(("host", "other.example"))
            .to_http_request();

        assert_eq!(ClientInfo::from_request(&req).host, "target.example");
    }

    #[test]
    fn missing_host_falls_back_to_the_listener_host() {
        let req = with_spoofed_headers(TestRequest::get().uri("/page")).to_http_request();

        assert_eq!(ClientInfo::from_request(&req).host, "localhost:8080");
    }
}
