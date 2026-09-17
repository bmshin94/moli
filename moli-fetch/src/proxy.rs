use std::net::IpAddr;

use cidr::AnyIpCidr;
use url::Url;

use crate::FetchConfig;

/// Concrete proxy route selected once for one HTTP request target.
///
/// The same value must drive both DNS admission and curl configuration so
/// environment variables and `no_proxy` cannot be interpreted twice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HttpProxyRoute {
    Direct,
    Proxy(String),
}

impl HttpProxyRoute {
    pub(crate) fn is_proxy(&self) -> bool {
        matches!(self, Self::Proxy(_))
    }
}

pub(crate) fn resolve_http_proxy_route(config: &FetchConfig, url: &Url) -> HttpProxyRoute {
    resolve_http_proxy_route_with_env(config, url, |name| std::env::var(name).ok())
}

fn resolve_http_proxy_route_with_env(
    config: &FetchConfig,
    url: &Url,
    mut env: impl FnMut(&str) -> Option<String>,
) -> HttpProxyRoute {
    let proxy = match config.http_proxy() {
        Some("") => return HttpProxyRoute::Direct,
        Some(proxy) => Some(proxy.to_owned()),
        None => env_proxy_for_scheme(url.scheme(), &mut env),
    };
    let Some(proxy) = proxy.filter(|proxy| !proxy.is_empty()) else {
        return HttpProxyRoute::Direct;
    };

    let no_proxy = match config.http_no_proxy() {
        Some(no_proxy) => Some(no_proxy.to_owned()),
        None => env_no_proxy(&mut env),
    };
    if let (Some(host), Some(port)) = (url.host_str(), url.port_or_known_default())
        && no_proxy_matches(host, port, no_proxy.as_deref())
    {
        return HttpProxyRoute::Direct;
    }

    HttpProxyRoute::Proxy(proxy)
}

fn env_proxy_for_scheme(
    scheme: &str,
    env: &mut impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    let names: &[&str] = match scheme {
        // curl deliberately ignores uppercase HTTP_PROXY because CGI servers
        // commonly expose an attacker-controlled Proxy header under that name.
        "http" => &["http_proxy"],
        "https" => &["https_proxy", "HTTPS_PROXY"],
        _ => &[],
    };
    for name in names {
        if let Some(value) = env(name).filter(|value| !value.is_empty()) {
            return Some(value);
        }
    }
    for name in ["all_proxy", "ALL_PROXY"] {
        if let Some(value) = env(name).filter(|value| !value.is_empty()) {
            return Some(value);
        }
    }
    None
}

fn env_no_proxy(env: &mut impl FnMut(&str) -> Option<String>) -> Option<String> {
    env("no_proxy")
        .filter(|value| !value.is_empty())
        .or_else(|| env("NO_PROXY").filter(|value| !value.is_empty()))
}

fn no_proxy_matches(host: &str, port: u16, no_proxy: Option<&str>) -> bool {
    let Some(no_proxy) = no_proxy else {
        return false;
    };
    let host = host
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    let host_ip = host.parse::<IpAddr>().ok();
    no_proxy.split(',').any(|token| {
        let token = token.trim();
        if token.is_empty() {
            return false;
        }
        if token == "*" {
            return true;
        }
        let (token_host, token_port) = split_no_proxy_host_port(token);
        if let Some(token_port) = token_port
            && token_port != port
        {
            return false;
        }
        let token_host = token_host
            .trim_matches(['[', ']'])
            .trim_start_matches('.')
            .trim_end_matches('.')
            .to_ascii_lowercase();
        if token_host.is_empty() {
            return false;
        }
        if let Some(host_ip) = host_ip {
            return token_host.parse::<IpAddr>().is_ok_and(|ip| ip == host_ip)
                || token_host
                    .parse::<AnyIpCidr>()
                    .is_ok_and(|cidr| cidr.contains(&host_ip));
        }
        host == token_host
            || host
                .strip_suffix(&token_host)
                .is_some_and(|prefix| prefix.ends_with('.'))
    })
}

fn split_no_proxy_host_port(token: &str) -> (&str, Option<u16>) {
    if let Some(bracketed) = token.strip_prefix('[')
        && let Some((host, port)) = bracketed.rsplit_once("]:")
        && let Ok(port) = port.parse::<u16>()
    {
        return (host, Some(port));
    }
    let Some((host, port)) = token.rsplit_once(':') else {
        return (token, None);
    };
    match port.parse::<u16>() {
        Ok(port) if !host.contains(':') => (host, Some(port)),
        _ => (token, None),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn route(config: &FetchConfig, raw_url: &str, env: &[(&str, &str)]) -> HttpProxyRoute {
        let env = env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<HashMap<_, _>>();
        resolve_http_proxy_route_with_env(
            config,
            &Url::parse(raw_url).expect("test URL should parse"),
            |name| env.get(name).cloned(),
        )
    }

    #[test]
    fn explicit_proxy_and_no_proxy_select_one_route() {
        let mut config = FetchConfig::default();
        config.set_http_proxy(Some("http://proxy.test:8080".to_owned()));

        assert_eq!(
            route(&config, "https://api.example.test/path", &[]),
            HttpProxyRoute::Proxy("http://proxy.test:8080".to_owned())
        );
        config.set_http_no_proxy(Some(".example.test".to_owned()));
        assert_eq!(
            route(&config, "https://api.example.test/path", &[]),
            HttpProxyRoute::Direct
        );
    }

    #[test]
    fn empty_explicit_proxy_disables_environment_proxy_fallback() {
        let mut config = FetchConfig::default();
        config.set_http_proxy(Some(String::new()));

        assert_eq!(
            route(
                &config,
                "http://example.test/path",
                &[("http_proxy", "http://proxy.test:8080")],
            ),
            HttpProxyRoute::Direct
        );
    }

    #[test]
    fn environment_proxy_and_no_proxy_follow_expected_precedence() {
        let config = FetchConfig::default();

        assert_eq!(
            route(
                &config,
                "http://example.test/path",
                &[("http_proxy", "http://proxy.test:8080")],
            ),
            HttpProxyRoute::Proxy("http://proxy.test:8080".to_owned())
        );
        assert_eq!(
            route(
                &config,
                "https://example.test/path",
                &[("HTTPS_PROXY", "http://proxy.test:8080")],
            ),
            HttpProxyRoute::Proxy("http://proxy.test:8080".to_owned())
        );
        assert_eq!(
            route(
                &config,
                "https://api.example.test/path",
                &[
                    ("all_proxy", "http://proxy.test:8080"),
                    ("NO_PROXY", "example.test"),
                ],
            ),
            HttpProxyRoute::Direct
        );
    }

    #[test]
    fn no_proxy_port_and_domain_boundaries_are_exact() {
        assert!(no_proxy_matches(
            "api.example.test",
            8443,
            Some("example.test:8443")
        ));
        assert!(!no_proxy_matches(
            "api.example.test",
            443,
            Some("example.test:8443")
        ));
        assert!(!no_proxy_matches(
            "notexample.test",
            443,
            Some("example.test")
        ));
        assert!(no_proxy_matches("anything.test", 443, Some("*")));
        assert!(no_proxy_matches(
            "api.example.test.",
            443,
            Some("example.test.")
        ));
        assert!(no_proxy_matches("192.0.2.42", 80, Some("192.0.2.0/24")));
        assert!(!no_proxy_matches("198.51.100.42", 80, Some("192.0.2.0/24")));
        assert!(no_proxy_matches("[::1]", 8443, Some("[::1]:8443")));
    }
}
