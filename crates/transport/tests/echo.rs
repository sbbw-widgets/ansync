//! End-to-end localhost test for the ZUDP transport stack.
//!
//! 1. Both peers hold long-term Ed25519 identities.
//! 2. Noise XX handshake is handled internally by zudp.
//! 3. A `Control` stream is opened and a round-trip message is asserted.

use std::sync::Arc;
use std::time::Duration;

use ansync_crypto::{IdentityKeypair, PeerIdentity};
use ansync_transport::{Connection, PeerResolver, Stream, StreamKind, ZudpServer};
use bytes::Bytes;
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

struct SinglePeerResolver(PeerIdentity);

impl PeerResolver for SinglePeerResolver {
    fn resolve(&self, _x25519_key: &[u8; 32]) -> Option<PeerIdentity> {
        Some(self.0.clone())
    }
}

#[tokio::test]
async fn zudp_echo() {
    let server_id = IdentityKeypair::generate();
    let client_id = IdentityKeypair::generate();

    let server_resolver: Arc<dyn PeerResolver> =
        Arc::new(SinglePeerResolver(client_id.public()));
    let client_resolver: Arc<dyn PeerResolver> =
        Arc::new(SinglePeerResolver(server_id.public()));

    let server = ZudpServer::bind(
        "127.0.0.1:0".parse().unwrap(),
        &server_id,
        server_resolver,
    )
    .await
    .expect("bind server");

    let server_addr = server.local_addr();

    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.expect("server accept");
        let (kind, mut stream) = conn.accept().await.expect("server accept stream");
        assert_eq!(kind, StreamKind::Control);

        let frame = stream.recv().await.expect("recv frame");
        stream.send(frame).await.expect("echo frame");
    });

    let client_task = tokio::spawn(async move {
        let server_x25519 = ansync_transport::zudp::ed25519_pubkey_to_x25519(
            &server_id.public().as_bytes(),
        )
        .expect("server pubkey to x25519");

        let conn = ZudpServer::connect(server_addr, server_x25519, &client_id, client_resolver)
            .await
            .expect("client connect");

        let mut stream = conn.open(StreamKind::Control).await.expect("open control");
        let msg = Bytes::from_static(b"hello ansync");
        stream.send(msg.clone()).await.expect("send");

        let echoed = stream.recv().await.expect("recv echo");
        assert_eq!(echoed, msg);

        conn.close("ok").await.expect("close");
    });

    let (s, c) = tokio::join!(
        timeout(TEST_TIMEOUT, server_task),
        timeout(TEST_TIMEOUT, client_task)
    );
    s.expect("server within timeout").expect("server panicked");
    c.expect("client within timeout").expect("client panicked");
}
