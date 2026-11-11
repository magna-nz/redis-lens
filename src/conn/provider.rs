//! Managed services recognised from the host name, and what each one degrades.
//!
//! Owner: connections. A provider is a hint for notices and the default scan rate; the
//! permission check stays authoritative for what works.

use crate::model::Provider;

/// The managed service `host` belongs to, when the host name shows it. A hint only.
///
/// - `<name>.serverless.<region>.cache.amazonaws.com`: ElastiCache Serverless.
/// - any other `*.cache.amazonaws.com`: ElastiCache (node-based).
/// - `*.memorydb.<region>.amazonaws.com`: MemoryDB.
/// - `*.redis.cache.windows.net` (and the China and US Government clouds'
///   `*.redis.cache.chinacloudapi.cn`, `*.redis.cache.usgovcloudapi.net`),
///   `*.redisenterprise.cache.azure.net` and `*.redis.azure.net`: Azure.
///
/// The AWS patterns also match the China regions' `.amazonaws.com.cn`. Memorystore uses
/// private IP addresses, so it is never recognised: only `conn add --provider` sets it.
pub fn classify(host: &str) -> Option<Provider> {
    let h = host.trim().trim_end_matches('.').to_ascii_lowercase();
    let aws = h
        .strip_suffix(".amazonaws.com")
        .or_else(|| h.strip_suffix(".amazonaws.com.cn"));
    if let Some(rest) = aws {
        if let Some(name) = rest.strip_suffix(".cache") {
            // <name>-<id>.serverless.<region>: "serverless" is the label before the
            // region. A node-based cluster that happens to be named "serverless" has
            // its id there instead (master.serverless.abc123.use1).
            let labels: Vec<&str> = name.split('.').collect();
            if labels.iter().any(|l| l.is_empty()) {
                return None;
            }
            return Some(
                if labels.len() >= 3 && labels[labels.len() - 2] == "serverless" {
                    Provider::ElasticacheServerless
                } else {
                    Provider::Elasticache
                },
            );
        }
        // <…>.memorydb.<region>
        let labels: Vec<&str> = rest.split('.').collect();
        if labels.len() >= 3
            && labels[labels.len() - 2] == "memorydb"
            && labels.iter().all(|l| !l.is_empty())
        {
            return Some(Provider::Memorydb);
        }
        return None;
    }
    const AZURE: [&str; 5] = [
        ".redis.cache.windows.net",
        ".redis.cache.chinacloudapi.cn",
        ".redis.cache.usgovcloudapi.net",
        ".redisenterprise.cache.azure.net",
        ".redis.azure.net",
    ];
    AZURE
        .iter()
        .any(|suffix| {
            h.strip_suffix(suffix)
                .is_some_and(|name| !name.is_empty() && !name.ends_with('.'))
        })
        .then_some(Provider::Azure)
}

/// The provider's name as shown in `conn list` and the TUI.
pub fn label(provider: Provider) -> &'static str {
    match provider {
        Provider::Elasticache => "ElastiCache",
        Provider::ElasticacheServerless => "ElastiCache Serverless",
        Provider::Memorydb => "MemoryDB",
        Provider::Azure => "Azure",
        Provider::Memorystore => "Memorystore",
    }
}

/// Default command budget on ElastiCache Serverless, commands per second (see
/// [`crate::scan::ScanOptions::for_profile`]).
pub const SERVERLESS_MAX_RATE: u32 = 1_000;

