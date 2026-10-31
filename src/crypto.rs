//! UKEY2, Google's key exchange, and the SecureMessage channel Quick Share runs over.
//! Byte-for-byte as NearDrop's PROTOCOL.md describes it.

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, BlockEncryptMut, KeyIvInit};
use hkdf::Hkdf;
use hmac::{Hmac, Mac};
use p256::ecdh::EphemeralSecret;
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::{EncodedPoint, FieldBytes, PublicKey};
use prost::Message;
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256, Sha512};

use crate::error::{protocol, Error, Result};
use crate::proto::securegcm::{
    self, ukey2_client_init::CipherCommitment, ukey2_message, DeviceToDeviceMessage, GcmMetadata, Ukey2ClientFinished,
    Ukey2ClientInit, Ukey2HandshakeCipher, Ukey2Message, Ukey2ServerInit,
};
use crate::proto::securemessage::{
    EcP256PublicKey, EncScheme, GenericPublicKey, Header, HeaderAndBody, PublicKeyType, SecureMessage, SigScheme,
};

const NEXT_PROTOCOL: &str = "AES_256_CBC-HMAC_SHA256";

type Encryptor = cbc::Encryptor<aes::Aes256>;
type Decryptor = cbc::Decryptor<aes::Aes256>;
type HmacSha256 = Hmac<Sha256>;

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0; N];
    OsRng.fill_bytes(&mut b);
    b
}

fn hkdf(ikm: &[u8], salt: &[u8], info: &[u8]) -> [u8; 32] {
    let mut out = [0; 32];
    Hkdf::<Sha256>::new(Some(salt), ikm).expand(info, &mut out).expect("32 bytes is a valid HKDF length");
    out
}

/// Java BigInteger bytes, as Android sends them: big-endian two's complement,
/// so a coordinate with its top bit set gets a leading zero.
fn to_signed(b: &[u8]) -> Vec<u8> {
    if b[0] & 0x80 != 0 { [&[0], b].concat() } else { b.to_vec() }
}

fn from_signed(b: &[u8]) -> Result<FieldBytes> {
    let b = &b[b.len().saturating_sub(32)..];
    let mut out = FieldBytes::default();
    out[32 - b.len()..].copy_from_slice(b);
    Ok(out)
}

fn encode_public_key(pk: &PublicKey) -> Vec<u8> {
    let p = pk.to_encoded_point(false);
    GenericPublicKey {
        r#type: PublicKeyType::EcP256 as i32,
        ec_p256_public_key: Some(EcP256PublicKey {
            x: to_signed(p.x().expect("uncompressed point")),
            y: to_signed(p.y().expect("uncompressed point")),
        }),
        ..Default::default()
    }
    .encode_to_vec()
}

fn decode_public_key(bytes: &[u8]) -> Result<PublicKey> {
    let key = GenericPublicKey::decode(bytes)?.ec_p256_public_key.ok_or(Error::Handshake("no P-256 key"))?;
    let point = EncodedPoint::from_affine_coordinates(&from_signed(&key.x)?, &from_signed(&key.y)?, false);
    Option::from(PublicKey::from_encoded_point(&point)).ok_or(Error::Handshake("peer key is not on the curve"))
}

fn ukey2(kind: ukey2_message::Type, data: Vec<u8>) -> Vec<u8> {
    Ukey2Message { message_type: Some(kind as i32), message_data: Some(data) }.encode_to_vec()
}

fn unwrap_ukey2(raw: &[u8], want: ukey2_message::Type) -> Result<Vec<u8>> {
    let msg = Ukey2Message::decode(raw)?;
    if msg.message_type != Some(want as i32) {
        return Err(Error::Handshake("unexpected UKEY2 message"));
    }
    msg.message_data.ok_or(Error::Handshake("empty UKEY2 message"))
}

/// The 4-digit code both screens show, from Chromium's nearby_sharing_service_impl.
pub fn pin(auth: &[u8]) -> String {
    let (mut hash, mut multiplier) = (0i32, 1i32);
    for &b in auth {
        hash = (hash + (b as i8) as i32 * multiplier) % 9973;
        multiplier = (multiplier * 31) % 9973;
    }
    format!("{:04}", hash.abs())
}

/// Shared secret → (auth string, next-protocol secret).
fn derive(secret: &EphemeralSecret, peer: &PublicKey, client_init: &[u8], server_init: &[u8]) -> ([u8; 32], [u8; 32]) {
    let dhs = Sha256::digest(secret.diffie_hellman(peer).raw_secret_bytes());
    let info = [client_init, server_init].concat();
    (hkdf(&dhs, b"UKEY2 v1 auth", &info), hkdf(&dhs, b"UKEY2 v1 next", &info))
}

/// The receiving side (UKEY2 responder).
pub struct ServerHandshake {
    secret: EphemeralSecret,
    commitment: Vec<u8>,
    client_init: Vec<u8>,
    server_init: Vec<u8>,
}

