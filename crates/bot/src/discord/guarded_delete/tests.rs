use super::*;
use std::{cell::Cell, future::poll_fn, pin::pin, rc::Rc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{
        ServerConfig, crypto::ring::default_provider, pki_types::PrivatePkcs8KeyDer, version::TLS13,
    },
};

struct GatedWire {
    io: DuplexStream,
    writes_left: Rc<Cell<usize>>,
}

impl AsyncRead for GatedWire {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_read(cx, buf)
    }
}

impl AsyncWrite for GatedWire {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let left = self.writes_left.get();
        // The test polls this writer directly before and after opening the gate.
        if left == 0 {
            return Poll::Pending;
        }
        let result = Pin::new(&mut self.io).poll_write(cx, buf);
        if matches!(result, Poll::Ready(Ok(bytes)) if bytes > 0) {
            self.writes_left.set(left - 1);
        }
        result
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(cx)
    }
}

async fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    poll_fn(|cx| Poll::Ready(future.as_mut().poll(cx))).await
}

#[tokio::test(start_paused = true)]
async fn tls_control_record_cannot_start_delete_confirmation() {
    const HEAD: &[u8] = b"DELETE / HTTP/1.1\r\nHost: localhost\r\n\r\n";
    let key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert = key.cert.der().clone();
    let provider = Arc::new(default_provider());
    let mut server = ServerConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&TLS13])
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.clone()],
            PrivatePkcs8KeyDer::from(key.key_pair.serialize_der()).into(),
        )
        .unwrap();
    server.send_tls13_tickets = 0;
    let server = TlsAcceptor::from(Arc::new(server));
    let mut roots = RootCertStore::empty();
    roots.add(cert).unwrap();
    let client = TlsConnector::from(Arc::new(
        ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ));

    for expire in [true, false] {
        let (local, peer) = tokio::io::duplex(4096);
        let writes_left = Rc::new(Cell::new(usize::MAX));
        let (sent, mut handoff) = oneshot::channel();
        let evidence_window = Duration::from_secs(5);
        let deadline = Instant::now() + evidence_window;
        let socket = GuardedSocket {
            socket: GatedWire {
                io: local,
                writes_left: Rc::clone(&writes_left),
            },
            first: Some(FirstWrite {
                deadline,
                admit: &|| true,
                refresh: &|| std::future::ready(true),
                check: None,
                sent,
            }),
            http_enqueued: false,
        };
        let (client, peer) = tokio::join!(
            client.connect(ServerName::try_from("localhost").unwrap(), socket),
            server.accept(peer),
        );
        let mut client = client.unwrap();
        let mut peer = peer.unwrap();
        peer.get_mut().1.refresh_traffic_keys().unwrap();
        peer.flush().await.unwrap();
        // Process a real peer KeyUpdate. Its reply is still queued lazily in
        // Rustls until the next plaintext write.
        let mut scratch = [0];
        assert!(
            poll_once(pin!(client.read(&mut scratch)))
                .await
                .is_pending()
        );
        assert!(!client.get_ref().1.wants_write());

        writes_left.set(1);
        let mut client = GuardedTls(client);
        let control_at = Instant::now();
        {
            let mut write = pin!(async {
                client.write_all(HEAD).await?;
                client.flush().await
            });
            assert!(poll_once(write.as_mut()).await.is_pending());
            assert_eq!(
                writes_left.get(),
                0,
                "the TLS KeyUpdate reply used the only write"
            );
            assert_eq!(handoff.try_recv(), Err(oneshot::error::TryRecvError::Empty));

            let stall = if expire {
                evidence_window
            } else {
                Duration::from_secs(1)
            };
            tokio::time::advance(stall).await;
            writes_left.set(usize::MAX);
            if expire {
                assert!(write.await.is_err());
                assert_eq!(handoff.try_recv(), Err(oneshot::error::TryRecvError::Empty));
            } else {
                write.await.unwrap();
                assert_eq!(handoff.try_recv().unwrap(), control_at + stall);
            }
        }
        drop(client);
        let mut received = Vec::new();
        let _ = peer.read_to_end(&mut received).await;
        assert_eq!(received, if expire { b"" } else { HEAD });
    }
}
