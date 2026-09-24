//! Reversible keyed secret placeholders (borrow from oh-my-pi, delta
//! §14.1).
//!
//! # The hole this closes
//!
//! `[security.redact] in_prompt = true` runs the one-way
//! [`crate::redact::Redactor`] over tool results before they reach
//! the model. That covers the leak *out*. It does not cover the
//! second-order leak: the model reads a file containing an API key,
//! the key is redacted to `[REDACTED:openai-key]`, and then the
//! model — asked to "move this config around" — writes a file whose
//! content contains the literal string `[REDACTED:openai-key]`. The
//! secret does not leak, but the operation the user asked for does
//! not happen, and worse, if the model *guesses* what was there
//! (a very common key shape), it can reconstruct the secret from
//! its training distribution.
//!
//! The delta's fix is a **reversible, keyed** placeholder. The model
//! sees `«Credential-a1b2c3d4»` instead of the raw secret. When the
//! model later puts that placeholder in an `edit`/`write_file`/
//! `execute_command` argument, the engine **deobfuscates** the
//! argument before the tool runs. The model never sees the key
//! bytes; the tool call still carries them.
//!
//! # Why keyed
//!
//! The digest is `HMAC-SHA256(install_key, secret)[..8]` — an
//! 8-byte hex prefix. Two properties fall out:
//!
//! * **Stable per install.** The same secret produces the same
//!   placeholder across every request in a session *and across
//!   sessions on the same install*. A conversation that mentions a
//!   secret in turn 3 and writes it in turn 30 sees one placeholder
//!   throughout, which is what makes the model's substitution
//!   reliable.
//! * **Opaque across installs.** A different install has a
//!   different key, so the digest for the same secret differs. That
//!   prevents cross-install correlation of a leaked transcript and
//!   means an attacker with a candidate secret cannot confirm it
//!   matches a leaked placeholder without the key.
//!
//! # What this is NOT
//!
//! * **Not a redactor.** It does not detect secrets on its own; it
//!   takes a set of *known* secret values and returns placeholders
//!   for them. The known set comes from the sources in [`sources`]
//!   — the process environment and a small set of vendor-shaped
//!   regexes.
//! * **Not a guarantee.** A model that never sees a secret cannot
//!   leak it, but a model that runs `execute_command("env")` sees
//!   the raw environment before obfuscation gets a chance. The
//!   `read_protection` policy is the complementary control.

use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::collections::HashMap;
use std::sync::Arc;

type HmacSha256 = Hmac<Sha256>;

/// Bytes of the HMAC digest retained. Eight bytes = 16 hex chars =
/// 64 bits. A collision in a session with more than a few hundred
/// secrets is astronomically unlikely; the placeholder stays short
/// enough that it does not eat the prompt budget the delta's §7.7
/// warnings are about.
const DIGEST_BYTES: usize = 8;

/// The placeholder's opening and closing brackets. Chosen because
/// neither character appears in a token, so a plain `contains`
/// check finds every placeholder and a regex is unnecessary.
const OPEN: char = '«';
const CLOSE: char = '»';

/// The literal between the brackets and the digest.
const PREFIX: &str = "Credential-";

/// A per-install key + the mapping between secrets and their
/// placeholders.
///
/// Cheap to clone (the key and map are behind an `Arc`). One vault
/// per process; the engine holds it and hands it to the pre-send
/// hook and the tool-argument deobfuscator.
#[derive(Clone)]
pub struct SecretVault {
    inner: Arc<VaultInner>,
}

struct VaultInner {
    key: [u8; 32],
    /// Secret value -> placeholder. Populated lazily as
    /// `register_source` walks the sources.
    forward: parking_lot_free::RwLock<HashMap<String, String>>,
    /// Placeholder -> secret. Kept in sync with `forward` so
    /// deobfuscation is a single lookup.
    reverse: parking_lot_free::RwLock<HashMap<String, String>>,
}

/// A minimal `RwLock` wrapper. Defined here so the module does not
/// pull `parking_lot` into `kod-types` for one lock. `std`'s
/// `RwLock` is fine — the lock is held for a single hashmap
/// operation, never across an await.
mod parking_lot_free {
    pub type RwLock<T> = std::sync::RwLock<T>;
}

