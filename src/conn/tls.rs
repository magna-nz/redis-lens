//! Trust roots for TLS connections.
//!
//! Server certificates are checked against the OS certificate store (macOS Keychain,
//! Windows certificate store, the CA bundle on Linux; `SSL_CERT_FILE` / `SSL_CERT_DIR`
//! replace it when set). Certificates or files that fail to load are skipped rather
//! than failing the connection.
//!
//! The bundled roots are Mozilla's, through the CCADB (`webpki-root-certs`, the same
//! data as the webpki-roots that redis-rs ships). How they combine with the OS store
//! depends on the platform ([`Platform`]):
//!
//! - **macOS and Linux:** the OS store on its own. Only when it yields no usable
//!   certificate at all (a slim container without `ca-certificates`, say) are the
//!   bundled roots used, instead of it, never in addition.
//! - **Windows:** the OS store *and* the bundled roots, deduplicated. Windows fills its
//!   root store lazily (Automatic Root Update fetches a public root the first time
//!   something asks Windows to verify a chain that needs it), and rustls never asks, so
//!   a public root not fetched yet would be refused with `UnknownIssuer`.
//!
//! Either way redis-rs gets one explicit store and uses it in place of its own default
//! (redis-rs 1.8.1 replaces, it does not merge; a test pins that version). The store is
//! loaded once per process and shared by every connection, the replica candidates
//! included. Nothing here turns certificate or host name checks off.

use std::collections::HashSet;
use std::sync::OnceLock;

use base64::Engine as _;
use redis::{ConnectionAddr, TlsCertificates};
use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;

/// The bundled roots, as certificates.
fn bundled_certs() -> &'static [CertificateDer<'static>] {
    webpki_root_certs::TLS_SERVER_ROOT_CERTS
}

/// How the OS store and the bundled roots combine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Platform {
    /// macOS and Linux: the OS store on its own, the bundled roots only when it has no
    /// usable certificate.
    OsStoreAlone,
    /// Windows: the OS store plus the bundled roots, because the OS store is filled
    /// lazily and may not hold a public root yet.
    OsStoreAndBundled,
}

impl Platform {
    /// The platform this binary was built for.
    pub(crate) fn current() -> Self {
        if cfg!(windows) {
            Self::OsStoreAndBundled
        } else {
            Self::OsStoreAlone
        }
    }
}

/// Which trust roots were chosen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Roots {
    /// Certificates from the OS store that rustls accepts as trust anchors.
    Native(Vec<CertificateDer<'static>>),
    /// Windows: the usable OS certificates, and the bundled certificates that are not
    /// already among them (compared by DER bytes).
    NativeAndBundled {
        native: Vec<CertificateDer<'static>>,
        bundled: Vec<CertificateDer<'static>>,
    },
    /// The OS store had no usable certificate: the bundled roots alone.
    Bundled,
}

/// The decision made from what the OS store returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootChoice {
    pub(crate) roots: Roots,
    /// Certificates or files that failed to load or were not usable, and were skipped.
    pub(crate) skipped: usize,
}

/// [`choose_roots_for`] on the platform this binary was built for.
pub(crate) fn choose_roots(certs: Vec<CertificateDer<'static>>, load_errors: usize) -> RootChoice {
    choose_roots_for(certs, load_errors, Platform::current())
}

/// Decide the trust roots from what loading the OS store returned: the certificates
/// that loaded, and how many certificates or files failed to.
///
/// Every certificate rustls refuses as a trust anchor is skipped and counted with the
/// load errors. With no usable certificate, the bundled roots are used on every
/// platform. Otherwise [`Platform::OsStoreAlone`] uses the OS store on its own and
/// [`Platform::OsStoreAndBundled`] adds the bundled certificates it does not already
/// hold.
pub(crate) fn choose_roots_for(
    certs: Vec<CertificateDer<'static>>,
    load_errors: usize,
    platform: Platform,
) -> RootChoice {
    let mut probe = RootCertStore::empty();
    let mut usable = Vec::with_capacity(certs.len());
    let mut skipped = load_errors;
    for cert in certs {
        if probe.add(cert.clone()).is_ok() {
            usable.push(cert);
        } else {
            skipped += 1;
        }
    }
    let roots = if usable.is_empty() {
        Roots::Bundled
    } else {
        match platform {
            Platform::OsStoreAlone => Roots::Native(usable),
            Platform::OsStoreAndBundled => {
                let have: HashSet<&[u8]> = usable.iter().map(|c| c.as_ref()).collect();
                let bundled = bundled_certs()
                    .iter()
                    .filter(|c| !have.contains(c.as_ref()))
                    .cloned()
                    .collect();
                Roots::NativeAndBundled {
                    native: usable,
                    bundled,
                }
            }
        }
    };
    RootChoice { roots, skipped }
}

