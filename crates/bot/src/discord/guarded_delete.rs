//! Each guarded DELETE owns one HTTP/1.1 connection. Only the first outgoing
//! request bytes end evidence admission and start response settlement.

use super::{DISCORD_REST_TIMEOUT, RouteQuota, before_dispatch_deadline};
use http_body_util::Empty;
use hyper_util::{client::legacy::connect::HttpConnector, rt::TokioIo};
use hyper1::{Request, Uri, body::Bytes, client::conn::http1};
use serenity::http::{ErrorResponse, HttpError};
use std::{
    future::Future,
    io::{self, Write as _},
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll, ready},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::oneshot,
    time::Instant,
};
use tokio_rustls::{
    TlsConnector,
    client::TlsStream,
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
    deadline: Instant,
    admit: &'a Admit,
    refresh: &'a Refresh,
    check: Option<Pin<Box<Check>>>,
    sent: oneshot::Sender<Instant>,
}

struct GuardedSocket<'a, Socket, Admit, Refresh, Check> {
    socket: Socket,
    first: Option<FirstWrite<'a, Admit, Refresh, Check>>,
    http_enqueued: bool,
}

impl<S: AsyncRead + Unpin, A, R, F> AsyncRead for GuardedSocket<'_, S, A, R, F> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().socket).poll_read(cx, buf)
    }
}

impl<S, A, R, F> AsyncWrite for GuardedSocket<'_, S, A, R, F>
where
    S: AsyncWrite + Unpin,
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
            .filter(|_| this.http_enqueued && !buf.is_empty())
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

struct GuardedTls<S>(TlsStream<S>);

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for GuardedTls<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_read(cx, buf)
    }
}

impl<S, A, R, F> AsyncWrite for GuardedTls<GuardedSocket<'_, S, A, R, F>>
where
    S: AsyncRead + AsyncWrite + Unpin,
    A: Fn() -> bool,
    R: Fn() -> F,
    F: Future<Output = bool>,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let stream = &mut self.get_mut().0;
        if buf.is_empty() || stream.get_ref().0.http_enqueued {
            return Pin::new(stream).poll_write(cx, buf);
        }

        // An empty plaintext write promotes Rustls' pending KeyUpdate reply
        // to its output queue. Drain all control output before adding any
        // part of the HTTP head. This is a fresh one-request connection with
        // no early data, so the first fragment uses fresh application keys.
        // There is no read or await between the drain and queuing that
        // fragment. Any later control output follows it in the TLS queue.
        let _ = stream.get_mut().1.writer().write(&[])?;
        ready!(Pin::new(&mut *stream).poll_flush(cx))?;
        let (socket, tls) = stream.get_mut();
        let accepted = tls.writer().write(&buf[..buf.len().min(16 * 1024)])?;
        if accepted == 0 {
            return Poll::Ready(Err(io::Error::from(io::ErrorKind::WriteZero)));
        }
        socket.http_enqueued = true;

        // Rustls accepting plaintext does not mean TCP accepted ciphertext.
        // The raw guard stays armed until it writes the application flight.
        match Pin::new(stream).poll_flush(cx) {
            Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
            _ => Poll::Ready(Ok(accepted)),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().0).poll_shutdown(cx)
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

pub(super) async fn send<A, R, F>(
    uri: &Uri,
    outbound: EmptyRequest,
    deadline: Instant,
    admit: &A,
    refresh: &R,
    quota: RouteQuota<'_>,
) -> Result<DeleteReceipt, SendFailure>
where
    A: Fn() -> bool + Sync,
    R: Fn() -> F + Sync,
    F: Future<Output = bool> + Send,
{
    if !admit() {
        return Err(SendFailure::Unsent);
    }
    let (sent, mut handoff) = oneshot::channel();
    let connected = before_dispatch_deadline(deadline, async {
        let mut connector = HttpConnector::new();
        connector.enforce_http(false);
        connector.set_nodelay(true);
        let socket = connector.call(uri.clone()).await.ok()?.into_inner();
        let https = uri.scheme_str() == Some("https");
        let socket = GuardedSocket {
            socket,
            first: Some(FirstWrite {
                deadline,
                admit,
                refresh,
                check: None,
                sent,
            }),
            http_enqueued: !https,
        };
        let io: Box<dyn Connection + '_> = if https {
            let name =
                ServerName::try_from(uri.host()?.trim_matches(['[', ']']).to_owned()).ok()?;
            Box::new(GuardedTls(
                tls_connector().connect(name, socket).await.ok()?,
            ))
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
        let Some(held_quota) = before_dispatch_deadline(deadline, quota.acquire()).await else {
            return Err(SendFailure::Unsent);
        };
        if !matches!(
            before_dispatch_deadline(deadline, sender.ready()).await,
            Some(Ok(()))
        ) {
            return Err(SendFailure::Unsent);
        }
        let pending = sender.send_request(outbound);
        tokio::pin!(pending);
        let sent_at = tokio::select! {
            biased;
            time = &mut handoff => time.map_err(|_| SendFailure::Unsent)?,
            _ = tokio::time::sleep_until(deadline) => return Err(SendFailure::Unsent),
            _ = &mut pending => return Err(SendFailure::Unsent),
        };
        quota.charge(held_quota).await;
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

#[cfg(test)]
mod tests;
