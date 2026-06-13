//! Noise XX session driver over `snow`.
//!
//! Runs after the QUIC/rustls layer (which pins to the peer Ed25519
//! identity). Noise gives us a second authenticated session — random
//! X25519 statics — so media stream framing can use Noise transport keys
//! independent of the TLS keys.

use snow::{Builder, HandshakeState, TransportState, params::NoiseParams};

const PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";

/// Max size of a single Noise message (per the spec).
pub const NOISE_MAX_MESSAGE: usize = 65535;

#[derive(Debug, thiserror::Error)]
pub enum NoiseError {
    #[error("snow: {0}")]
    Snow(#[from] snow::Error),
    #[error("handshake not complete")]
    Incomplete,
    #[error("message exceeds Noise spec: {0}")]
    Oversized(usize),
}

#[derive(Debug, Clone, Copy)]
pub enum Role {
    Initiator,
    Responder,
}

/// Driver around the Noise XX handshake.
pub struct NoiseXxSession {
    role: Role,
    state: HandshakeState,
}

impl NoiseXxSession {
    fn params() -> NoiseParams {
        PATTERN
            .parse()
            .expect("hard-coded Noise pattern is well-formed")
    }

    /// Generate a fresh local X25519 static for this session and start as
    /// initiator.
    pub fn initiator() -> Result<Self, NoiseError> {
        let builder = Builder::new(Self::params());
        let keypair = builder.generate_keypair()?;
        let state = Builder::new(Self::params())
            .local_private_key(&keypair.private)
            .build_initiator()?;
        Ok(Self { role: Role::Initiator, state })
    }

    /// Generate a fresh local X25519 static and start as responder.
    pub fn responder() -> Result<Self, NoiseError> {
        let builder = Builder::new(Self::params());
        let keypair = builder.generate_keypair()?;
        let state = Builder::new(Self::params())
            .local_private_key(&keypair.private)
            .build_responder()?;
        Ok(Self { role: Role::Responder, state })
    }

    pub fn role(&self) -> Role {
        self.role
    }

    pub fn is_complete(&self) -> bool {
        self.state.is_handshake_finished()
    }

    /// Drive one outbound handshake message. Returns the buffer to send.
    pub fn write_message(&mut self, payload: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        let n = self.state.write_message(payload, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Consume one inbound handshake message. Returns the inner payload.
    pub fn read_message(&mut self, msg: &[u8]) -> Result<Vec<u8>, NoiseError> {
        if msg.len() > NOISE_MAX_MESSAGE {
            return Err(NoiseError::Oversized(msg.len()));
        }
        let mut buf = vec![0u8; NOISE_MAX_MESSAGE];
        let n = self.state.read_message(msg, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    /// Consume the handshake state into a transport-mode cipher.
    pub fn into_transport(self) -> Result<NoiseTransport, NoiseError> {
        if !self.state.is_handshake_finished() {
            return Err(NoiseError::Incomplete);
        }
        Ok(NoiseTransport {
            state: self.state.into_transport_mode()?,
        })
    }
}

/// AEAD wrapper bound to the completed Noise session.
pub struct NoiseTransport {
    state: TransportState,
}

impl NoiseTransport {
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; plaintext.len() + 16];
        let n = self.state.write_message(plaintext, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }

    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>, NoiseError> {
        let mut buf = vec![0u8; ciphertext.len()];
        let n = self.state.read_message(ciphertext, &mut buf)?;
        buf.truncate(n);
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xx_roundtrip_local() {
        let mut initiator = NoiseXxSession::initiator().unwrap();
        let mut responder = NoiseXxSession::responder().unwrap();

        let m1 = initiator.write_message(&[]).unwrap();
        let _ = responder.read_message(&m1).unwrap();
        let m2 = responder.write_message(b"hello-from-responder").unwrap();
        let p2 = initiator.read_message(&m2).unwrap();
        assert_eq!(p2, b"hello-from-responder");
        let m3 = initiator.write_message(b"hello-from-initiator").unwrap();
        let p3 = responder.read_message(&m3).unwrap();
        assert_eq!(p3, b"hello-from-initiator");

        assert!(initiator.is_complete());
        assert!(responder.is_complete());

        let mut ti = initiator.into_transport().unwrap();
        let mut tr = responder.into_transport().unwrap();

        let ct = ti.encrypt(b"ping").unwrap();
        let pt = tr.decrypt(&ct).unwrap();
        assert_eq!(pt, b"ping");

        let ct = tr.encrypt(b"pong").unwrap();
        let pt = ti.decrypt(&ct).unwrap();
        assert_eq!(pt, b"pong");
    }
}