/// The trust roots every TLS connection uses, ready to hand to redis-rs.
pub(crate) struct TlsRoots {
    /// A TLS address whose `tls_params` carry the chosen store (redis-rs does not export
    /// the parameter type, so the address is kept as a template). `None` only if redis-rs
    /// refused the store, which is not expected: it then falls back to its own default,
    /// the bundled webpki roots (feature `tls-rustls-webpki-roots`).
    template: Option<ConnectionAddr>,
    /// How many OS certificates are trusted.
    native: usize,
    /// How many bundled certificates are trusted on top of them (all of them when the OS
    /// store had none; on Windows, those the OS store did not already hold).
    bundled: usize,
    /// Certificates or files that were skipped. Kept for diagnostics only: nothing prints
    /// it on connect.
    skipped: usize,
}

impl std::fmt::Debug for TlsRoots {
    /// Counts only; the certificates themselves are not printed. `native` and `bundled`
    /// both 0 means redis-rs's default store (not expected, see [`TlsRoots::from_choice`]).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsRoots")
            .field("native", &self.native)
            .field("bundled", &self.bundled)
            .field("skipped", &self.skipped)
            .finish()
    }
}

impl TlsRoots {
    pub(crate) fn from_choice(choice: RootChoice) -> Self {
        let RootChoice { roots, skipped } = choice;
        let (native, bundled) = match roots {
            Roots::Native(certs) => (certs, Vec::new()),
            Roots::NativeAndBundled { native, bundled } => (native, bundled),
            Roots::Bundled => (Vec::new(), bundled_certs().to_vec()),
        };
        let all: Vec<_> = native.iter().chain(&bundled).cloned().collect();
        match template_from_pem(pem_bundle(&all)) {
            Some(template) => Self {
                template: Some(template),
                native: native.len(),
                bundled: bundled.len(),
                skipped,
            },
            // Not expected: every certificate passed the same `RootCertStore::add` check
            // redis-rs makes, the PEM round-trips, and a test builds the bundled store.
            // redis-rs's default is still a verified store: the bundled webpki roots.
            None => Self {
                template: None,
                native: 0,
                bundled: 0,
                skipped: skipped + native.len(),
            },
        }
    }

    /// Whether only the bundled roots are trusted, because the OS store had none.
    // Part of the module's in-crate API; only tests call it today.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn is_bundled(&self) -> bool {
        self.native == 0
    }

    /// The TLS address for `host:port` with these roots. `insecure` is always `false`, so
    /// the certificate is checked against these roots and against `host`.
    pub(crate) fn tls_addr(&self, host: &str, port: u16) -> ConnectionAddr {
        let tls_params = match &self.template {
            Some(ConnectionAddr::TcpTls { tls_params, .. }) => tls_params.clone(),
            _ => None,
        };
        ConnectionAddr::TcpTls {
            host: host.to_string(),
            port,
            insecure: false,
            tls_params,
        }
    }
}

