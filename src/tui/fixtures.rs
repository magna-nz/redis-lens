//! Test data shaped like the design mockup. Test-only.

use std::time::Duration;

use crate::conn::docker::DiscoveredInstance;
use crate::model::{
    Auth, Endpoint, Env, KeySample, KeyType, PERM_FREQ, PERM_INFO, PERM_MEMORY, PERM_SCAN,
    PERM_TYPE_TTL, PatternStats, PermCheck, PermStatus, PermissionReport, Profile, ScanReport,
    ServerFlavor, ServerInfo, ServerMode, Ttl,
};

pub const PROD_URL: &str = "rediss://lens-ro:Sup3rS3cret@prod-cache-02.internal:6380/0";

const KIB: f64 = 1024.0;
const MIB: f64 = KIB * 1024.0;
const GIB: f64 = MIB * 1024.0;
const TOTAL_KEYS: u64 = 7_920_000;
const SAMPLED: u64 = 100_000;

fn profile(name: &str, env: Env, host: &str, port: u16, tls: bool, auth: Auth) -> Profile {
    Profile {
        name: name.into(),
        env,
        endpoint: Endpoint {
            host: host.into(),
            port,
            tls,
        },
        db: 0,
        auth,
        read_only: None,
    }
}

/// Saved PROD, STAGE and LOCAL profiles.
pub fn profiles() -> Vec<Profile> {
    vec![
        profile(
            "prod-cache-01",
            Env::Prod,
            "prod-cache-01.internal",
            6380,
            true,
            Auth::Keychain,
        ),
        profile(
            "staging-cache",
            Env::Stage,
            "staging-cache.internal",
            6379,
            false,
            Auth::Keychain,
        ),
        profile(
            "local-6379",
            Env::Local,
            "localhost",
            6379,
            false,
            Auth::None,
        ),
    ]
}

pub fn docker_instance() -> DiscoveredInstance {
    DiscoveredInstance {
        container: "myapp-redis-1".into(),
        image: "redis:7.4-alpine".into(),
        profile: profile(
            "myapp-redis-1",
            Env::Local,
            "127.0.0.1",
            6390,
            false,
            Auth::None,
        ),
        credentials: None,
    }
}

pub fn server() -> ServerInfo {
    ServerInfo {
        flavor: ServerFlavor::Redis,
        version: "7.4.2".into(),
        mode: ServerMode::Standalone,
        used_memory: (14.8 * GIB) as u64,
        maxmemory: (24.0 * GIB) as u64,
        maxmemory_policy: "allkeys-lfu".into(),
        total_keys: TOTAL_KEYS,
    }
}

fn check(label: &'static str, status: PermStatus) -> PermCheck {
    PermCheck { label, status }
}

pub fn perms_ok() -> PermissionReport {
    PermissionReport {
        checks: vec![
            check(PERM_SCAN, PermStatus::Allowed),
            check(PERM_TYPE_TTL, PermStatus::Allowed),
            check(PERM_MEMORY, PermStatus::Allowed),
            check(PERM_FREQ, PermStatus::Allowed),
            check(PERM_INFO, PermStatus::Allowed),
        ],
    }
}

pub fn perms_memory_denied() -> PermissionReport {
    PermissionReport {
        checks: vec![
            check(PERM_SCAN, PermStatus::Allowed),
            check(PERM_TYPE_TTL, PermStatus::Allowed),
            check(
                PERM_MEMORY,
                PermStatus::Denied(
                    "NOPERM User lens-limited has no permissions to run the 'memory|usage' command"
                        .into(),
                ),
            ),
            check(
                PERM_FREQ,
                PermStatus::Skipped("eviction policy is not LFU".into()),
            ),
            check(PERM_INFO, PermStatus::Allowed),
        ],
    }
}

