//! Trust roots for TLS connections.
//!
//! Server certificates are checked against the OS certificate store (macOS Keychain,
//! Windows certificate store, the CA bundle on Linux; `SSL_CERT_FILE` / `SSL_CERT_DIR`
//! replace it when set). Certificates or files that fail to load are skipped rather
//! than failing the connection. When the OS store yields no usable certificate at all
//! (a slim container without `ca-certificates`, say), the roots bundled with redis-rs
//! (webpki-roots) are used instead, never in addition.
//!
//! The store is loaded once per process and shared by every connection, the replica
//! candidates included. Nothing here turns certificate or host name checks off.

use std::sync::OnceLock;

use base64::Engine as _;
use redis::{ConnectionAddr, TlsCertificates};
use rustls::RootCertStore;
use rustls::pki_types::CertificateDer;

/// Which trust roots the OS store produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Roots {
    /// Certificates from the OS store that rustls accepts as trust anchors.
    Native(Vec<CertificateDer<'static>>),
    /// The OS store had no usable certificate: use the bundled webpki roots.
    Bundled,
}

/// The decision made from what the OS store returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RootChoice {
    pub(crate) roots: Roots,
    /// Certificates or files that failed to load or were not usable, and were skipped.
    pub(crate) skipped: usize,
}

/// Decide the trust roots from what loading the OS store returned: the certificates
/// that loaded, and how many certificates or files failed to.
///
/// Every certificate rustls refuses as a trust anchor is skipped and counted with the
/// load errors. With at least one usable certificate the OS store is used on its own;
/// with none, the bundled roots.
pub(crate) fn choose_roots(certs: Vec<CertificateDer<'static>>, load_errors: usize) -> RootChoice {
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
        Roots::Native(usable)
    };
    RootChoice { roots, skipped }
}

/// The trust roots every TLS connection uses, ready to hand to redis-rs.
pub(crate) struct TlsRoots {
    /// With the OS roots, a TLS address whose `tls_params` carry them (redis-rs does not
    /// export the parameter type, so the address is kept as a template). `None` leaves
    /// redis-rs on its default store, which with the `tls-rustls-webpki-roots` feature is
    /// the bundled webpki roots (and never loads the OS store itself).
    template: Option<ConnectionAddr>,
    /// How many OS certificates are trusted; 0 when the bundled roots are used.
    native: usize,
    /// Certificates or files that were skipped. Kept for diagnostics only: nothing prints
    /// it on connect.
    skipped: usize,
}

impl std::fmt::Debug for TlsRoots {
    /// Counts only; the certificates themselves are not printed.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TlsRoots")
            .field("native", &self.native)
            .field("bundled", &self.is_bundled())
            .field("skipped", &self.skipped)
            .finish()
    }
}

impl TlsRoots {
    pub(crate) fn from_choice(choice: RootChoice) -> Self {
        let RootChoice { roots, skipped } = choice;
        match roots {
            Roots::Native(certs) => match template_from_pem(pem_bundle(&certs)) {
                Some(template) => Self {
                    template: Some(template),
                    native: certs.len(),
                    skipped,
                },
                // Not expected: every certificate already passed the same check redis-rs
                // makes. Should it happen, the bundled roots are still a verified store.
                None => Self {
                    template: None,
                    native: 0,
                    skipped: skipped + certs.len(),
                },
            },
            Roots::Bundled => Self {
                template: None,
                native: 0,
                skipped,
            },
        }
    }

    /// Whether the bundled roots are used because the OS store had none.
    pub(crate) fn is_bundled(&self) -> bool {
        self.template.is_none()
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

/// Load the OS store and decide, as [`choose_roots`] does.
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

    #[test]
    fn a_non_empty_os_store_is_used_on_its_own() {
        let certs = vec![ca_cert("a"), ca_cert("b")];
        let choice = choose_roots(certs.clone(), 0);
        assert_eq!(
            choice,
            RootChoice {
                roots: Roots::Native(certs),
                skipped: 0
            }
        );
    }

    #[test]
    fn an_empty_os_store_falls_back_to_the_bundled_roots() {
        assert_eq!(
            choose_roots(Vec::new(), 0),
            RootChoice {
                roots: Roots::Bundled,
                skipped: 0
            }
        );
    }

    #[test]
    fn partial_load_errors_are_skipped() {
        let good = ca_cert("good");
        // Two files failed to load and one certificate is not usable.
        let choice = choose_roots(vec![garbage(), good.clone()], 2);
        assert_eq!(
            choice,
            RootChoice {
                roots: Roots::Native(vec![good]),
                skipped: 3
            }
        );
    }

    #[test]
    fn when_every_certificate_fails_the_bundled_roots_are_used() {
        let choice = choose_roots(vec![garbage(), garbage()], 4);
        assert_eq!(
            choice,
            RootChoice {
                roots: Roots::Bundled,
                skipped: 6
            }
        );
    }

    #[test]
    fn native_roots_become_tls_params_with_a_root_store() {
        let roots = TlsRoots::from_choice(choose_roots(vec![ca_cert("a"), ca_cert("b")], 1));
        assert!(!roots.is_bundled());
        assert_eq!((roots.native, roots.skipped), (2, 1));
        let ConnectionAddr::TcpTls {
            insecure,
            tls_params: Some(params),
            ..
        } = roots.tls_addr("db.example", 6380)
        else {
            panic!("expected TLS params");
        };
        assert!(!insecure);
        // `TlsConnParams` keeps its fields private; its `Debug` shows the custom store.
        assert!(
            format!("{params:?}").contains("root_cert_store: Some("),
            "{params:?}"
        );
    }

    #[test]
    fn bundled_roots_leave_redis_on_its_default_store() {
        let roots = TlsRoots::from_choice(choose_roots(Vec::new(), 3));
        assert!(roots.is_bundled());
        assert_eq!((roots.native, roots.skipped), (0, 3));
        assert!(matches!(
            roots.tls_addr("db.example", 6380),
            ConnectionAddr::TcpTls {
                insecure: false,
                tls_params: None,
                ..
            }
        ));
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
            TlsRoots::from_choice(choose_roots(Vec::new(), 0))
        };
        let first: *const TlsRoots = cached(&cell, load);
        let second: *const TlsRoots = cached(&cell, load);
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert!(std::ptr::eq(first, second));
    }

    /// Loads the real OS store, so CI exercises it on Linux, macOS and Windows.
    #[test]
    fn the_process_store_loads_and_is_shared() {
        let first = tls_roots();
        assert!(std::ptr::eq(first, tls_roots()));
        assert!(first.is_bundled() || first.native > 0, "{first:?}");
    }
}
