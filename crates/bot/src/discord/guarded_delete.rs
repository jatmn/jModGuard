//! Each guarded DELETE owns one HTTP/1.1 connection. The socket distinguishes
//! connection setup from the first request bytes and from response settlement.

use super::{DISCORD_REST_TIMEOUT, before_dispatch_deadline};
use http_body_util::Empty;
use hyper_util::{client::legacy::connect::HttpConnector, rt::TokioIo};
use hyper1::{Request, Uri, body::Bytes, client::conn::http1};
use serenity::http::{ErrorResponse, HttpError};
use std::{
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::TcpStream,
    sync::oneshot,
    time::Instant,
};
use tokio_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};
use tower_service::Service;

type EmptyRequest = Request<Empty<Bytes>>;

pub(super) enum SendFailure {
    Unsent,
    Uncertain,
}

pub(super) struct DeleteReceipt {
    pub headers: reqwest::Response,
    pub result: serenity::Result<()>,
    pub response_deadline: Instant,
}

struct FirstWrite<'a, Admit, Refresh, Check> {
    armed: &'a AtomicBool,
    deadline: Instant,
    admit: &'a Admit,
    refresh: &'a Refresh,
    check: Option<Pin<Box<Check>>>,
    sent: oneshot::Sender<Instant>,
}

struct GuardedSocket<'a, Admit, Refresh, Check> {
    socket: TcpStream,
    first: Option<FirstWrite<'a, Admit, Refresh, Check>>,
}

impl<A, R, F> AsyncRead for GuardedSocket<'_, A, R, F> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_read(cx, buf)
    }
}

impl<A, R, F> AsyncWrite for GuardedSocket<'_, A, R, F>
where
    A: Fn() -> bool,
    R: Fn() -> F,
    F: Future<Output = bool>,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let Some(first) = this
            .first
            .as_mut()
            .filter(|first| first.armed.load(Ordering::Relaxed) && !buf.is_empty())
        else {
            return Pin::new(&mut this.socket).poll_write(cx, buf);
        };
        let denied = || io::Error::other("Discord DELETE no longer admitted");
        if !(first.admit)() || Instant::now() >= first.deadline {
            return Poll::Ready(Err(denied()));
        }

        // This read runs after connection and rate admission. If the socket
        // still needs to wait, refresh again before the next write attempt.
        let check = first
            .check
            .get_or_insert_with(|| Box::pin((first.refresh)()));
        match check.as_mut().poll(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(false) => return Poll::Ready(Err(denied())),
            Poll::Ready(true) => first.check = None,
        }
        if !(first.admit)() || Instant::now() >= first.deadline {
            return Poll::Ready(Err(denied()));
        }
        let result = Pin::new(&mut this.socket).poll_write(cx, buf);
        if matches!(result, Poll::Ready(Ok(bytes)) if bytes > 0) {
            let first = this.first.take().expect("first write");
            let _ = first.sent.send(Instant::now());
        }
        result
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_shutdown(cx)
    }
}

trait Connection: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T: AsyncRead + AsyncWrite + Send + Unpin> Connection for T {}

fn tls_connector() -> &'static TlsConnector {
    static CONNECTOR: OnceLock<TlsConnector> = OnceLock::new();
    CONNECTOR.get_or_init(|| {
        // Match the bot's reqwest client trust roots. The direct transport
        // speaks HTTP/1.1 and never enables TLS early data.
        let mut config = ClientConfig::builder_with_provider(
            tokio_rustls::rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .expect("default TLS versions")
        .with_root_certificates(RootCertStore::from_iter(
            webpki_roots::TLS_SERVER_ROOTS.iter().cloned(),
        ))
        .with_no_client_auth();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        TlsConnector::from(Arc::new(config))
    })
}

pub(super) async fn send<A, R, F, Q, G>(
    uri: &Uri,
    outbound: EmptyRequest,
    deadline: Instant,
    admit: &A,
    refresh: &R,
    quota: Q,
) -> Result<DeleteReceipt, SendFailure>
where
    A: Fn() -> bool + Sync,
    R: Fn() -> F + Sync,
    F: Future<Output = bool> + Send,
    Q: Future<Output = G>,
{
    if !admit() {
        return Err(SendFailure::Unsent);
    }
    let armed = AtomicBool::new(false);
    let (sent, mut handoff) = oneshot::channel();
    let connected = before_dispatch_deadline(deadline, async {
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);
        connector.set_nodelay(true);
        let socket = connector.call(uri.clone()).await.ok()?.into_inner();
        let socket = GuardedSocket {
            socket,
            first: Some(FirstWrite {
                armed: &armed,
                deadline,
                admit,
                refresh,
                check: None,
                sent,
            }),
        };
        let io: Box<dyn Connection + '_> = if uri.scheme_str() == Some("https") {
            let name =
                ServerName::try_from(uri.host()?.trim_matches(['[', ']']).to_owned()).ok()?;
            Box::new(tls_connector().connect(name, socket).await.ok()?)
        } else {
            Box::new(socket)
        };
        #[cfg(test)]
        if let Ok(hook) = AFTER_DELETE_CONNECT.try_with(Arc::clone) {
            hook().await;
        }
        http1::handshake(TokioIo::new(io)).await.ok()
    })
    .await
    .flatten();
    let Some((mut sender, driver)) = connected else {
        return Err(SendFailure::Unsent);
    };

    let response = async {
        let Some(held_quota) = before_dispatch_deadline(deadline, quota).await else {
            return Err(SendFailure::Unsent);
        };
        if !matches!(
            before_dispatch_deadline(deadline, sender.ready()).await,
            Some(Ok(()))
        ) {
            return Err(SendFailure::Unsent);
        }
        armed.store(true, Ordering::Relaxed);
        let pending = sender.send_request(outbound);
        tokio::pin!(pending);
        let sent_at = tokio::select! {
            biased;
            time = &mut handoff => time.map_err(|_| SendFailure::Unsent)?,
            _ = tokio::time::sleep_until(deadline) => return Err(SendFailure::Unsent),
            _ = &mut pending => return Err(SendFailure::Unsent),
        };
        drop(held_quota);
        jmodguard::resources::count("discord_rest", 1);

        let response_deadline = sent_at + DISCORD_REST_TIMEOUT;
        tokio::time::timeout_at(response_deadline, async {
            let response = pending.await.map_err(|_| SendFailure::Uncertain)?;
            let (parts, body) = response.into_parts();
            let headers = reqwest::Response::from(hyper1::Response::from_parts(parts.clone(), ""));
            let result = if parts.status.is_success() {
                Ok(())
            } else {
                let response = reqwest::Response::from(hyper1::Response::from_parts(
                    parts,
                    reqwest::Body::wrap(body),
                ));
                let error = ErrorResponse::from_response(response, reqwest::Method::DELETE).await;
                Err(HttpError::UnsuccessfulRequest(error).into())
            };
            Ok(DeleteReceipt {
                headers,
                result,
                response_deadline,
            })
        })
        .await
        .map_err(|_| SendFailure::Uncertain)?
    };
    tokio::pin!(response);
    tokio::select! {
        biased;
        result = &mut response => result,
        _ = driver => response.await,
    }
}

#[cfg(test)]
type ConnectionHook = dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync;

#[cfg(test)]
tokio::task_local! {
    static AFTER_DELETE_CONNECT: Arc<ConnectionHook>;
}

#[cfg(test)]
pub(crate) async fn with_delete_connect_hook<F: Future>(
    hook: Arc<ConnectionHook>,
    work: F,
) -> F::Output {
    AFTER_DELETE_CONNECT.scope(hook, work).await
}