/// `"11h 04m"`, `"38s"`, `"none"` to a TTL.
fn ttl(s: &str) -> Ttl {
    if s == "none" {
        return Ttl::Persistent;
    }
    let mut secs = 0;
    for part in s.split_whitespace() {
        let (n, unit) = part.split_at(part.len() - 1);
        let n: u64 = n.parse().unwrap();
        secs += n * match unit {
            "s" => 1,
            "m" => 60,
            "h" => 3600,
            "d" => 86_400,
            _ => panic!("bad unit in {s}"),
        };
    }
    Ttl::Expires(Duration::from_secs(secs))
}

struct Pat {
    pattern: &'static str,
    key_type: KeyType,
    est_keys: u64,
    est_bytes: f64,
    no_ttl: f64,
    hist: [f64; 7],
    sizes: [f64; 3],
    examples: [(&'static str, f64, &'static str); 5],
}

fn build(p: &Pat, sizes: bool) -> PatternStats {
    let scale = TOTAL_KEYS as f64 / SAMPLED as f64;
    let sampled_keys = ((p.est_keys as f64 / scale).round() as u64).max(1);
    let opt = |v: f64| sizes.then_some(v as u64);
    let mut examples: Vec<KeySample> = p
        .examples
        .iter()
        .map(|(k, b, t)| KeySample {
            key: (*k).into(),
            key_type: p.key_type,
            bytes: opt(*b),
            ttl: ttl(t),
            lfu_freq: None,
        })
        .collect();
    examples.sort_by_key(|e| std::cmp::Reverse(e.bytes));
    PatternStats {
        pattern: p.pattern.into(),
        key_type: Some(p.key_type),
        sampled_keys,
        est_keys: p.est_keys,
        sampled_bytes: opt(p.est_bytes * sampled_keys as f64 / p.est_keys as f64),
        est_bytes: opt(p.est_bytes),
        no_ttl: (p.no_ttl * sampled_keys as f64).round() as u64,
        ttl_hist: p.hist.map(|f| (f * sampled_keys as f64).round() as u64),
        size_p50: opt(p.sizes[0]),
        size_p99: opt(p.sizes[1]),
        size_max: opt(p.sizes[2]),
        examples,
    }
}

