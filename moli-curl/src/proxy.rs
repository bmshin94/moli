use std::net::IpAddr;

use anyhow::{Context, Result, bail};
use cidr::AnyIpCidr;
use url::{Host, Url};

/// How an accepted proxy resolves the request target hostname.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyTargetResolution {
    /// The target hostname is sent to the proxy without a local DNS lookup.
    Proxy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyScheme {
    Http,
    Https,
    Socks5h,
    Socks4a,
}

impl ProxyScheme {
    pub fn uses_http_headers(self) -> bool {
        matches!(self, Self::Http | Self::Https)
    }

    fn default_port(self) -> u16 {
        match self {
            Self::Http => 80,
            Self::Https => 443,
            Self::Socks5h | Self::Socks4a => 1080,
        }
    }
}

/// Parsed proxy selected for a single request target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedProxy {
    url: Url,
    scheme: ProxyScheme,
    endpoint_port: u16,
}

impl SelectedProxy {
    pub fn parse(raw: &str) -> Result<Self> {
        let normalized = if raw.contains("://") {
            raw.to_owned()
        } else {
            format!("http://{raw}")
        };
        let url = Url::parse(&normalized)
            .with_context(|| format!("failed to parse proxy URL `{raw}`"))?;
        let scheme = match url.scheme() {
            "http" => ProxyScheme::Http,
            "https" => ProxyScheme::Https,
            "socks5h" => ProxyScheme::Socks5h,
            "socks4a" => ProxyScheme::Socks4a,
            "socks5" | "socks4" => bail!(
                "unsupported proxy scheme `{}`; use a remote-DNS proxy scheme such as `socks5h://`",
                url.scheme()
            ),
            scheme => bail!(
                "unsupported proxy scheme `{scheme}`; expected http, https, socks5h, or socks4a"
            ),
        };
        if url.host().is_none() {
            bail!("proxy URL `{raw}` is missing a host");
        }
        let endpoint_port = url.port().unwrap_or_else(|| scheme.default_port());
        Ok(Self {
            url,
            scheme,
            endpoint_port,
        })
    }

    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    pub fn scheme(&self) -> ProxyScheme {
        self.scheme
    }

    pub fn endpoint_host(&self) -> &str {
        self.url
            .host_str()
            .expect("selected proxy was validated with a host")
    }

    pub fn endpoint_ip(&self) -> Option<IpAddr> {
        match self
            .url
            .host()
            .expect("selected proxy was validated with a host")
        {
            Host::Ipv4(address) => Some(address.into()),
            Host::Ipv6(address) => Some(address.into()),
            Host::Domain(_) => None,
        }
    }

    pub fn endpoint_port(&self) -> u16 {
        self.endpoint_port
    }

    pub fn target_resolution(&self) -> ProxyTargetResolution {
        ProxyTargetResolution::Proxy
    }
}

/// Concrete route selected exactly once for a request target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyRoute {
    Direct,
    Proxy(SelectedProxy),
}

impl ProxyRoute {
    pub fn from_proxy_url(raw: &str) -> Result<Self> {
        Ok(Self::Proxy(SelectedProxy::parse(raw)?))
    }

    pub fn is_proxy(&self) -> bool {
        matches!(self, Self::Proxy(_))
    }

    pub fn proxy(&self) -> Option<&SelectedProxy> {
        match self {
            Self::Direct => None,
            Self::Proxy(proxy) => Some(proxy),
        }
    }
}

pub fn select_proxy_route(
    target: &Url,
    configured_proxy: Option<&str>,
    configured_no_proxy: Option<&str>,
) -> Result<ProxyRoute> {
    select_proxy_route_with_env(target, configured_proxy, configured_no_proxy, |name| {
        std::env::var(name).ok()
    })
}

pub fn select_proxy_route_with_env(
    target: &Url,
    configured_proxy: Option<&str>,
    configured_no_proxy: Option<&str>,
    mut env: impl FnMut(&str) -> Option<String>,
) -> Result<ProxyRoute> {
    let proxy = match configured_proxy {
        Some("") => return Ok(ProxyRoute::Direct),
        Some(proxy) => Some(proxy.to_owned()),
        None => env_proxy_for_scheme(target.scheme(), &mut env),
    };
    let Some(proxy) = proxy.filter(|proxy| !proxy.is_empty()) else {
        return Ok(ProxyRoute::Direct);
    };

    let no_proxy = match configured_no_proxy {
        Some(no_proxy) => Some(no_proxy.to_owned()),
        None => env_no_proxy(&mut env),
    };
    if let (Some(host), Some(port)) = (target.host_str(), target.port_or_known_default())
        && no_proxy_matches(host, port, no_proxy.as_deref())
    {
        return Ok(ProxyRoute::Direct);
    }

    ProxyRoute::from_proxy_url(&proxy)
}

