//! This crate provides a [`rustls::crypto::CryptoProvider`] that includes
//! a hybrid[^1], post-quantum-secure[^2] key exchange algorithm --
//! specifically [X25519MLKEM768].
//!
//! X25519MLKEM768 is pre-standardization, so you should treat
//! this as experimental.  You may see unexpected interop failures, and
//! the algorithm implemented here may not be the one that eventually
//! becomes widely deployed.
//!
//! However, the two components of this key exchange are well regarded:
//! X25519 alone is already used by default by rustls, and tends to have
//! higher quality implementations than other elliptic curves.
//! ML-KEM-768 was standardized by NIST in [FIPS203].
//!
//! [^1]: meaning: a construction that runs a classical and post-quantum
//!       key exchange, and uses the output of both together.  This is a hedge
//!       against the post-quantum half being broken.
//!
//! [^2]: a "post-quantum-secure" algorithm is one posited to be invulnerable
//!       to attack using a cryptographically-relevant quantum computer.  In contrast,
//!       classical algorithms would be broken by such a computer.  Note that such computers
//!       do not currently exist, and may never exist, but current traffic could be captured
//!       now and attacked later.
//!
//! [X25519MLKEM768]: <https://datatracker.ietf.org/doc/draft-kwiatkowski-tls-ecdhe-mlkem/>
//! [FIPS203]: <https://nvlpubs.nist.gov/nistpubs/FIPS/NIST.FIPS.203.pdf>
//!
//! # How to use this crate
//!
//! There are a few options:
//!
//! **To use this as the rustls default provider**: include this code early in your program:
//!
//! ```rust
//! rustls_post_quantum::provider().install_default().unwrap();
//! ```
//!
//! **To incorporate just the key exchange algorithm in a custom [`rustls::crypto::CryptoProvider`]**:
//!
//! ```rust
//! use rustls::crypto::{aws_lc_rs, CryptoProvider};
//! let parent = aws_lc_rs::default_provider();
//! let my_provider = CryptoProvider {
//!     kx_groups: vec![
//!         &rustls_post_quantum::X25519MLKEM768,
//!         aws_lc_rs::kx_group::X25519,
//!     ],
//!     ..parent
//! };
//! ```
//!

use aws_lc_rs::kem;
use aws_lc_rs::unstable::kem::ML_KEM_768;
use rustls::crypto::aws_lc_rs::{default_provider, kx_group};
use rustls::crypto::{
    ActiveKeyExchange, CompletedKeyExchange, CryptoProvider, SharedSecret, SupportedKxGroup,
};
use rustls::ffdhe_groups::FfdheGroup;
use rustls::{Error, NamedGroup, PeerMisbehaved, ProtocolVersion};

/// A `CryptoProvider` which includes `X25519MLKEM768` key exchange.
pub fn provider() -> CryptoProvider {
    let mut parent = default_provider();
    parent
        .kx_groups
        .insert(0, &X25519MLKEM768);
    parent
}

/// This is the [X25519MLKEM768] key exchange.
///
/// [X25519MLKEM768]: <https://datatracker.ietf.org/doc/draft-kwiatkowski-tls-ecdhe-mlkem/>
#[derive(Debug)]
pub struct X25519MLKEM768;

impl SupportedKxGroup for X25519MLKEM768 {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        let x25519 = kx_group::X25519.start()?;

        let ml_kem = kem::DecapsulationKey::generate(&ML_KEM_768)
            .map_err(|_| Error::FailedToGetRandomBytes)?;

        let ml_kem_pub = ml_kem
            .encapsulation_key()
            .map_err(|_| Error::FailedToGetRandomBytes)?;

        let mut combined_pub_key = Vec::with_capacity(COMBINED_PUBKEY_LEN);
        combined_pub_key.extend_from_slice(ml_kem_pub.key_bytes().unwrap().as_ref());
        combined_pub_key.extend_from_slice(x25519.pub_key());

        Ok(Box::new(Active {
            x25519,
            decap_key: Box::new(ml_kem),
            combined_pub_key,
        }))
    }

    fn start_and_complete(&self, client_share: &[u8]) -> Result<CompletedKeyExchange, Error> {
        let Some(share) = ReceivedShare::new(client_share) else {
            return Err(INVALID_KEY_SHARE);
        };

        let x25519 = kx_group::X25519.start_and_complete(share.x25519)?;

        let (ml_kem_share, ml_kem_secret) = kem::EncapsulationKey::new(&ML_KEM_768, share.ml_kem)
            .map_err(|_| INVALID_KEY_SHARE)
            .and_then(|pk| {
                pk.encapsulate()
                    .map_err(|_| INVALID_KEY_SHARE)
            })?;

        let combined_secret = CombinedSecret::combine(x25519.secret, ml_kem_secret);
        let combined_share = CombinedShare::combine(&x25519.pub_key, ml_kem_share);

        Ok(CompletedKeyExchange {
            group: self.name(),
            pub_key: combined_share.0,
            secret: SharedSecret::from(&combined_secret.0[..]),
        })
    }

    fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
        None
    }

    fn name(&self) -> NamedGroup {
        NAMED_GROUP
    }

    fn usable_for_version(&self, version: ProtocolVersion) -> bool {
        version == ProtocolVersion::TLSv1_3
    }
}

