// SPDX-License-Identifier: Apache-2.0
//! Use ring-backed RS256 rather than the vulnerable RustCrypto RSA operation.
use reqsign_core::{Error, Result};

fn key(pem: &[u8]) -> Result<jsonwebtoken::EncodingKey> {
    jsonwebtoken::EncodingKey::from_rsa_pem(pem)
        .map_err(|e| Error::credential_invalid("invalid RSA signing key").with_source(e))
}

pub(crate) fn sign_rs256(input: &[u8], pem: &[u8]) -> Result<String> {
    jsonwebtoken::crypto::sign(input, &key(pem)?, jsonwebtoken::Algorithm::RS256)
        .map_err(|e| Error::credential_invalid("RS256 signing failed").with_source(e))
}

pub(crate) fn encode_rs256<C: serde::Serialize>(claims: &C, pem: &[u8]) -> Result<String> {
    jsonwebtoken::encode(
        &jsonwebtoken::Header::new(jsonwebtoken::Algorithm::RS256),
        claims,
        &key(pem)?,
    )
    .map_err(|e| Error::credential_invalid("RS256 JWT signing failed").with_source(e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};

    #[test]
    fn signatures_and_jwts_verify_with_public_key() {
        let private = rsa::RsaPrivateKey::new(&mut rsa::rand_core::OsRng, 2048).unwrap();
        let pem = private.to_pkcs8_pem(LineEnding::LF).unwrap();
        let public = private
            .to_public_key()
            .to_public_key_pem(LineEnding::LF)
            .unwrap();
        let decoding = jsonwebtoken::DecodingKey::from_rsa_pem(public.as_bytes()).unwrap();
        let message = b"canonical Google request";
        let signature = sign_rs256(message, pem.as_bytes()).unwrap();
        assert!(
            jsonwebtoken::crypto::verify(
                &signature,
                message,
                &decoding,
                jsonwebtoken::Algorithm::RS256
            )
            .unwrap()
        );
        assert!(
            !jsonwebtoken::crypto::verify(
                &signature,
                b"tampered",
                &decoding,
                jsonwebtoken::Algorithm::RS256
            )
            .unwrap()
        );
        let claims = serde_json::json!({"iss":"fixture@example.invalid", "exp":4102444800u64});
        let jwt = encode_rs256(&claims, pem.as_bytes()).unwrap();
        let decoded = jsonwebtoken::decode::<serde_json::Value>(
            &jwt,
            &decoding,
            &jsonwebtoken::Validation::new(jsonwebtoken::Algorithm::RS256),
        )
        .unwrap();
        assert_eq!(decoded.claims, claims);
        assert!(sign_rs256(message, b"invalid private key").is_err());
    }
}
