//! The APNs provider authentication token (DESIGN §25.3).
//!
//! Every assertion here goes through a *verifier*: the token is decoded with the public half of
//! the fixture key pair, so the test proves the signature is a real ES256 signature over the
//! header and claims Apple is going to read, rather than proving that this crate's encoder agrees
//! with itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use aulos_apns::{ApnsError, ProviderToken, REMINT_AFTER};
use aulos_core::clock::{Clock, FakeClock};
use common::{TEST_KEY_ID, TEST_KEY_P8, TEST_KEY_PUB, TEST_TEAM_ID, clock};
use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode, decode_header};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Claims {
    iss: String,
    iat: i64,
}

fn verify(token: &str) -> (jsonwebtoken::Header, Claims) {
    let header = decode_header(token).expect("the header must parse");
    let key = DecodingKey::from_ec_pem(TEST_KEY_PUB.as_bytes()).expect("public key");
    let mut validation = Validation::new(Algorithm::ES256);
    // Apple's provider token has no `exp` and no `aud`; asking for them would test the validator.
    validation.validate_exp = false;
    validation.required_spec_claims.clear();
    let data = decode::<Claims>(token, &key, &validation).expect("the signature must verify");
    (header, data.claims)
}

fn token(clock: Arc<FakeClock>) -> ProviderToken {
    let clock: Arc<dyn Clock> = clock;
    ProviderToken::new(TEST_KEY_P8.as_bytes(), TEST_KEY_ID, TEST_TEAM_ID, clock)
        .expect("the fixture key must load")
}

#[test]
fn the_header_is_es256_with_the_key_id_and_the_claims_are_iss_and_iat() {
    let c = clock();
    let now_ms = c.now_ms();
    let minted = token(Arc::clone(&c)).bearer().expect("mint");

    let (header, claims) = verify(&minted);
    assert_eq!(header.alg, Algorithm::ES256);
    assert_eq!(header.kid.as_deref(), Some(TEST_KEY_ID));
    assert_eq!(claims.iss, TEST_TEAM_ID);
    assert_eq!(claims.iat, now_ms / 1_000, "iat is unix SECONDS");
}

#[test]
fn there_is_no_exp_claim() {
    // Apple derives expiry from `iat`; a token carrying `exp` is rejected outright.
    let minted = token(clock()).bearer().expect("mint");
    let payload = minted.split('.').nth(1).expect("payload segment");
    let decoded = base64_url(payload);
    assert!(!decoded.contains("\"exp\""), "{decoded}");
    assert!(!decoded.contains("\"aud\""), "{decoded}");
}

/// Minimal base64url decode, so the test does not need a base64 dependency of its own.
fn base64_url(segment: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut bits = 0u32;
    let mut nbits = 0u32;
    let mut out = Vec::new();
    for ch in segment.bytes() {
        let Some(v) = ALPHABET.iter().position(|c| *c == ch) else {
            continue;
        };
        bits = (bits << 6) | v as u32;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((bits >> nbits) as u8);
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn the_token_is_cached_until_fifty_minutes_have_passed() {
    let c = clock();
    let t = token(Arc::clone(&c));

    let first = t.bearer().expect("mint");
    c.advance(REMINT_AFTER - Duration::from_secs(1));
    let still_cached = t.bearer().expect("cached");
    assert_eq!(first, still_cached);
    assert_eq!(t.minted_total(), 1, "one signature, not one per request");

    c.advance(Duration::from_secs(2));
    let fresh = t.bearer().expect("remint");
    assert_ne!(first, fresh);
    assert_eq!(t.minted_total(), 2);
    assert_eq!(verify(&fresh).1.iat, c.now_ms() / 1_000);
}

#[test]
fn remint_replaces_the_cache_whatever_its_age() {
    // This is the `403 ExpiredProviderToken` recovery: Apple beats our own timer.
    let c = clock();
    let t = token(Arc::clone(&c));
    let first = t.bearer().expect("mint");
    c.advance(Duration::from_secs(1));
    let forced = t.remint().expect("remint");
    assert_ne!(first, forced);
    assert_eq!(t.bearer().expect("cached"), forced);
    assert_eq!(t.minted_total(), 2);
}

#[test]
fn a_blank_identifier_or_a_bad_key_is_reported_at_construction() {
    let c: Arc<dyn Clock> = clock();
    assert!(matches!(
        ProviderToken::new(TEST_KEY_P8.as_bytes(), "  ", TEST_TEAM_ID, Arc::clone(&c)),
        Err(ApnsError::MissingKeyId)
    ));
    assert!(matches!(
        ProviderToken::new(TEST_KEY_P8.as_bytes(), TEST_KEY_ID, "", Arc::clone(&c)),
        Err(ApnsError::MissingTeamId)
    ));
    assert!(matches!(
        ProviderToken::new(
            b"-----BEGIN PRIVATE KEY-----\nnope\n",
            TEST_KEY_ID,
            TEST_TEAM_ID,
            c
        ),
        Err(ApnsError::KeyFormat(_))
    ));
}

#[test]
fn the_debug_rendering_never_shows_the_key() {
    let t = token(clock());
    let rendered = format!("{t:?}");
    assert!(rendered.contains(TEST_KEY_ID));
    assert!(!rendered.contains("PRIVATE KEY"));
    assert!(!rendered.contains(&TEST_KEY_P8[40..80]));
}