struct Active {
    x25519: Box<dyn ActiveKeyExchange>,
    decap_key: Box<kem::DecapsulationKey<kem::AlgorithmId>>,
    combined_pub_key: Vec<u8>,
}

impl ActiveKeyExchange for Active {
    fn complete(self: Box<Self>, peer_pub_key: &[u8]) -> Result<SharedSecret, Error> {
        let Some(ciphertext) = ReceivedCiphertext::new(peer_pub_key) else {
            return Err(INVALID_KEY_SHARE);
        };

        let combined = CombinedSecret::combine(
            self.x25519
                .complete(ciphertext.x25519)?,
            self.decap_key
                .decapsulate(ciphertext.ml_kem.into())
                .map_err(|_| INVALID_KEY_SHARE)?,
        );
        Ok(SharedSecret::from(&combined.0[..]))
    }

    fn pub_key(&self) -> &[u8] {
        &self.combined_pub_key
    }

    fn ffdhe_group(&self) -> Option<FfdheGroup<'static>> {
        None
    }

    fn group(&self) -> NamedGroup {
        NAMED_GROUP
    }
}

struct ReceivedShare<'a> {
    ml_kem: &'a [u8],
    x25519: &'a [u8],
}

impl<'a> ReceivedShare<'a> {
    fn new(buf: &'a [u8]) -> Option<ReceivedShare<'a>> {
        if buf.len() != COMBINED_PUBKEY_LEN {
            return None;
        }

        let (ml_kem, x25519) = buf.split_at(MLKEM768_ENCAP_LEN);
        Some(ReceivedShare { ml_kem, x25519 })
    }
}

struct ReceivedCiphertext<'a> {
    ml_kem: &'a [u8],
    x25519: &'a [u8],
}

impl<'a> ReceivedCiphertext<'a> {
    fn new(buf: &'a [u8]) -> Option<ReceivedCiphertext<'a>> {
        if buf.len() != COMBINED_CIPHERTEXT_LEN {
            return None;
        }

        let (ml_kem, x25519) = buf.split_at(MLKEM768_CIPHERTEXT_LEN);
        Some(ReceivedCiphertext { ml_kem, x25519 })
    }
}

struct CombinedSecret([u8; COMBINED_SHARED_SECRET_LEN]);

impl CombinedSecret {
    fn combine(x25519: SharedSecret, ml_kem: kem::SharedSecret) -> Self {
        let mut out = CombinedSecret([0u8; COMBINED_SHARED_SECRET_LEN]);
        out.0[..MLKEM768_SECRET_LEN].copy_from_slice(ml_kem.as_ref());
        out.0[MLKEM768_SECRET_LEN..].copy_from_slice(x25519.secret_bytes());
        out
    }
}

struct CombinedShare(Vec<u8>);

impl CombinedShare {
    fn combine(x25519: &[u8], ml_kem: kem::Ciphertext<'_>) -> Self {
        let mut out = CombinedShare(vec![0u8; COMBINED_CIPHERTEXT_LEN]);
        out.0[..MLKEM768_CIPHERTEXT_LEN].copy_from_slice(ml_kem.as_ref());
        out.0[MLKEM768_CIPHERTEXT_LEN..].copy_from_slice(x25519);
        out
    }
}

const NAMED_GROUP: NamedGroup = NamedGroup::Unknown(0x11ec);

const INVALID_KEY_SHARE: Error = Error::PeerMisbehaved(PeerMisbehaved::InvalidKeyShare);

const X25519_LEN: usize = 32;
const MLKEM768_CIPHERTEXT_LEN: usize = 1088;
const MLKEM768_ENCAP_LEN: usize = 1184;
const MLKEM768_SECRET_LEN: usize = 32;
const COMBINED_PUBKEY_LEN: usize = MLKEM768_ENCAP_LEN + X25519_LEN;
const COMBINED_CIPHERTEXT_LEN: usize = MLKEM768_CIPHERTEXT_LEN + X25519_LEN;
const COMBINED_SHARED_SECRET_LEN: usize = MLKEM768_SECRET_LEN + X25519_LEN;
