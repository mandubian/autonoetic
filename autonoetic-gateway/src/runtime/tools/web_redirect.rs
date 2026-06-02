//! Safe redirect handling for web tools: follow 30x only within the same registrable
//! domain (or exact host); cross-domain redirects require a new approval.

use std::net::IpAddr;

/// Maximum redirect hops per web.fetch / web.call request.
pub const MAX_WEB_REDIRECT_HOPS: u32 = 5;

/// Multi-part public suffixes (eTLD) for registrable-domain extraction.
/// Not exhaustive; covers common cases without pulling in the full PSL.
const MULTI_PART_PUBLIC_SUFFIXES: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "gov.uk", "co.jp", "ne.jp", "or.jp", "ac.jp", "go.jp", "co.kr",
    "com.au", "net.au", "org.au", "edu.au", "gov.au", "com.br", "com.mx", "com.ar", "co.nz",
    "com.sg", "com.hk", "co.in", "co.za", "com.tr", "com.pl", "com.tw", "com.cn", "com.my",
];

/// Normalize a host for comparison (lowercase, no trailing dot, strip port if embedded).
pub fn normalize_host(host: &str) -> String {
    let mut h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    if let Some((name, _port)) = h.rsplit_once(':') {
        if !name.contains(':') && h.parse::<IpAddr>().is_err() {
            h = name.to_string();
        }
    }
    h
}

/// Best-effort registrable domain (eTLD+1) for redirect scope checks.
pub fn registrable_domain(host: &str) -> String {
    let host = normalize_host(host);
    if host.parse::<IpAddr>().is_ok() {
        return host;
    }
    let labels: Vec<&str> = host.split('.').filter(|label| !label.is_empty()).collect();
    if labels.is_empty() {
        return host;
    }
    if labels.len() == 1 {
        return host;
    }
    let suffix_two = format!("{}.{}", labels[labels.len() - 2], labels[labels.len() - 1]);
    if labels.len() >= 3 && MULTI_PART_PUBLIC_SUFFIXES.contains(&suffix_two.as_str()) {
        labels[labels.len() - 3..].join(".")
    } else if labels.len() >= 2 {
        labels[labels.len() - 2..].join(".")
    } else {
        host
    }
}

/// True when a redirect from `from_host` to `to_host` may be followed automatically.
pub fn hosts_same_redirect_scope(from_host: &str, to_host: &str) -> bool {
    let from = normalize_host(from_host);
    let to = normalize_host(to_host);
    if from == to {
        return true;
    }
    registrable_domain(&from) == registrable_domain(&to)
}

pub fn is_redirect_status(status: reqwest::StatusCode) -> bool {
    matches!(
        status.as_u16(),
        301 | 302 | 303 | 307 | 308
    )
}

/// Resolve a redirect `Location` relative to `current_url`.
pub fn resolve_redirect_location(current_url: &str, location: &str) -> anyhow::Result<String> {
    let location = location.trim();
    anyhow::ensure!(!location.is_empty(), "redirect Location header is empty");
    let base = reqwest::Url::parse(current_url).map_err(|e| {
        anyhow::anyhow!("invalid current URL '{}': {}", current_url, e)
    })?;
    let resolved = base.join(location).map_err(|e| {
        anyhow::anyhow!(
            "invalid redirect Location '{}' from '{}': {}",
            location,
            current_url,
            e
        )
    })?;
    let scheme = resolved.scheme();
    anyhow::ensure!(
        scheme == "http" || scheme == "https",
        "redirect to non-http(s) scheme '{}' is not allowed",
        scheme
    );
    anyhow::ensure!(
        resolved.host_str().is_some(),
        "redirect URL '{}' has no host",
        resolved
    );
    Ok(resolved.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registrable_domain_subdomain_and_apex_match() {
        assert_eq!(
            registrable_domain("docs.alpaca.markets"),
            "alpaca.markets"
        );
        assert_eq!(registrable_domain("alpaca.markets"), "alpaca.markets");
        assert!(hosts_same_redirect_scope(
            "docs.alpaca.markets",
            "alpaca.markets"
        ));
    }

    #[test]
    fn registrable_domain_differs_across_sites() {
        assert!(!hosts_same_redirect_scope(
            "alpaca.markets",
            "example.com"
        ));
    }

    #[test]
    fn exact_host_match_ignores_case() {
        assert!(hosts_same_redirect_scope("Docs.Alpaca.Markets", "docs.alpaca.markets"));
    }

    #[test]
    fn resolve_redirect_location_relative_path() {
        let resolved = resolve_redirect_location(
            "https://docs.alpaca.markets/old/path",
            "/new/doc",
        )
        .expect("resolve");
        assert_eq!(resolved, "https://docs.alpaca.markets/new/doc");
    }

    #[test]
    fn resolve_redirect_location_rejects_non_http_scheme() {
        let err = resolve_redirect_location("https://example.com/a", "javascript:alert(1)")
            .expect_err("scheme");
        assert!(err.to_string().contains("non-http"));
    }
}