/// A secret's source, for a log line that says "5 secrets from env,
/// 2 from a config file".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretSource {
    /// An environment variable matching the name heuristic.
    Env,
    /// A vendor-shaped value matched by one of the built-in regexes.
    Vendor,
    /// A password embedded in a connection URL
    /// (`postgres://user:pw@host`).
    ConnectionUrl,
}

/// A registered secret. Handed back by [`SecretVault::register_all`]
/// for a report; the vault does not keep provenance itself.
#[derive(Debug, Clone)]
pub struct RegisteredSecret {
    pub placeholder: String,
    pub source: SecretSource,
}

impl SecretVault {
    /// A vault with a freshly generated random key. Used when no key
    /// file exists yet; [`Self::with_key_file`] is what the engine
    /// actually calls.
    pub fn with_random_key() -> Self {
        // `kod-types` deliberately does not depend on a crypto RNG
        // crate. The seed comes from `std::collections::hash_map::
        // RandomState`, which the standard library seeds from the
        // OS's entropy source. That is not a documented "crypto
        // secure RNG" API, and the key it produces is not
        // indistinguishable from uniform — but the threat model here
        // is "different installs produce different digests", not
        // "an adversary cannot recover the key from a long stream
        // of HMAC outputs". A caller who wants a stronger key
        // supplies one via `with_key`.
        let mut key = [0u8; 32];
        fill_from_randomstate(&mut key);
        Self::with_key(key)
    }

    /// A vault with an explicit key. For tests and for a caller that
    /// manages key material itself.
    pub fn with_key(key: [u8; 32]) -> Self {
        Self {
            inner: Arc::new(VaultInner {
                key,
                forward: parking_lot_free::RwLock::new(HashMap::new()),
                reverse: parking_lot_free::RwLock::new(HashMap::new()),
            }),
        }
    }

