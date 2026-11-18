use jmodguard_updater::http_client_builder;
use rustls::crypto::{CryptoProvider, ring};

// This is the only test in this executable, so other callers cannot install the provider first.
#[test]
fn first_https_client_uses_ring() {
    assert!(CryptoProvider::get_default().is_none());

    let builder = http_client_builder();
    let provider = CryptoProvider::get_default().expect("builder must install a Rustls provider");
    let ring = ring::default_provider();
    assert!(
        std::ptr::eq(
            provider.signature_verification_algorithms.all,
            ring.signature_verification_algorithms.all,
        ),
        "the installed Rustls provider must be ring"
    );

    builder
        .https_only(true)
        .build()
        .expect("first HTTPS client must build with ring installed");
}
