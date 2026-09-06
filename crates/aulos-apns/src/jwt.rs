//! The APNs provider authentication token: an ES256 JWT signed with the `.p8`, cached and
//! reminted every 50 minutes (DESIGN §25.3).
//!
//! Apple accepts a provider token whose `iat` is between 20 and 60 minutes old and rejects
//! anything outside that window with `403 ExpiredProviderToken`; it *also* rate-limits providers
//! that mint a fresh token per request. 50 minutes sits in the middle of the accepted band with
//! ten minutes of slack for a slow clock, which is why it is not 59.
//!
//! There is no `exp` claim: Apple derives expiry from `iat`, and a token carrying an `exp` is
//! rejected outright.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use aulos_core::clock::Clock;
use aulos_core::id::UnixMs;
use jsonwebtoken::{Algorithm, EncodingKey, Header};
use serde::Serialize;

use crate::error::ApnsError;

/// How long a minted token is reused before a fresh one is signed.
///
/// Apple's accepted band is 20–60 minutes from `iat`; this is the middle of it.
pub const REMINT_AFTER: Duration = Duration::from_secs(50 * 60);

/// The two claims Apple wants, and no others.
#[derive(Serialize)]
struct Claims {
    /// The Apple Developer team id.
    iss: Arc<str>,
    /// Issued-at, unix **seconds**.
    iat: i64,
}

/// One cached provider token.
#[derive(Clone, Debug)]
struct Cached {
    bearer: Arc<str>,
    minted_at_ms: UnixMs,
}

/// The signing key plus the token it last produced.
///
/// Cheap to share: [`Self::bearer`] takes `&self` and hands back an `Arc<str>`, so the hot path is
/// one mutex acquisition and one `Arc` clone.
pub struct ProviderToken {
    key: EncodingKey,
    header: Header,
    team_id: Arc<str>,
    clock: Arc<dyn Clock>,
    cached: Mutex<Option<Cached>>,
    minted_total: AtomicU64,
}

impl std::fmt::Debug for ProviderToken {
    /// Prints nothing derived from the signing key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderToken")
            .field("kid", &self.header.kid)
            .field("iss", &self.team_id)
            .field("minted_total", &self.minted_total())
            .finish()
    }
}

impl ProviderToken {
    /// Parses a `.p8` (PKCS#8 PEM) and prepares the ES256 header.
    ///
    /// The key is parsed **once, here**, so a malformed `APNS_KEY_FILE` is reported at boot rather
    /// than on the first completed download.
    ///
    /// # Errors
    /// [`ApnsError::MissingKeyId`], [`ApnsError::MissingTeamId`] or [`ApnsError::KeyFormat`].
    pub fn new(
        pem: &[u8],
        key_id: &str,
        team_id: &str,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, ApnsError> {
        if key_id.trim().is_empty() {
            return Err(ApnsError::MissingKeyId);
        }
        if team_id.trim().is_empty() {
            return Err(ApnsError::MissingTeamId);
        }
        let key = EncodingKey::from_ec_pem(pem)
            .map_err(|e| ApnsError::KeyFormat(e.to_string().into()))?;
        let mut header = Header::new(Algorithm::ES256);
        header.kid = Some(key_id.to_owned());
        Ok(Self {
            key,
            header,
            team_id: Arc::from(team_id),
            clock,
            cached: Mutex::new(None),
            minted_total: AtomicU64::new(0),
        })
    }

    /// The current bearer token, minting one if the cache is empty or older than [`REMINT_AFTER`].
    ///
    /// The whole check-and-mint happens **under one guard**. Dropping the lock to sign would make
    /// this a check-then-act race, and the eight concurrent push tasks of a completion fan-out
    /// that straddles the 50-minute boundary would each mint a token — the exact behaviour Apple
    /// answers with `429 TooManyProviderTokenUpdates`, and the one this cache exists to prevent.
    /// Signing an ES256 JWT is microseconds and the lock is otherwise uncontended, so holding it
    /// across the mint costs nothing worth measuring.
    ///
    /// # Errors
    /// [`ApnsError::Mint`] if signing fails.
    pub fn bearer(&self) -> Result<Arc<str>, ApnsError> {
        let mut cache = self.lock();
        if let Some(bearer) = Self::fresh(&cache, self.clock.now_ms()) {
            return Ok(bearer);
        }
        self.mint_into(&mut cache)
    }

    /// The cached token when it is still inside Apple's accepted band, else `None`.
    fn fresh(cache: &Option<Cached>, now: UnixMs) -> Option<Arc<str>> {
        let ttl_ms = i64::try_from(REMINT_AFTER.as_millis()).unwrap_or(i64::MAX);
        cache
            .as_ref()
            .filter(|c| now >= c.minted_at_ms && now.saturating_sub(c.minted_at_ms) < ttl_ms)
            .map(|c| Arc::clone(&c.bearer))
    }

    /// Signs a fresh token and replaces the cache, whatever its age.
    ///
    /// This is the `403 InvalidProviderToken`/`ExpiredProviderToken` recovery of DESIGN §25.5:
    /// Apple is the authority on whether our token is acceptable, so its verdict beats our timer.
    ///
    /// # Errors
    /// [`ApnsError::Mint`] if signing fails.
    pub fn remint(&self) -> Result<Arc<str>, ApnsError> {
        let mut cache = self.lock();
        self.mint_into(&mut cache)
    }

    /// Remints **only if** `used` is still the cached token.
    ///
    /// The `403` recovery of [`crate::ApnsClient::send`] runs in every push task at once, so a
    /// genuinely stale JWT would otherwise have all eight in-flight tasks sign a replacement
    /// instead of reusing the one the first task just installed. Comparing against the token that
    /// actually got the `403` makes the rotation a compare-and-swap: the loser gets the winner's
    /// token back and retries with it.
    ///
    /// # Errors
    /// [`ApnsError::Mint`] if signing fails.
    pub fn remint_if_current(&self, used: &str) -> Result<Arc<str>, ApnsError> {
        let mut cache = self.lock();
        if let Some(c) = cache.as_ref()
            && &*c.bearer != used
        {
            // Another task already rotated it; that token has not been rejected yet.
            return Ok(Arc::clone(&c.bearer));
        }
        self.mint_into(&mut cache)
    }

    /// Signs a token and installs it. The caller holds the guard, which is what makes the mint
    /// atomic with respect to the freshness check that decided to call it.
    fn mint_into(&self, cache: &mut Option<Cached>) -> Result<Arc<str>, ApnsError> {
        let now_ms = self.clock.now_ms();
        let claims = Claims {
            iss: Arc::clone(&self.team_id),
            iat: now_ms.div_euclid(1_000),
        };
        let jwt = jsonwebtoken::encode(&self.header, &claims, &self.key)
            .map_err(|e| ApnsError::Mint(e.to_string().into()))?;
        let bearer: Arc<str> = Arc::from(jwt);
        *cache = Some(Cached {
            bearer: Arc::clone(&bearer),
            minted_at_ms: now_ms,
        });
        self.minted_total.fetch_add(1, Ordering::Relaxed);
        Ok(bearer)
    }

    /// How many tokens have been signed since boot. A number that climbs once per request means
    /// the cache is broken.
    #[must_use]
    pub fn minted_total(&self) -> u64 {
        self.minted_total.load(Ordering::Relaxed)
    }

    /// The `kid` header this token carries.
    #[must_use]
    pub fn key_id(&self) -> Option<&str> {
        self.header.kid.as_deref()
    }

    /// A poison-tolerant lock: a panic while holding it cannot leave the notifier unable to push.
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Cached>> {
        self.cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