fn pats() -> Vec<Pat> {
    vec![
        Pat {
            pattern: "session:{uuid}",
            key_type: KeyType::String,
            est_keys: 1_840_000,
            est_bytes: 6.1 * GIB,
            no_ttl: 0.0,
            hist: [0.0, 0.04, 0.31, 0.65, 0.0, 0.0, 0.0],
            sizes: [3.1 * KIB, 9.8 * KIB, 41.0 * KIB],
            examples: [
                (
                    "session:9f1c2a7e-4b3d-4e0a-9c61-2f8d7b1e5a43",
                    3.2 * KIB,
                    "11h 04m",
                ),
                (
                    "session:03be77d1-91aa-4f52-8e3c-6a0d24c9f1b8",
                    2.9 * KIB,
                    "2h 51m",
                ),
                (
                    "session:c41f0e9a-2d6b-47e8-b5a3-91e7d0c38f26",
                    41.0 * KIB,
                    "23h 12m",
                ),
                (
                    "session:7a2d9b14-6e0c-4b1f-a8d2-3c5e9f07b6a1",
                    3.4 * KIB,
                    "16h 40m",
                ),
                (
                    "session:e8b03c5d-7f21-4a96-9d4e-0b6c18a2f7e3",
                    2.7 * KIB,
                    "47m 00s",
                ),
            ],
        },
        Pat {
            pattern: "cache:search:{hash}",
            key_type: KeyType::String,
            est_keys: 1_120_000,
            est_bytes: 2.2 * GIB,
            no_ttl: 0.12,
            hist: [0.12, 0.0, 0.08, 0.80, 0.0, 0.0, 0.0],
            sizes: [1.8 * KIB, 7.4 * KIB, 96.0 * KIB],
            examples: [
                (
                    "cache:search:3f9a6c01e2b7d48f5a90c3e1b6d27f84a05c9e13",
                    1.9 * KIB,
                    "18h 22m",
                ),
                (
                    "cache:search:a17e4d9b3c02f68e1d5a7b40c9e23f6d18b5a7c2",
                    96.0 * KIB,
                    "none",
                ),
                (
                    "cache:search:0c5b8e2f7a91d34c6e0b29f5a8d17c3e4b6f9a01",
                    2.1 * KIB,
                    "3h 09m",
                ),
                (
                    "cache:search:d92f1a6c8e4b07d3a5c9e2f61b8d4a07c3e5f9b2",
                    11.0 * KIB,
                    "none",
                ),
                (
                    "cache:search:6e3a0d8b5f2c91e7a4d06b3f8c2e5a91d7b4c0f6",
                    2.4 * KIB,
                    "21h 47m",
                ),
            ],
        },
        Pat {
            pattern: "cache:product:{id}",
            key_type: KeyType::String,
            est_keys: 220_000,
            est_bytes: 1.4 * GIB,
            no_ttl: 0.0,
            hist: [0.0, 0.0, 0.18, 0.82, 0.0, 0.0, 0.0],
            sizes: [5.9 * KIB, 22.0 * KIB, 180.0 * KIB],
            examples: [
                ("cache:product:31337", 6.2 * KIB, "9h 41m"),
                ("cache:product:1024", 5.8 * KIB, "22h 03m"),
                ("cache:product:88410", 180.0 * KIB, "14h 30m"),
                ("cache:product:4471", 6.0 * KIB, "52m 00s"),
                ("cache:product:120009", 5.5 * KIB, "19h 16m"),
            ],
        },
        Pat {
            pattern: "user:{id}:cart",
            key_type: KeyType::Hash,
            est_keys: 388_000,
            est_bytes: 1.2 * GIB,
            no_ttl: 0.31,
            hist: [0.31, 0.0, 0.01, 0.06, 0.62, 0.0, 0.0],
            sizes: [2.8 * KIB, 41.0 * KIB, 212.0 * KIB],
            examples: [
                ("user:88213:cart", 3.1 * KIB, "6d 23h"),
                ("user:10442:cart", 212.0 * KIB, "none"),
                ("user:500917:cart", 2.6 * KIB, "4d 02h"),
                ("user:73:cart", 2.9 * KIB, "none"),
                ("user:331870:cart", 4.4 * KIB, "6d 11h"),
            ],
        },
        Pat {
            pattern: "tmp:import:{ulid}",
            key_type: KeyType::String,
            est_keys: 48_000,
            est_bytes: 0.72 * GIB,
            no_ttl: 1.0,
            hist: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            sizes: [14.0 * KIB, 31.0 * KIB, 88.0 * KIB],
            examples: [
                ("tmp:import:01JCDX4N8Q2R7V3K9M5T6W1YZB", 14.0 * KIB, "none"),
                ("tmp:import:01JCDX4P2F6H8J0L3N5Q7S9UWX", 31.0 * KIB, "none"),
                ("tmp:import:01JCDY0A1B3C5D7E9F2G4H6J8K", 13.0 * KIB, "none"),
                ("tmp:import:01JCE2M7N9P1Q3R5S7T9V2W4XY", 88.0 * KIB, "none"),
                ("tmp:import:01JCE5Z3Y1X9W7V5T3S1R8Q6PN", 15.0 * KIB, "none"),
            ],
        },
        Pat {
            pattern: "queue:emails",
            key_type: KeyType::List,
            est_keys: 1,
            est_bytes: 612.0 * MIB,
            no_ttl: 1.0,
            hist: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            sizes: [612.0 * MIB, 612.0 * MIB, 612.0 * MIB],
            examples: [
                ("queue:emails", 612.0 * MIB, "none"),
                ("", 0.0, "none"),
                ("", 0.0, "none"),
                ("", 0.0, "none"),
                ("", 0.0, "none"),
            ],
        },
        Pat {
            pattern: "user:{id}:profile",
            key_type: KeyType::Hash,
            est_keys: 412_000,
            est_bytes: 0.54 * GIB,
            no_ttl: 1.0,
            hist: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            sizes: [1.2 * KIB, 3.9 * KIB, 8.1 * KIB],
            examples: [
                ("user:88213:profile", 1.2 * KIB, "none"),
                ("user:10442:profile", 1.4 * KIB, "none"),
                ("user:500917:profile", 1.1 * KIB, "none"),
                ("user:73:profile", 8.1 * KIB, "none"),
                ("user:331870:profile", 1.3 * KIB, "none"),
            ],
        },
        Pat {
            pattern: "rl:{ip}:{id}",
            key_type: KeyType::String,
            est_keys: 2_310_000,
            est_bytes: 0.41 * GIB,
            no_ttl: 0.0,
            hist: [0.0, 0.97, 0.03, 0.0, 0.0, 0.0, 0.0],
            sizes: [160.0, 210.0, 240.0],
            examples: [
                ("rl:10.0.4.12:29187460", 162.0, "38s"),
                ("rl:172.16.9.201:29187460", 158.0, "38s"),
                ("rl:10.0.7.88:29187459", 171.0, "2s"),
                ("rl:192.168.3.40:29187460", 160.0, "38s"),
                ("rl:10.0.4.12:29187459", 162.0, "2s"),
            ],
        },
        Pat {
            pattern: "user:{id}:feed",
            key_type: KeyType::ZSet,
            est_keys: 96_000,
            est_bytes: 0.18 * GIB,
            no_ttl: 0.0,
            hist: [0.0, 0.0, 0.0, 0.22, 0.78, 0.0, 0.0],
            sizes: [1.4 * KIB, 11.0 * KIB, 64.0 * KIB],
            examples: [
                ("user:88213:feed", 1.6 * KIB, "5d 08h"),
                ("user:204551:feed", 64.0 * KIB, "6d 19h"),
                ("user:10442:feed", 1.2 * KIB, "2d 13h"),
                ("user:931:feed", 2.0 * KIB, "18h 05m"),
                ("user:500917:feed", 1.4 * KIB, "6d 02h"),
            ],
        },
        Pat {
            pattern: "feature:{var}",
            key_type: KeyType::String,
            est_keys: 212,
            est_bytes: 302.0 * KIB,
            no_ttl: 1.0,
            hist: [1.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
            sizes: [52.0, 12.0 * KIB, 12.0 * KIB],
            examples: [
                ("feature:checkout-v2", 48.0, "none"),
                ("feature:dark-mode", 52.0, "none"),
                ("feature:pricing-rules", 12.0 * KIB, "none"),
                ("feature:search-ranker", 3.9 * KIB, "none"),
                ("feature:beta-banner", 40.0, "none"),
            ],
        },
        Pat {
            pattern: "lock:{var}",
            key_type: KeyType::String,
            est_keys: 14,
            est_bytes: 2100.0,
            no_ttl: 0.0,
            hist: [0.0, 0.14, 0.86, 0.0, 0.0, 0.0, 0.0],
            sizes: [150.0, 152.0, 152.0],
            examples: [
                ("lock:billing-run", 150.0, "24m 10s"),
                ("lock:nightly-export", 148.0, "8m 02s"),
                ("lock:reindex:products", 152.0, "41s"),
                ("lock:import:01JCE5Z3Y1", 149.0, "5m 33s"),
                ("lock:sitemap", 146.0, "12m 47s"),
            ],
        },
    ]
}

/// A finished scan. Without sizes every byte figure is `None`, as when
/// `MEMORY USAGE` is denied.
pub fn report(sizes: bool) -> ScanReport {
    let mut patterns: Vec<PatternStats> = pats()
        .iter()
        .map(|p| {
            let mut s = build(p, sizes);
            s.examples.retain(|e| !e.key.is_empty());
            s
        })
        .collect();
    if sizes {
        patterns.sort_by_key(|p| std::cmp::Reverse(p.est_bytes));
    } else {
        patterns.sort_by_key(|p| std::cmp::Reverse(p.est_keys));
    }
    ScanReport {
        profile: "prod-cache-01".into(),
        // 2026-11-10 14:02:11 UTC
        taken_at_unix: 1_794_319_331,
        server: server(),
        sampled_keys: SAMPLED,
        patterns,
        sizes_available: sizes,
    }
}
