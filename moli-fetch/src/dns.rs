use anyhow::{Result, bail};
use moli_curl::CurlDnsResolution;
use moli_dns_resolver::DnsTarget;
use url::{Host, Url};

use crate::{
    FetchConfig,
    blocking::{normalized_http_host_resolve_entries, resolve_host_resolve_override_ips},
    proxy::HttpProxyRoute,
};

/// Fetch-side DNS admission decision.
///
/// The shared resolver is used only when Fetch can prove that curl will
/// connect directly to an HTTP(S) origin. Proxy traffic stays curl-managed
/// only when no address policy is active; otherwise a proxy-resolved hostname
/// cannot be verified locally and is rejected. IP literals and matching
/// explicit host-resolve entries already have exact routing and are checked
/// synchronously before this decision.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FetchCurlDnsAdmission {
    CurlManaged,
    SharedResolver(DnsTarget),
}

pub(crate) fn curl_dns_resolution(
    config: &FetchConfig,
    url: &Url,
    proxy_route: &HttpProxyRoute,
) -> Result<CurlDnsResolution> {
    match curl_dns_admission(config, url, proxy_route)? {
        FetchCurlDnsAdmission::CurlManaged => Ok(CurlDnsResolution::curl_managed()),
        FetchCurlDnsAdmission::SharedResolver(target) => {
            let policy = config.network_address_policy();
            Ok(CurlDnsResolution::resolve_origin(
                target,
                normalized_http_host_resolve_entries(
                    config.http_host_resolve(),
                    policy.is_enforced(),
                )?,
            )
            .with_network_address_policy(policy, url.to_string()))
        }
    }
}

fn curl_dns_admission(
    config: &FetchConfig,
    url: &Url,
    proxy_route: &HttpProxyRoute,
) -> Result<FetchCurlDnsAdmission> {
    if !matches!(url.scheme(), "http" | "https") {
        return Ok(FetchCurlDnsAdmission::CurlManaged);
    }
    let Some(Host::Domain(host)) = url.host() else {
        return Ok(FetchCurlDnsAdmission::CurlManaged);
    };
    let Some(port) = url.port_or_known_default() else {
        return Ok(FetchCurlDnsAdmission::CurlManaged);
    };
    if proxy_route.is_proxy() {
        if config.network_address_policy().is_enforced() {
            bail!(
                "cannot enforce network address policy for proxied hostname `{host}` in `{url}`; the proxy must not resolve an unchecked target hostname"
            );
        }
        return Ok(FetchCurlDnsAdmission::CurlManaged);
    }
    if resolve_host_resolve_override_ips(config.http_host_resolve(), host, port)?.is_some() {
        return Ok(FetchCurlDnsAdmission::CurlManaged);
    }
    Ok(FetchCurlDnsAdmission::SharedResolver(DnsTarget::new(
        host, port,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission(
        config: &FetchConfig,
        raw_url: &str,
        proxy_route: &HttpProxyRoute,
    ) -> FetchCurlDnsAdmission {
        admission_result(config, raw_url, proxy_route).expect("test DNS admission should succeed")
    }

    fn admission_result(
        config: &FetchConfig,
        raw_url: &str,
        proxy_route: &HttpProxyRoute,
    ) -> Result<FetchCurlDnsAdmission> {
        curl_dns_admission(
            config,
            &Url::parse(raw_url).expect("test URL should parse"),
            proxy_route,
        )
    }

    fn shared_target(host: &str, port: u16) -> FetchCurlDnsAdmission {
        FetchCurlDnsAdmission::SharedResolver(DnsTarget::new(host, port))
    }

    #[test]
    fn direct_http_and_https_domains_use_shared_resolution() {
        let config = FetchConfig::default();

        assert_eq!(
            admission(&config, "http://example.test/path", &HttpProxyRoute::Direct),
            shared_target("example.test", 80)
        );
        assert_eq!(
            admission(
                &config,
                "https://example.test:8443/path",
                &HttpProxyRoute::Direct,
            ),
            shared_target("example.test", 8443)
        );
    }

    #[test]
    fn ip_literals_and_matching_host_resolve_entries_stay_curl_managed() {
        let mut config = FetchConfig::default();

        assert_eq!(
            admission(&config, "http://127.0.0.1/path", &HttpProxyRoute::Direct,),
            FetchCurlDnsAdmission::CurlManaged
        );
        assert_eq!(
            admission(&config, "http://[::1]/path", &HttpProxyRoute::Direct,),
            FetchCurlDnsAdmission::CurlManaged
        );
        config.set_http_host_resolve(vec!["example.test:80:127.0.0.1".to_owned()]);
        assert_eq!(
            admission(&config, "http://example.test/path", &HttpProxyRoute::Direct,),
            FetchCurlDnsAdmission::CurlManaged
        );
        assert_eq!(
            admission(&config, "http://other.test/path", &HttpProxyRoute::Direct,),
            shared_target("other.test", 80),
            "an unrelated host-resolve entry must not return DNS ownership to curl"
        );
    }

    #[test]
    fn selected_proxy_route_uses_curl_resolution() {
        let config = FetchConfig::default();

        assert_eq!(
            admission(
                &config,
                "https://api.example.test/path",
                &HttpProxyRoute::Proxy("http://proxy.test:8080".to_owned()),
            ),
            FetchCurlDnsAdmission::CurlManaged
        );
    }

    #[test]
    fn enforced_policy_rejects_proxy_resolved_hostname() {
        let mut config = FetchConfig::default();
        config.set_network_blocking(true, Vec::new());

        let error = admission_result(
            &config,
            "https://api.example.test/path",
            &HttpProxyRoute::Proxy("http://proxy.test:8080".to_owned()),
        )
        .expect_err("a strict policy cannot verify proxy-side DNS");

        assert!(error.to_string().contains("proxied hostname"));
    }
}