impl ServerHandshake {
    /// Takes the raw ClientInit; returns the handshake and the raw ServerInit to send.
    pub fn new(client_init_raw: &[u8]) -> Result<(Self, Vec<u8>)> {
        let init = Ukey2ClientInit::decode(&*unwrap_ukey2(client_init_raw, ukey2_message::Type::ClientInit)?)?;
        if init.version != Some(1) || init.random.as_ref().map(Vec::len) != Some(32) {
            return Err(Error::Handshake("unsupported UKEY2 version"));
        }
        if init.next_protocol.as_deref() != Some(NEXT_PROTOCOL) {
            return Err(Error::Handshake("unsupported next protocol"));
        }
        let commitment = init
            .cipher_commitments
            .into_iter()
            .find(|c| c.handshake_cipher == Some(Ukey2HandshakeCipher::P256Sha512 as i32))
            .and_then(|c| c.commitment)
            .ok_or(Error::Handshake("no P-256 cipher offered"))?;

        let secret = EphemeralSecret::random(&mut OsRng);
        let server_init = ukey2(
            ukey2_message::Type::ServerInit,
            Ukey2ServerInit {
                version: Some(1),
                random: Some(random_bytes::<32>().to_vec()),
                handshake_cipher: Some(Ukey2HandshakeCipher::P256Sha512 as i32),
                public_key: Some(encode_public_key(&secret.public_key())),
            }
            .encode_to_vec(),
        );
        let hs = Self { secret, commitment, client_init: client_init_raw.to_vec(), server_init: server_init.clone() };
        Ok((hs, server_init))
    }

    /// Takes the raw ClientFinish; returns the encrypted channel and the PIN.
    pub fn finish(self, client_finish_raw: &[u8]) -> Result<(SecureChannel, String)> {
        if Sha512::digest(client_finish_raw).as_slice() != self.commitment {
            return Err(Error::Handshake("client finish does not match its commitment"));
        }
        let finish = Ukey2ClientFinished::decode(&*unwrap_ukey2(client_finish_raw, ukey2_message::Type::ClientFinish)?)?;
        let peer = decode_public_key(&finish.public_key.ok_or(Error::Handshake("no client key"))?)?;
        let (auth, next) = derive(&self.secret, &peer, &self.client_init, &self.server_init);
        Ok((SecureChannel::new(&next, true), pin(&auth)))
    }
}

/// The sending side (UKEY2 initiator).
pub struct ClientHandshake {
    secret: EphemeralSecret,
    client_init: Vec<u8>,
    client_finish: Vec<u8>,
}

impl ClientHandshake {
    /// Returns the handshake and the raw ClientInit to send. The ClientFinish is built
    /// up front: ClientInit commits to its hash.
    pub fn new() -> (Self, Vec<u8>) {
        let secret = EphemeralSecret::random(&mut OsRng);
        let client_finish = ukey2(
            ukey2_message::Type::ClientFinish,
            Ukey2ClientFinished { public_key: Some(encode_public_key(&secret.public_key())) }.encode_to_vec(),
        );
        let client_init = ukey2(
            ukey2_message::Type::ClientInit,
            Ukey2ClientInit {
                version: Some(1),
                random: Some(random_bytes::<32>().to_vec()),
                cipher_commitments: vec![CipherCommitment {
                    handshake_cipher: Some(Ukey2HandshakeCipher::P256Sha512 as i32),
                    commitment: Some(Sha512::digest(&client_finish).to_vec()),
                }],
                next_protocol: Some(NEXT_PROTOCOL.into()),
            }
            .encode_to_vec(),
        );
        (Self { secret, client_init: client_init.clone(), client_finish }, client_init)
    }

    /// Takes the raw ServerInit; returns the channel, the PIN and the raw ClientFinish to send.
    pub fn finish(self, server_init_raw: &[u8]) -> Result<(SecureChannel, String, Vec<u8>)> {
        let init = Ukey2ServerInit::decode(&*unwrap_ukey2(server_init_raw, ukey2_message::Type::ServerInit)?)?;
        let peer = decode_public_key(&init.public_key.ok_or(Error::Handshake("no server key"))?)?;
        let (auth, next) = derive(&self.secret, &peer, &self.client_init, server_init_raw);
        Ok((SecureChannel::new(&next, false), pin(&auth), self.client_finish))
    }
}

/// AES-256-CBC + HMAC-SHA256 SecureMessages carrying sequence-numbered offline frames.
pub struct SecureChannel {
    encrypt: [u8; 32],
    decrypt: [u8; 32],
    send_hmac: [u8; 32],
    recv_hmac: [u8; 32],
    send_seq: i32,
    recv_seq: i32,
}

impl SecureChannel {
    fn new(next_secret: &[u8; 32], server: bool) -> Self {
        let d2d = Sha256::digest(b"D2D");
        let client = hkdf(next_secret, &d2d, b"client");
        let server_key = hkdf(next_secret, &d2d, b"server");
        let smsg = Sha256::digest(b"SecureMessage");
        let keys = |k: &[u8; 32]| (hkdf(k, &smsg, b"ENC:2"), hkdf(k, &smsg, b"SIG:1"));
        let ((client_enc, client_sig), (server_enc, server_sig)) = (keys(&client), keys(&server_key));
        let (encrypt, send_hmac, decrypt, recv_hmac) = if server {
            (server_enc, server_sig, client_enc, client_sig)
        } else {
            (client_enc, client_sig, server_enc, server_sig)
        };
        Self { encrypt, decrypt, send_hmac, recv_hmac, send_seq: 0, recv_seq: 0 }
    }