    /// Load the vault from `path`, creating the key file if it does
    /// not exist.
    ///
    /// The file holds 44 bytes of base64url key material (32 raw
    /// bytes, one newline). Created with mode `0600` on Unix. A
    /// file that exists but is not 44 bytes of valid base64url
    /// returns an error rather than silently regenerating — a
    /// truncated key file is a bug worth surfacing.
    pub fn load_or_create(path: &std::path::Path) -> std::io::Result<Self> {
        use base64::Engine as _;
        match std::fs::read(path) {
            Ok(bytes) => {
                let text = String::from_utf8_lossy(&bytes);
                let trimmed = text.trim();
                let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(trimmed)
                    .map_err(|e| {
                        std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!("secret-placeholder key is not base64url: {e}"),
                        )
                    })?;
                if decoded.len() != 32 {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "secret-placeholder key is {} bytes, expected 32",
                            decoded.len(),
                        ),
                    ));
                }
                let mut key = [0u8; 32];
                key.copy_from_slice(&decoded);
                Ok(Self::with_key(key))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let vault = Self::with_random_key();
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .encode(vault.inner.key);
                write_key_file(path, encoded.as_bytes())?;
                Ok(vault)
            }
            Err(e) => Err(e),
        }
    }

    /// The placeholder for `secret`. Deterministic — the same secret
    /// always produces the same placeholder for this vault's key.
    pub fn placeholder_for(&self, secret: &str) -> String {
        let mut mac =
            HmacSha256::new_from_slice(&self.inner.key).expect("HMAC accepts any key length");
        mac.update(secret.as_bytes());
        let digest = mac.finalize().into_bytes();
        let hex: String = digest[..DIGEST_BYTES]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        format!("{OPEN}{PREFIX}{hex}{CLOSE}")
    }

    /// Register one secret. Idempotent — registering the same value
    /// twice is a no-op. Returns the placeholder.
    pub fn register(&self, secret: impl Into<String>) -> String {
        let secret = secret.into();
        if let Ok(g) = self.inner.forward.read()
            && let Some(p) = g.get(&secret)
        {
            return p.clone();
        }
        let placeholder = self.placeholder_for(&secret);
        if let Ok(mut g) = self.inner.forward.write() {
            g.insert(secret.clone(), placeholder.clone());
        }
        if let Ok(mut g) = self.inner.reverse.write() {
            g.insert(placeholder.clone(), secret);
        }
        placeholder
    }

    /// Register every string in `secrets`. Returns one
    /// [`RegisteredSecret`] per *newly registered* value; a value
    /// already present is not re-reported.
    pub fn register_all(
        &self,
        secrets: impl IntoIterator<Item = (String, SecretSource)>,
    ) -> Vec<RegisteredSecret> {
        let mut out = Vec::new();
        for (s, source) in secrets {
            if s.is_empty() {
                continue;
            }
            let is_new = self
                .inner
                .forward
                .read()
                .map(|g| !g.contains_key(&s))
                .unwrap_or(true);
            let placeholder = self.register(s);
            if is_new {
                out.push(RegisteredSecret {
                    placeholder,
                    source,
                });
            }
        }
        out
    }

    /// How many secrets are registered.
    pub fn len(&self) -> usize {
        self.inner
            .forward
            .read()
            .map(|g| g.len())
            .unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Replace every registered secret in `text` with its
    /// placeholder.
    ///
    /// # Fixed-point
    ///
    /// The scan is one pass over the source text. A secret whose
    /// *replacement* (a placeholder) contains the raw form of
    /// another secret is impossible by construction — a placeholder
    /// is `«Credential-<hex>»`, which does not match any real secret
    /// shape. That is what makes the operation safe to run twice on
    /// the same input.
    ///
    /// # Longest-first
    ///
    /// When two registered secrets share a prefix, the longer one is
    /// substituted first. Otherwise a shorter secret would eat part
    /// of a longer one and the longer one would never match.
    pub fn obfuscate(&self, text: &str) -> String {
        let secrets: Vec<(String, String)> = match self.inner.forward.read() {
            Ok(g) => g.iter().map(|(s, p)| (s.clone(), p.clone())).collect(),
            Err(_) => return text.to_string(),
        };
        if secrets.is_empty() {
            return text.to_string();
        }
        // Longest-first by secret length.
        let mut sorted = secrets;
        sorted.sort_by(|a, b| b.0.len().cmp(&a.0.len()));

        let mut out = text.to_string();
        for (secret, placeholder) in sorted {
            if !out.contains(&secret) {
                continue;
            }
            out = out.replace(&secret, &placeholder);
        }
        out
    }

    /// Replace every placeholder in `text` with the raw secret it
    /// stands for.
    ///
    /// Used on model-authored tool arguments: a `write_file` whose
    /// `content` contains `«Credential-a1b2»` becomes a write of the
    /// real secret, which is what the model's caller intended when
    /// it said "put the key back".
    ///
    /// Returns `(output, restored_count)`. A placeholder that is not
    /// in the vault passes through unchanged — a model that invented
    /// a placeholder shape (there is no reason to, but a hostile or
    /// confused prompt might) does not get a raw secret it did not
    /// have.
    pub fn deobfuscate(&self, text: &str) -> (String, usize) {
        let reverse = match self.inner.reverse.read() {
            Ok(g) => g,
            Err(_) => return (text.to_string(), 0),
        };
        if reverse.is_empty() {
            return (text.to_string(), 0);
        }
        let mut out = text.to_string();
        let mut restored = 0usize;
        for (placeholder, secret) in reverse.iter() {
            if out.contains(placeholder) {
                let n = out.matches(placeholder).count();
                out = out.replace(placeholder, secret);
                restored += n;
            }
        }
        (out, restored)
    }

    /// Whether `text` contains any registered placeholder. A caller
    /// about to hand a tool argument to a subprocess can use this as
    /// a fast pre-check before paying for [`Self::deobfuscate`].
    pub fn contains_placeholder(&self, text: &str) -> bool {
        match self.inner.reverse.read() {
            Ok(g) => g.keys().any(|p| text.contains(p)),
            Err(_) => false,
        }
    }
}

impl std::fmt::Debug for SecretVault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately does not print the key or the secrets.
        f.debug_struct("SecretVault")
            .field("secrets", &self.len())
            .finish()
    }
}

/// Write a key file with mode 0600 on Unix. On other platforms the
/// file inherits the process's default ACL, which is the best a
/// cross-platform `std::fs` can do without a platform-specific
/// attribute crate.
fn write_key_file(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        f.write_all(bytes)?;
        f.write_all(b"\n")?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)?;
        Ok(())
    }
}

