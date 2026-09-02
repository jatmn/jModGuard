//! rustls 0.23 HTTPS connector for libsql's hyper 0.14 remote client.
//!
//! libsql 0.9.30's `tls` feature pulls `hyper-rustls 0.25` → `rustls 0.22` →
//! `rustls-webpki 0.102.8` (GHSA-82j2-j2ch-gfr8). Building the connector here
//! lets that feature stay off while remote HTTPS still uses rustls.

use anyhow::{Context as _, Result, bail};
use hyper::Uri;
use hyper::client::HttpConnector;
use hyper::client::connect::{Connected, Connection};
use hyper::service::Service;
use rustls::ClientConfig;
use rustls_pki_types::ServerName;
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::{Arc, OnceLock};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

/// Hyper connector used for every remote libSQL URL, including `http://`.
///
/// TLS client config is loaded on the first `https://` connect. libsql rewrites
/// `libsql://` to `https://` inside the client, so open-time scheme checks cannot
/// decide whether roots are needed; HTTP remotes must not fail just because the
/// process has no native CA store.
pub(crate) fn https_connector() -> HttpsConnector<HttpConnector> {
    let mut http = HttpConnector::new();
    http.enforce_http(false);
    http.set_nodelay(true);
    HttpsConnector {
        http,
        tls: Arc::new(OnceLock::new()),
    }
}

fn tls_connector(slot: &OnceLock<TlsConnector>) -> Result<TlsConnector> {
    if let Some(existing) = slot.get() {
        return Ok(existing.clone());
    }
    let built = TlsConnector::from(Arc::new(client_config()?));
    Ok(slot.get_or_init(|| built).clone())
}

fn client_config() -> Result<ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    if roots.is_empty() {
        let detail = if native.errors.is_empty() {
            "no valid native root CA certificates found".to_string()
        } else {
            format!(
                "no valid native root CA certificates found ({})",
                native
                    .errors
                    .iter()
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            )
        };
        bail!("{detail}");
    }

    let mut config =
        ClientConfig::builder_with_provider(rustls::crypto::ring::default_provider().into())
            .with_safe_default_protocol_versions()
            .context("invalid rustls protocol versions")?
            .with_root_certificates(roots)
            .with_no_client_auth();
    // hyper is compiled with HTTP/1.1 only; advertise that ALPN so HTTPS
    // libSQL endpoints do not negotiate HTTP/2.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}

fn server_name_from_uri_host(host: &str) -> Result<ServerName<'static>, io::Error> {
    let host = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(ServerName::from(ip));
    }
    ServerName::try_from(host)
        .map(|name| name.to_owned())
        .map_err(|_| io::Error::other("invalid dnsname"))
}

#[derive(Clone)]
pub(crate) struct HttpsConnector<T> {
    http: T,
    tls: Arc<OnceLock<TlsConnector>>,
}

pub(crate) enum MaybeHttpsStream<T> {
    Http(T),
    Https(Box<TlsStream<T>>),
}

impl<T> Connection for MaybeHttpsStream<T>
where
    T: AsyncRead + AsyncWrite + Connection + Unpin,
{
    fn connected(&self) -> Connected {
        match self {
            Self::Http(stream) => stream.connected(),
            Self::Https(stream) => {
                let (tcp, tls) = stream.get_ref();
                if tls.alpn_protocol() == Some(b"h2") {
                    tcp.connected().negotiated_h2()
                } else {
                    tcp.connected()
                }
            }
        }
    }
}

impl<T> AsyncRead for MaybeHttpsStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Http(stream) => Pin::new(stream).poll_read(cx, buf),
            Self::Https(stream) => Pin::new(stream.as_mut()).poll_read(cx, buf),
        }
    }
}

impl<T> AsyncWrite for MaybeHttpsStream<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            Self::Http(stream) => Pin::new(stream).poll_write(cx, buf),
            Self::Https(stream) => Pin::new(stream.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Http(stream) => Pin::new(stream).poll_flush(cx),
            Self::Https(stream) => Pin::new(stream.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            Self::Http(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Https(stream) => Pin::new(stream.as_mut()).poll_shutdown(cx),
        }
    }
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

impl<T> Service<Uri> for HttpsConnector<T>
where
    T: Service<Uri>,
    T::Response: Connection + AsyncRead + AsyncWrite + Send + Unpin + 'static,
    T::Future: Send + 'static,
    T::Error: Into<BoxError>,
{
    type Response = MaybeHttpsStream<T::Response>;
    type Error = BoxError;
    type Future =
        Pin<Box<dyn Future<Output = std::result::Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        self.http.poll_ready(cx).map_err(Into::into)
    }

    fn call(&mut self, dst: Uri) -> Self::Future {
        match dst.scheme_str() {
            Some("http") => {
                let future = self.http.call(dst);
                return Box::pin(async move {
                    Ok(MaybeHttpsStream::Http(future.await.map_err(Into::into)?))
                });
            }
            Some("https") => {}
            Some(scheme) => {
                let message = format!("unsupported scheme {scheme}");
                return Box::pin(async move { Err(io::Error::other(message).into()) });
            }
            None => {
                return Box::pin(async move { Err(io::Error::other("missing scheme").into()) });
            }
        }

        let hostname = match server_name_from_uri_host(dst.host().unwrap_or_default()) {
            Ok(name) => name,
            Err(error) => {
                return Box::pin(async move { Err(error.into()) });
            }
        };

        let connecting = self.http.call(dst);
        let tls_slot = Arc::clone(&self.tls);
        Box::pin(async move {
            let tcp = connecting.await.map_err(Into::into)?;
            let tls = tls_connector(tls_slot.as_ref()).map_err(io::Error::other)?;
            let tls_stream = tls.connect(hostname, tcp).await.map_err(io::Error::other)?;
            Ok(MaybeHttpsStream::Https(Box::new(tls_stream)))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_host_ipv4_literal_is_an_ip_server_name() {
        let name = server_name_from_uri_host("127.0.0.1").expect("ipv4");
        assert_eq!(
            name,
            ServerName::from("127.0.0.1".parse::<IpAddr>().unwrap())
        );
    }

    #[test]
    fn uri_host_bracketed_ipv6_literal_is_an_ip_server_name() {
        let name = server_name_from_uri_host("[::1]").expect("ipv6");
        assert_eq!(name, ServerName::from("::1".parse::<IpAddr>().unwrap()));
    }

    #[test]
    fn uri_host_dns_name_is_preserved() {
        let name = server_name_from_uri_host("example.lite.bunnydb.net").expect("dns");
        assert!(matches!(name, ServerName::DnsName(_)));
    }

    #[test]
    fn uri_host_empty_is_invalid() {
        let error = server_name_from_uri_host("").expect_err("empty host");
        assert_eq!(error.to_string(), "invalid dnsname");
    }

    #[test]
    fn https_client_config_advertises_http11_alpn() {
        let config = client_config().expect("native roots");
        assert_eq!(config.alpn_protocols, [b"http/1.1".to_vec()]);
    }

    #[test]
    fn constructing_the_connector_does_not_load_native_roots() {
        let connector = https_connector();
        assert!(
            connector.tls.get().is_none(),
            "HTTP remotes must not require a CA store at open"
        );
    }

    #[test]
    fn http_connect_does_not_load_native_roots() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let _ = listener.accept();
        });
        let mut connector = https_connector();
        let uri: Uri = format!("http://{address}/").parse().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _ = runtime.block_on(hyper::service::Service::call(&mut connector, uri));
        assert!(
            connector.tls.get().is_none(),
            "plain HTTP must not load native CA roots"
        );
        drop(server);
    }
}