    /// Wraps a serialized offline frame into a serialized SecureMessage.
    pub fn seal(&mut self, frame: &[u8]) -> Vec<u8> {
        self.send_seq += 1;
        let d2d = DeviceToDeviceMessage { message: Some(frame.to_vec()), sequence_number: Some(self.send_seq) };
        let iv = random_bytes::<16>();
        let body = Encryptor::new(&self.encrypt.into(), &iv.into()).encrypt_padded_vec_mut::<Pkcs7>(&d2d.encode_to_vec());
        let metadata = GcmMetadata { r#type: securegcm::Type::DeviceToDeviceMessage as i32, version: Some(1) };
        let header_and_body = HeaderAndBody {
            header: Header {
                signature_scheme: SigScheme::HmacSha256 as i32,
                encryption_scheme: EncScheme::Aes256Cbc as i32,
                iv: Some(iv.to_vec()),
                public_metadata: Some(metadata.encode_to_vec()),
                ..Default::default()
            },
            body,
        }
        .encode_to_vec();
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.send_hmac).expect("any key length");
        mac.update(&header_and_body);
        SecureMessage { header_and_body, signature: mac.finalize().into_bytes().to_vec() }.encode_to_vec()
    }

    /// Verifies and decrypts a serialized SecureMessage back into the offline frame inside.
    pub fn open(&mut self, raw: &[u8]) -> Result<Vec<u8>> {
        let msg = SecureMessage::decode(raw)?;
        let mut mac = <HmacSha256 as Mac>::new_from_slice(&self.recv_hmac).expect("any key length");
        mac.update(&msg.header_and_body);
        mac.verify_slice(&msg.signature).map_err(|_| protocol("bad message signature"))?;
        let hb = HeaderAndBody::decode(&*msg.header_and_body)?;
        let iv: [u8; 16] = hb.header.iv.as_deref().and_then(|v| v.try_into().ok()).ok_or_else(|| protocol("bad IV"))?;
        let plain = Decryptor::new(&self.decrypt.into(), &iv.into())
            .decrypt_padded_vec_mut::<Pkcs7>(&hb.body)
            .map_err(|_| protocol("bad padding"))?;
        let d2d = DeviceToDeviceMessage::decode(&*plain)?;
        self.recv_seq += 1;
        if d2d.sequence_number != Some(self.recv_seq) {
            return Err(protocol(format!("sequence {:?}, expected {}", d2d.sequence_number, self.recv_seq)));
        }
        d2d.message.ok_or_else(|| protocol("empty message"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_agree() {
        let (client, client_init) = ClientHandshake::new();
        let (server, server_init) = ServerHandshake::new(&client_init).unwrap();
        let (mut c, client_pin, client_finish) = client.finish(&server_init).unwrap();
        let (mut s, server_pin) = server.finish(&client_finish).unwrap();
        assert_eq!(client_pin, server_pin);
        assert_eq!(client_pin.len(), 4);

        assert_eq!(s.open(&c.seal(b"hola")).unwrap(), b"hola");
        assert_eq!(c.open(&s.seal(b"que onda")).unwrap(), b"que onda");
        assert_eq!(s.open(&c.seal(b"2")).unwrap(), b"2");
    }

    #[test]
    fn tampering_and_replay_fail() {
        let (client, client_init) = ClientHandshake::new();
        let (server, server_init) = ServerHandshake::new(&client_init).unwrap();
        let (mut c, _, client_finish) = client.finish(&server_init).unwrap();
        let (mut s, _) = server.finish(&client_finish).unwrap();
        let sealed = c.seal(b"hola");
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(s.open(&bad).is_err());
        s.open(&sealed).unwrap();
        assert!(s.open(&sealed).is_err(), "replayed sequence number");
    }

    #[test]
    fn rejects_a_finish_that_breaks_the_commitment() {
        let (_, client_init) = ClientHandshake::new();
        let (other, _) = ClientHandshake::new();
        let (server, server_init) = ServerHandshake::new(&client_init).unwrap();
        let (_, _, wrong_finish) = other.finish(&server_init).unwrap();
        assert!(server.finish(&wrong_finish).is_err());
    }

    #[test]
    fn signed_coordinates() {
        assert_eq!(to_signed(&[0x80, 1]), [0, 0x80, 1]);
        assert_eq!(to_signed(&[0x7f, 1]), [0x7f, 1]);
        let mut x = [0u8; 33];
        x[1] = 0x90;
        assert_eq!(from_signed(&x).unwrap()[0], 0x90);
        assert_eq!(from_signed(&[5]).unwrap()[31], 5);
    }

    #[test]
    fn pin_is_four_digits_and_stable() {
        assert_eq!(pin(&[0; 32]), "0000");
        assert_eq!(pin(&[1, 2, 3]), "2946"); // 1 + 2·31 + 3·31²
        assert_eq!(pin(&[0xff]), "0001");
    }
}