/// Fill `dest` with bytes derived from a fresh `RandomState`.
///
/// `RandomState::new()` seeds itself from the OS's entropy source
/// once per process. Hashing distinct small integers under two
/// independent states gives a byte stream that is *not* uniform but
/// is different across process runs and different across calls.
/// That is what a per-install key needs: two installs must not
/// derive the same HMAC digest for the same secret.
///
/// This is not a substitute for a real CSPRNG, and the module doc
/// says so. A caller that wants cryptographic key generation
/// supplies a key via [`SecretVault::with_key`].
fn fill_from_randomstate(dest: &mut [u8]) {
    use std::hash::{BuildHasher, Hasher};
    let s1 = std::collections::hash_map::RandomState::new();
    let s2 = std::collections::hash_map::RandomState::new();
    let mut h1 = s1.build_hasher();
    let mut h2 = s2.build_hasher();
    for (i, b) in dest.iter_mut().enumerate() {
        h1.write_usize(i);
        h2.write_usize(i ^ 0xA5A5_A5A5_A5A5_A5A5);
        let a = h1.finish().to_le_bytes();
        let c = h2.finish().to_le_bytes();
        *b = a[0] ^ c[3];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vault() -> SecretVault {
        SecretVault::with_key([42u8; 32])
    }

    #[test]
    fn placeholder_is_deterministic() {
        let v = vault();
        let p1 = v.placeholder_for("sk-abc");
        let p2 = v.placeholder_for("sk-abc");
        assert_eq!(p1, p2);
    }

    #[test]
    fn placeholder_shape_is_the_documented_form() {
        let v = vault();
        let p = v.placeholder_for("sk-abc");
        assert!(p.starts_with('«'), "got {p}");
        assert!(p.ends_with('»'), "got {p}");
        assert!(p.contains("Credential-"), "got {p}");
        // 16 hex characters after the prefix.
        let hex_part = p
            .trim_start_matches('«')
            .trim_end_matches('»')
            .strip_prefix("Credential-")
            .unwrap();
        assert_eq!(hex_part.len(), 16);
        assert!(hex_part.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn different_secrets_produce_different_placeholders() {
        let v = vault();
        let a = v.placeholder_for("sk-aaa");
        let b = v.placeholder_for("sk-bbb");
        assert_ne!(a, b);
    }

    #[test]
    fn different_keys_produce_different_placeholders() {
        let v1 = SecretVault::with_key([1u8; 32]);
        let v2 = SecretVault::with_key([2u8; 32]);
        assert_ne!(
            v1.placeholder_for("sk-abc"),
            v2.placeholder_for("sk-abc"),
        );
    }

    #[test]
    fn register_is_idempotent() {
        let v = vault();
        let p1 = v.register("sk-abc");
        let p2 = v.register("sk-abc");
        assert_eq!(p1, p2);
        assert_eq!(v.len(), 1);
    }

    #[test]
    fn obfuscate_replaces_a_registered_secret() {
        let v = vault();
        let p = v.register("sk-abc123");
        let out = v.obfuscate("the key is sk-abc123 ok");
        assert_eq!(out, format!("the key is {p} ok"));
    }

    #[test]
    fn obfuscate_leaves_unknown_text_alone() {
        let v = vault();
        v.register("sk-abc123");
        let out = v.obfuscate("no secrets here");
        assert_eq!(out, "no secrets here");
    }

    #[test]
    fn obfuscate_with_no_registered_secrets_is_a_no_op() {
        let v = vault();
        assert_eq!(v.obfuscate("sk-abc"), "sk-abc");
    }

    #[test]
    fn obfuscate_is_longest_first() {
        // A short secret that is a prefix of a longer one must not
        // eat the longer one.
        let v = vault();
        v.register("sk-abc");
        v.register("sk-abcdef");
        let out = v.obfuscate("value sk-abcdef");
        // The long placeholder should be present; the short one is
        // not a substring of the result.
        let long_p = v.placeholder_for("sk-abcdef");
        let short_p = v.placeholder_for("sk-abc");
        assert!(out.contains(&long_p), "got {out}");
        assert!(!out.contains(&short_p), "got {out}");
    }

    #[test]
    fn obfuscate_replaces_multiple_occurrences() {
        let v = vault();
        let p = v.register("sk-abc");
        let out = v.obfuscate("a sk-abc b sk-abc c");
        assert_eq!(out, format!("a {p} b {p} c"));
    }

    #[test]
    fn deobfuscate_restores_the_secret() {
        let v = vault();
        let p = v.register("sk-abc123");
        let (out, n) = v.deobfuscate(&format!("the key is {p} ok"));
        assert_eq!(out, "the key is sk-abc123 ok");
        assert_eq!(n, 1);
    }

    #[test]
    fn deobfuscate_counts_each_occurrence() {
        let v = vault();
        let p = v.register("sk-abc");
        let (out, n) = v.deobfuscate(&format!("{p} and {p}"));
        assert_eq!(out, "sk-abc and sk-abc");
        assert_eq!(n, 2);
    }

    #[test]
    fn deobfuscate_leaves_unknown_placeholders_alone() {
        // A model that invented a placeholder shape does not get a
        // secret it did not have.
        let v = vault();
        v.register("sk-abc");
        let (out, n) = v.deobfuscate("«Credential-deadbeefdeadbeef»");
        assert_eq!(out, "«Credential-deadbeefdeadbeef»");
        assert_eq!(n, 0);
    }

    #[test]
    fn round_trip_obfuscate_then_deobfuscate() {
        let v = vault();
        v.register("sk-abc123");
        v.register("ghp_deadbeef");
        let original = "keys: sk-abc123 and ghp_deadbeef";
        let obfuscated = v.obfuscate(original);
        assert!(!obfuscated.contains("sk-abc123"));
        assert!(!obfuscated.contains("ghp_deadbeef"));
        let (restored, n) = v.deobfuscate(&obfuscated);
        assert_eq!(restored, original);
        assert_eq!(n, 2);
    }

    #[test]
    fn obfuscate_is_a_fixed_point() {
        // Running obfuscate twice produces the same output as running
        // it once — the placeholder contains no secret, so the second
        // pass changes nothing.
        let v = vault();
        v.register("sk-abc123");
        let once = v.obfuscate("the key is sk-abc123");
        let twice = v.obfuscate(&once);
        assert_eq!(once, twice);
    }

    #[test]
    fn contains_placeholder_detects_and_misses() {
        let v = vault();
        let p = v.register("sk-abc");
        assert!(v.contains_placeholder(&format!("has {p}")));
        assert!(!v.contains_placeholder("no placeholder"));
    }

    #[test]
    fn register_all_reports_only_new_secrets() {
        let v = vault();
        let first = v.register_all(vec![
            ("sk-a".to_string(), SecretSource::Env),
            ("sk-b".to_string(), SecretSource::Vendor),
        ]);
        assert_eq!(first.len(), 2);
        let second = v.register_all(vec![
            ("sk-a".to_string(), SecretSource::Env),
            ("sk-c".to_string(), SecretSource::Env),
        ]);
        assert_eq!(second.len(), 1, "only sk-c is new");
    }

    #[test]
    fn register_all_skips_empty_values() {
        let v = vault();
        let r = v.register_all(vec![
            (String::new(), SecretSource::Env),
            ("sk-a".to_string(), SecretSource::Env),
        ]);
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn load_or_create_round_trips_the_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("secret-placeholder.key");
        let v1 = SecretVault::load_or_create(&path).unwrap();
        let v2 = SecretVault::load_or_create(&path).unwrap();
        // Same key file -> same placeholders.
        assert_eq!(
            v1.placeholder_for("sk-abc"),
            v2.placeholder_for("sk-abc"),
        );
    }

    #[test]
    fn load_or_create_rejects_a_truncated_key_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let path = tmp.path().join("bad.key");
        std::fs::write(&path, b"too short").unwrap();
        let err = SecretVault::load_or_create(&path).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn debug_does_not_leak_the_key_or_the_secrets() {
        let v = vault();
        v.register("sk-super-secret");
        let dbg = format!("{v:?}");
        assert!(!dbg.contains("sk-super-secret"), "got {dbg}");
        assert!(!dbg.contains("42"), "key bytes leaked: {dbg}");
    }
}
