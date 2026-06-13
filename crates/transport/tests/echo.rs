//! End-to-end localhost test for the Step 2 transport stack:
//!
//! 1. Both peers hold long-term Ed25519 identities.
//! 2. QUIC handshake — each side's rustls verifier pins the peer's pubkey.
//! 3. A bidirectional `Control` stream is opened.
//! 4. Noise XX runs over the control stream to establish independent
//!    session keys.
//! 5. A `Hello` envelope is sent under Noise transport encryption and
//!    echoed back; the test asserts the round-trip survives.

use std::time::Duration;

use ansync_core::{Capabilities, DeviceName};
use ansync_crypto::{IdentityKeypair, NoiseXxSession};
use ansync_proto::{Envelope, Hello, Message, PROTOCOL_VERSION};
use ansync_transport::{Connection, QuicTransport, Stream, StreamKind};
use bytes::Bytes;
use tokio::time::timeout;

const TEST_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn quic_ed25519_pinning_noise_echo() {
    let server_identity = IdentityKeypair::generate();
    let client_identity = IdentityKeypair::generate();

    let server_pub = server_identity.public().as_bytes();
    let client_pub = client_identity.public().as_bytes();

    let server_transport = QuicTransport::new(server_identity.clone());
    let client_transport = QuicTransport::new(client_identity.clone());

    let server = server_transport
        .bind("127.0.0.1:0".parse().unwrap(), client_pub)
        .expect("bind server");
    let server_addr = server.local_addr().expect("local addr");

    let server_task = tokio::spawn(async move {
        let conn = server.accept().await.expect("server accept");
        let (kind, mut stream) = conn.accept().await.expect("server accept stream");
        assert_eq!(kind, StreamKind::Control);

        let mut session = NoiseXxSession::responder().expect("responder");
        // XX message 1: <- e
        let m1 = read_noise(&mut stream).await;
        session.read_message(&m1).expect("read m1");
        // XX message 2: -> e, ee, s, es
        let m2 = session.write_message(&[]).expect("write m2");
        send_noise(&mut stream, &m2).await;
        // XX message 3: <- s, se
        let m3 = read_noise(&mut stream).await;
        session.read_message(&m3).expect("read m3");
        assert!(session.is_complete(), "responder handshake done");

        let mut transport = session.into_transport().expect("into transport");
        // Decrypt incoming hello.
        let frame = stream.recv().await.expect("recv hello frame");
        let plaintext = transport.decrypt(&frame).expect("decrypt hello");
        let envelope: Envelope = postcard::from_bytes(&plaintext).expect("decode envelope");
        assert_eq!(envelope.version, PROTOCOL_VERSION);
        match envelope.message {
            Message::Hello(ref hello) => {
                assert_eq!(hello.name.0, "client-test");
            }
            ref other => panic!("expected Hello, got {other:?}"),
        }
        // Echo it back encrypted.
        let echoed = transport.encrypt(&plaintext).expect("encrypt echo");
        stream.send(Bytes::from(echoed)).await.expect("send echo");
        stream.finish().await.expect("finish stream");
        // Wait for the client to tear down the connection before exiting
        // so the in-flight bytes are flushed.
        let _ = timeout(TEST_TIMEOUT, conn.closed()).await;
    });

    let client_task = tokio::spawn(async move {
        let conn = client_transport
            .connect(server_addr, server_pub)
            .await
            .expect("client connect");
        let mut stream = conn.open(StreamKind::Control).await.expect("open control");

        let mut session = NoiseXxSession::initiator().expect("initiator");
        // XX message 1: -> e
        let m1 = session.write_message(&[]).expect("write m1");
        send_noise(&mut stream, &m1).await;
        // XX message 2: <- e, ee, s, es
        let m2 = read_noise(&mut stream).await;
        session.read_message(&m2).expect("read m2");
        // XX message 3: -> s, se
        let m3 = session.write_message(&[]).expect("write m3");
        send_noise(&mut stream, &m3).await;
        assert!(session.is_complete(), "initiator handshake done");

        let mut transport = session.into_transport().expect("into transport");

        let hello = Envelope {
            version: PROTOCOL_VERSION,
            message: Message::Hello(Hello {
                device_id: client_identity.device_id(),
                name: DeviceName("client-test".into()),
                capabilities: Capabilities::SCREEN_MIRROR | Capabilities::FILES,
            }),
        };
        let plaintext = postcard::to_allocvec(&hello).expect("encode envelope");
        let ciphertext = transport.encrypt(&plaintext).expect("encrypt");
        stream
            .send(Bytes::from(ciphertext))
            .await
            .expect("send hello");

        let echoed_ct = stream.recv().await.expect("recv echo");
        let echoed_pt = transport.decrypt(&echoed_ct).expect("decrypt echo");
        assert_eq!(echoed_pt, plaintext);

        conn.close("ok").await.expect("client close");
    });

    let (s, c) = tokio::join!(timeout(TEST_TIMEOUT, server_task), timeout(TEST_TIMEOUT, client_task));
    s.expect("server task within timeout").expect("server task panicked");
    c.expect("client task within timeout").expect("client task panicked");
}

async fn send_noise(stream: &mut ansync_transport::QuicStream, msg: &[u8]) {
    stream
        .send(Bytes::copy_from_slice(msg))
        .await
        .expect("send noise frame");
}

async fn read_noise(stream: &mut ansync_transport::QuicStream) -> Vec<u8> {
    stream
        .recv()
        .await
        .map(|b| b.to_vec())
        .expect("recv noise frame")
}