/// Encode certificates as one PEM bundle, the form redis-rs takes trust roots in.
fn pem_bundle(certs: &[CertificateDer<'_>]) -> Vec<u8> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut out = String::new();
    for cert in certs {
        out.push_str("-----BEGIN CERTIFICATE-----\n");
        let encoded = b64.encode(cert.as_ref());
        for line in encoded.as_bytes().chunks(64) {
            // Base64 output is ASCII, so every chunk is valid UTF-8.
            out.push_str(std::str::from_utf8(line).unwrap_or_default());
            out.push('\n');
        }
        out.push_str("-----END CERTIFICATE-----\n");
    }
    out.into_bytes()
}

/// A TLS address (for a placeholder host) whose parameters make `pem` the only trust
/// roots, or `None` if redis-rs refuses them. Its `TlsConnParams` can only be built
/// through `Client::build_with_tls`, so a client is built and its address taken out.
fn template_from_pem(pem: Vec<u8>) -> Option<ConnectionAddr> {
    let addr = ConnectionAddr::TcpTls {
        host: "localhost".to_string(),
        port: 6379,
        insecure: false,
        tls_params: None,
    };
    let certs = TlsCertificates {
        client_tls: None,
        root_cert: Some(pem),
    };
    let client = redis::Client::build_with_tls(addr, certs).ok()?;
    match client.get_connection_info().addr() {
        addr @ ConnectionAddr::TcpTls {
            tls_params: Some(_),
            ..
        } => Some(addr.clone()),
        _ => None,
    }
}

/// Load the OS store and decide, as [`choose_roots`] does for this platform.
fn load_native() -> TlsRoots {
    let loaded = rustls_native_certs::load_native_certs();
    TlsRoots::from_choice(choose_roots(loaded.certs, loaded.errors.len()))
}

static ROOTS: OnceLock<TlsRoots> = OnceLock::new();

/// `cell`'s value, running `load` only if it has none yet.
fn cached(cell: &OnceLock<TlsRoots>, load: impl FnOnce() -> TlsRoots) -> &TlsRoots {
    cell.get_or_init(load)
}

/// The process's trust roots, loaded on first use.
pub(crate) fn tls_roots() -> &'static TlsRoots {
    cached(&ROOTS, load_native)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use Platform::{OsStoreAlone, OsStoreAndBundled};

    fn ca_cert(name: &str) -> CertificateDer<'static> {
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, name);
        let key = rcgen::KeyPair::generate().unwrap();
        params.self_signed(&key).unwrap().der().clone()
    }

    fn garbage() -> CertificateDer<'static> {
        CertificateDer::from(b"not a certificate".to_vec())
    }

    /// How many trust anchors the store in `addr`'s `tls_params` holds. `TlsConnParams`
    /// keeps its fields private; its `Debug` shows `root_cert_store: Some(RootCertStore
    /// { roots: "(N roots)" })`.
    fn root_count(addr: &ConnectionAddr) -> usize {
        let ConnectionAddr::TcpTls {
            tls_params: Some(params),
            ..
        } = addr
        else {
            panic!("expected TLS params in {addr:?}");
        };
        let debug = format!("{params:?}");
        let (_, rest) = debug
            .split_once("root_cert_store: Some(RootCertStore { roots: \"(")
            .unwrap_or_else(|| panic!("no custom root store in {debug}"));
        let (n, _) = rest
            .split_once(" roots)")
            .unwrap_or_else(|| panic!("unexpected root store format in {debug}"));
        n.parse()
            .unwrap_or_else(|_| panic!("not a count in {debug}"))
    }

    #[test]
    fn the_current_platform_merges_only_on_windows() {
        let expected = if cfg!(windows) {
            OsStoreAndBundled
        } else {
            OsStoreAlone
        };
        assert_eq!(Platform::current(), expected);
        let certs = vec![ca_cert("a")];
        assert_eq!(
            choose_roots(certs.clone(), 0),
            choose_roots_for(certs, 0, expected)
        );
    }

    #[test]
    fn a_non_empty_os_store_is_used_on_its_own() {
        let certs = vec![ca_cert("a"), ca_cert("b")];
        let choice = choose_roots_for(certs.clone(), 0, OsStoreAlone);
        assert_eq!(
            choice,
            RootChoice {
                roots: Roots::Native(certs),
                skipped: 0
            }
        );
    }

    #[test]
    fn on_windows_the_os_store_gets_the_bundled_roots_added() {
        let certs = vec![ca_cert("a"), ca_cert("b")];
        let choice = choose_roots_for(certs.clone(), 1, OsStoreAndBundled);
        assert_eq!(
            choice,
            RootChoice {
                roots: Roots::NativeAndBundled {
                    native: certs,
                    bundled: bundled_certs().to_vec(),
                },
                skipped: 1
            }
        );
    }

    #[test]
    fn on_windows_bundled_roots_the_os_store_already_has_are_not_added_twice() {
        let own = ca_cert("a");
        let shared = bundled_certs()[0].clone();
        let choice = choose_roots_for(vec![own.clone(), shared.clone()], 0, OsStoreAndBundled);
        let Roots::NativeAndBundled { native, bundled } = &choice.roots else {
            panic!("expected a merge, got {choice:?}");
        };
        assert_eq!(native, &vec![own, shared.clone()]);
        assert_eq!(bundled.len(), bundled_certs().len() - 1);
        assert!(!bundled.contains(&shared));

        let roots = TlsRoots::from_choice(choice);
        assert_eq!(
            (roots.native, roots.bundled),
            (2, bundled_certs().len() - 1)
        );
        assert!(!roots.is_bundled());
        // One store holding each certificate once.
        assert_eq!(
            root_count(&roots.tls_addr("db.example", 6380)),
            1 + bundled_certs().len()
        );
    }

    #[test]
    fn an_empty_os_store_falls_back_to_the_bundled_roots() {
        for platform in [OsStoreAlone, OsStoreAndBundled] {
            assert_eq!(
                choose_roots_for(Vec::new(), 0, platform),
                RootChoice {
                    roots: Roots::Bundled,
                    skipped: 0
                },
                "{platform:?}"
            );
        }
    }

    #[test]
    fn partial_load_errors_are_skipped() {
        let good = ca_cert("good");
        // Two files failed to load and one certificate is not usable.
        let choice = choose_roots_for(vec![garbage(), good.clone()], 2, OsStoreAlone);
        assert_eq!(
            choice,
            RootChoice {
                roots: Roots::Native(vec![good.clone()]),
                skipped: 3
            }
        );
        let choice = choose_roots_for(vec![garbage(), good.clone()], 2, OsStoreAndBundled);
        assert!(
            matches!(&choice.roots, Roots::NativeAndBundled { native, .. } if *native == vec![good]),
            "{choice:?}"
        );
        assert_eq!(choice.skipped, 3);
    }

    #[test]
    fn when_every_certificate_fails_the_bundled_roots_are_used() {
        for platform in [OsStoreAlone, OsStoreAndBundled] {
            let choice = choose_roots_for(vec![garbage(), garbage()], 4, platform);
            assert_eq!(
                choice,
                RootChoice {
                    roots: Roots::Bundled,
                    skipped: 6
                },
                "{platform:?}"
            );
        }
    }

    #[test]
    fn native_roots_become_tls_params_with_exactly_those_roots() {
        let choice = choose_roots_for(
            vec![ca_cert("a"), ca_cert("b"), ca_cert("c")],
            1,
            OsStoreAlone,
        );
        let roots = TlsRoots::from_choice(choice);
        assert!(!roots.is_bundled());
        assert_eq!((roots.native, roots.bundled, roots.skipped), (3, 0, 1));
        let addr = roots.tls_addr("db.example", 6380);
        assert!(matches!(
            addr,
            ConnectionAddr::TcpTls {
                insecure: false,
                tls_params: Some(_),
                ..
            }
        ));
        // Exactly the three: had redis-rs merged its default store in, there would be
        // more.
        assert_eq!(root_count(&addr), 3);
    }

    /// Catches a redis-rs that merges its default store into the one it is given: the
    /// template from N certificates must hold N roots, no more.
    #[test]
    fn a_template_holds_exactly_the_given_roots() {
        for n in [1, 2, 5] {
            let certs: Vec<_> = (0..n).map(|i| ca_cert(&format!("ca {i}"))).collect();
            let template = template_from_pem(pem_bundle(&certs)).expect("a template");
            assert_eq!(root_count(&template), n);
        }
    }

    #[test]
    fn bundled_roots_are_an_explicit_store_of_exactly_the_crate_certs() {
        let roots = TlsRoots::from_choice(choose_roots_for(Vec::new(), 3, OsStoreAlone));
        assert!(roots.is_bundled());
        assert_eq!(
            (roots.native, roots.bundled, roots.skipped),
            (0, bundled_certs().len(), 3)
        );
        assert!(roots.template.is_some(), "{roots:?}");
        let addr = roots.tls_addr("db.example", 6380);
        assert!(matches!(
            addr,
            ConnectionAddr::TcpTls {
                insecure: false,
                tls_params: Some(_),
                ..
            }
        ));
        assert_eq!(root_count(&addr), bundled_certs().len());
    }

    /// An empty bundle would make "falls back to the bundled roots" pass while trusting
    /// nothing.
    #[test]
    fn the_bundle_is_the_crate_set_and_holds_isrg_root_x1() {
        let certs = bundled_certs();
        assert!(certs.len() > 100, "only {} bundled roots", certs.len());
        assert_eq!(certs, webpki_root_certs::TLS_SERVER_ROOT_CERTS);
        let mut store = RootCertStore::empty();
        for cert in certs {
            store
                .add(cert.clone())
                .expect("a bundled root rustls accepts");
        }
        assert_eq!(store.len(), certs.len());
        let contains = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
        let isrg = store.roots.iter().find(|anchor| {
            let subject = anchor.subject.as_ref();
            contains(subject, b"Internet Security Research Group")
                && contains(subject, b"ISRG Root X1")
        });
        assert!(
            isrg.is_some(),
            "ISRG Root X1 is not among the bundled roots"
        );
    }

    /// redis-rs replaces its default store with the one in `tls_params` in 1.8.1. The
    /// tests above would catch a merge only for the stores they build, so this pins the
    /// version whose code was read.
    #[test]
    fn redis_is_still_the_version_whose_store_handling_was_checked() {
        const CHECKED: &str = "1.8.1";
        let lock = include_str!("../../Cargo.lock");
        let version = lock
            .split("[[package]]")
            .find(|block| block.lines().any(|l| l.trim() == "name = \"redis\""))
            .and_then(|block| {
                block
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("version = \""))
                    .and_then(|v| v.strip_suffix('"'))
            })
            .expect("redis in Cargo.lock");
        assert_eq!(
            version, CHECKED,
            "redis is now {version} in Cargo.lock. Re-check that its connection.rs \
             (`create_rustls_config`) still REPLACES its default trust store with \
             `tls_params.root_cert_store` (`root_cert_store.unwrap_or(root_store)` in \
             1.8.1) rather than merging the two; src/conn/tls.rs depends on it. Then \
             update CHECKED here and the comment on `redis` in Cargo.toml."
        );
    }

    #[test]
    fn the_pem_bundle_round_trips() {
        use rustls::pki_types::pem::PemObject;
        let certs = vec![ca_cert("a"), ca_cert("b")];
        let pem = pem_bundle(&certs);
        let back: Vec<_> = CertificateDer::pem_slice_iter(&pem)
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(back, certs);
        assert!(template_from_pem(pem).is_some());
    }

    #[test]
    fn pem_that_redis_refuses_gives_no_params() {
        let bad = b"-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n".to_vec();
        assert!(template_from_pem(bad).is_none());
    }

    #[test]
    fn the_store_is_loaded_once() {
        let cell = OnceLock::new();
        let loads = AtomicUsize::new(0);
        let load = || {
            loads.fetch_add(1, Ordering::SeqCst);
            TlsRoots::from_choice(choose_roots_for(Vec::new(), 0, OsStoreAlone))
        };
        let first: *const TlsRoots = cached(&cell, load);
        let second: *const TlsRoots = cached(&cell, load);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(std::ptr::eq(first, second));
    }

    #[test]
    fn debug_prints_counts_only() {
        let roots = TlsRoots::from_choice(choose_roots_for(vec![ca_cert("a")], 2, OsStoreAlone));
        assert_eq!(
            format!("{roots:?}"),
            "TlsRoots { native: 1, bundled: 0, skipped: 2 }"
        );
    }

    /// Loads the real OS store, so CI exercises it on Linux, macOS and Windows.
    #[test]
    fn the_process_store_loads_and_is_shared() {
        let first = tls_roots();
        assert!(std::ptr::eq(first, tls_roots()));
        assert!(first.template.is_some(), "{first:?}");
        assert!(first.native + first.bundled > 0, "{first:?}");
        if cfg!(windows) {
            // The OS store and the whole bundle, each certificate once.
            assert!(
                first.native + first.bundled >= bundled_certs().len(),
                "{first:?}"
            );
        } else if first.native > 0 {
            assert_eq!(first.bundled, 0, "{first:?}");
        }
    }
}