/// A one-line notice shown before a scan of a profile on `provider`, saying which
/// figures will be missing and why, or `None` when nothing degrades. At most 115
/// characters, so it fits the Connections detail panel of a 120-column terminal.
pub fn notice(provider: Option<Provider>) -> Option<&'static str> {
    match provider? {
        // AWS's supported-commands page lists MEMORY (with MEMORY USAGE) and OBJECT
        // (with OBJECT FREQ) among the commands restricted on serverless caches, and
        // warns that frequent or large SCANs there can raise latency.
        Provider::ElasticacheServerless => Some(
            "ElastiCache Serverless restricts MEMORY and OBJECT, so sizes and hot keys are \
             missing; counts and TTLs still work.",
        ),
        // redis.io marks MEMORY STATS as unsupported on Redis Software and Redis Cloud,
        // which Azure Managed Redis and the Enterprise tiers run on. The host name does
        // not always tell the tiers apart, so the notice hedges.
        Provider::Azure => Some(
            "Azure Managed Redis and Enterprise tiers don't support MEMORY STATS, so \
             dataset.bytes may be missing.",
        ),
        // Node-based ElastiCache, MemoryDB and Memorystore allow every command a scan
        // sends; the permission check reports anything a parameter group or ACL takes
        // away.
        Provider::Elasticache | Provider::Memorydb | Provider::Memorystore => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn elasticache_serverless() {
        for host in [
            "my-cache-abc123.serverless.use1.cache.amazonaws.com",
            "MY-CACHE-ABC123.SERVERLESS.USE1.CACHE.AMAZONAWS.COM",
            "my-cache-abc123.serverless.use1.cache.amazonaws.com.",
            "orders-x1y2z3.serverless.euw1.cache.amazonaws.com",
            "c-1a2b3c.serverless.cnn1.cache.amazonaws.com.cn",
        ] {
            assert_eq!(
                classify(host),
                Some(Provider::ElasticacheServerless),
                "{host}"
            );
        }
    }

    #[test]
    fn elasticache_node_based() {
        for host in [
            "master.my-cluster.abc123.use1.cache.amazonaws.com",
            "replica.my-cluster.abc123.use1.cache.amazonaws.com",
            "clustercfg.my-cluster.abc123.use1.cache.amazonaws.com",
            "my-cluster.abc123.ng.0001.use1.cache.amazonaws.com",
            "my-cluster-001.abc123.0001.use1.cache.amazonaws.com",
            // A node-based cluster named "serverless".
            "master.serverless.abc123.use1.cache.amazonaws.com",
            "master.c.abc.cnn1.cache.amazonaws.com.cn",
        ] {
            assert_eq!(classify(host), Some(Provider::Elasticache), "{host}");
        }
    }

    #[test]
    fn memorydb() {
        for host in [
            "clustercfg.my-db.abc123.memorydb.us-east-1.amazonaws.com",
            "my-db-0001-001.my-db.abc123.memorydb.eu-west-1.amazonaws.com",
            "clustercfg.x.memorydb.cn-north-1.amazonaws.com.cn",
        ] {
            assert_eq!(classify(host), Some(Provider::Memorydb), "{host}");
        }
    }

    #[test]
    fn azure() {
        for host in [
            "contoso.redis.cache.windows.net",
            "Contoso.Redis.Cache.Windows.Net",
            "contoso.redis.cache.chinacloudapi.cn",
            "contoso.redis.cache.usgovcloudapi.net",
            "contoso.eastus.redisenterprise.cache.azure.net",
            "contoso.eastus.redis.azure.net",
        ] {
            assert_eq!(classify(host), Some(Provider::Azure), "{host}");
        }
    }

    #[test]
    fn negatives() {
        for host in [
            "",
            "localhost",
            "127.0.0.1",
            "10.0.1.21",
            "::1",
            "cache.amazonaws.com",
            ".cache.amazonaws.com",
            "evilcache.amazonaws.com",
            "x.cache.amazonaws.com.evil.example",
            "x..cache.amazonaws.com",
            "my-bucket.s3.amazonaws.com",
            "memorydb.amazonaws.com",
            "x.memorydb.amazonaws.com",
            "x.memorydb.example.com",
            "serverless.example.com",
            "redis.cache.windows.net",
            ".redis.cache.windows.net",
            "contosoredis.cache.windows.net",
            "contoso.redis.cache.windows.net.evil.example",
            "redis.azure.net",
            "prod-cache-02.internal",
            // Memorystore: private addresses only, never recognised.
            "10.128.0.5",
        ] {
            assert_eq!(classify(host), None, "{host:?}");
        }
    }

    #[test]
    fn notices() {
        assert_eq!(notice(None), None);
        let serverless = notice(Some(Provider::ElasticacheServerless)).unwrap();
        assert_eq!(
            serverless,
            "ElastiCache Serverless restricts MEMORY and OBJECT, so sizes and hot keys are \
             missing; counts and TTLs still work."
        );
        let azure = notice(Some(Provider::Azure)).unwrap();
        assert_eq!(
            azure,
            "Azure Managed Redis and Enterprise tiers don't support MEMORY STATS, so \
             dataset.bytes may be missing."
        );
        for p in [
            Provider::Elasticache,
            Provider::Memorydb,
            Provider::Memorystore,
        ] {
            assert_eq!(notice(Some(p)), None, "{p:?}");
        }
        // One short line each.
        for n in [serverless, azure] {
            assert!(!n.contains('\n'), "{n}");
            assert!(!n.contains("  "), "{n}");
            assert!(n.chars().count() <= 115, "{} chars: {n}", n.chars().count());
        }
    }

    #[test]
    fn labels() {
        assert_eq!(
            label(Provider::ElasticacheServerless),
            "ElastiCache Serverless"
        );
        assert_eq!(label(Provider::Memorydb), "MemoryDB");
    }

    proptest! {
        #[test]
        fn classify_never_panics(host in ".{0,80}") {
            let _ = classify(&host);
        }

        #[test]
        fn any_label_before_a_known_suffix_is_recognised(name in "[a-z0-9][a-z0-9-]{0,20}") {
            let windows = format!("{name}.redis.cache.windows.net");
            prop_assert_eq!(classify(&windows), Some(Provider::Azure));
            let serverless = format!("{name}.serverless.use1.cache.amazonaws.com");
            prop_assert_eq!(classify(&serverless), Some(Provider::ElasticacheServerless));
        }
    }
}