fn env_proxy_for_scheme(
    scheme: &str,
    env: &mut impl FnMut(&str) -> Option<String>,
) -> Option<String> {
    let names: &[&str] = match scheme {
        // curl deliberately ignores uppercase HTTP_PROXY because CGI servers
        // commonly expose an attacker-controlled Proxy header under that name.
        "http" | "ws" => &["http_proxy"],
        "https" | "wss" => &["https_proxy", "HTTPS_PROXY"],
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

    fn route(
        raw_target: &str,
        configured_proxy: Option<&str>,
        configured_no_proxy: Option<&str>,
        env: &[(&str, &str)],
    ) -> Result<ProxyRoute> {
        let env = env
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect::<HashMap<_, _>>();
        select_proxy_route_with_env(
            &Url::parse(raw_target).expect("test URL should parse"),
            configured_proxy,
            configured_no_proxy,
            |name| env.get(name).cloned(),
        )
    }

    #[test]
    fn explicit_proxy_and_no_proxy_select_one_route() {
        assert_eq!(
            route(
                "https://api.example.test/path",
                Some("http://proxy.test:8080"),
                None,
                &[],
            )
            .unwrap(),
            ProxyRoute::from_proxy_url("http://proxy.test:8080").unwrap()
        );
        assert_eq!(
            route(
                "https://api.example.test/path",
                Some("http://proxy.test:8080"),
                Some(".example.test"),
                &[],
            )
            .unwrap(),
            ProxyRoute::Direct
        );
    }

    #[test]
    fn environment_proxy_selection_is_shared_by_http_and_websocket_schemes() {
        assert!(
            route(
                "http://example.test/path",
                None,
                None,
                &[("http_proxy", "http://proxy.test:8080")],
            )
            .unwrap()
            .is_proxy()
        );
        assert!(
            route(
                "ws://example.test/socket",
                None,
                None,
                &[("http_proxy", "http://proxy.test:8080")],
            )
            .unwrap()
            .is_proxy()
        );
        assert!(
            route(
                "wss://example.test/socket",
                None,
                None,
                &[("HTTPS_PROXY", "http://proxy.test:8080")],
            )
            .unwrap()
            .is_proxy()
        );
        assert_eq!(
            route(
                "ws://example.test/socket",
                None,
                None,
                &[("HTTP_PROXY", "http://proxy.test:8080")],
            )
            .unwrap(),
            ProxyRoute::Direct
        );
    }

    #[test]
    fn empty_explicit_proxy_disables_environment_proxy_fallback() {
        assert_eq!(
            route(
                "http://example.test/path",
                Some(""),
                None,
                &[("http_proxy", "http://proxy.test:8080")],
            )
            .unwrap(),
            ProxyRoute::Direct
        );
    }

    #[test]
    fn no_proxy_handles_domains_ports_ip_cidrs_and_trailing_dots() {
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

    #[test]
    fn proxy_parser_accepts_only_remote_dns_schemes() {
        for (raw, scheme, port) in [
            ("proxy.test:8080", ProxyScheme::Http, 8080),
            ("https://proxy.test", ProxyScheme::Https, 443),
            ("socks5h://proxy.test", ProxyScheme::Socks5h, 1080),
            ("socks4a://proxy.test", ProxyScheme::Socks4a, 1080),
        ] {
            let proxy = SelectedProxy::parse(raw).unwrap();
            assert_eq!(proxy.scheme(), scheme);
            assert_eq!(proxy.endpoint_host(), "proxy.test");
            assert_eq!(proxy.endpoint_port(), port);
            assert_eq!(proxy.target_resolution(), ProxyTargetResolution::Proxy);
        }

        for scheme in ["socks5", "socks4"] {
            let error = SelectedProxy::parse(&format!("{scheme}://proxy.test:1080")).unwrap_err();
            assert!(error.to_string().contains("remote-DNS proxy scheme"));
        }
    }
}
