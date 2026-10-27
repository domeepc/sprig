//! Signed changes. A change made locally (commit, merge) is signed over its
//! own JSON. A landed change is built by the server, so the client signs
//! what it asked for instead (base, edits, message, metadata) and the server
//! keeps that request as the proof; verifying checks the signature and that
//! the change touched nothing but the signed paths.

use crate::store::{short, Change, Meta, Repo};
use crate::tree::diff_flat;
use crate::util::{self, Result};
use crate::{bail, err};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Signature {
    /// "ed25519:<base64 public key>"
    #[serde(default)]
    pub key: String,
    /// base64
    #[serde(default)]
    pub sig: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub land: Option<LandProof>,
    /// Signed paths the server edited at a new path, because someone else had
    /// renamed them since the signed base. Not signed; checked against
    /// history instead.
    #[serde(default, deserialize_with = "util::nullable", skip_serializing_if = "BTreeMap::is_empty")]
    pub moved: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct LandProof {
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub base: String,
    /// path -> blob id, "" = deleted
    #[serde(default, deserialize_with = "util::nullable")]
    pub files: BTreeMap<String, String>,
}

const KEY_PREFIX: &str = "ed25519:";

/// Go's os.UserConfigDir.
fn config_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if cfg!(windows) {
        return env("AppData");
    }
    if cfg!(target_os = "macos") {
        return env("HOME").map(|h| h.join("Library/Application Support"));
    }
    env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))
}

fn key_path() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("SPRIG_KEY").filter(|v| !v.is_empty()) {
        return Ok(p.into());
    }
    Ok(config_dir().ok_or_else(|| err!("neither $XDG_CONFIG_HOME nor $HOME are defined"))?.join("sprig/signing.key"))
}

/// The user's private key, or None if they have none.
pub fn signing_key() -> Result<Option<SigningKey>> {
    let p = key_path()?;
    let b = match std::fs::read_to_string(&p) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        r => r?,
    };
    let seed: [u8; 32] = B64
        .decode(b.trim())
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| err!("{} is not a sprig signing key", p.display()))?;
    Ok(Some(SigningKey::from_bytes(&seed)))
}

pub fn generate_key() -> Result<(PathBuf, String)> {
    let p = key_path()?;
    if p.exists() {
        bail!("{} already exists", p.display());
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| err!("random: {e}"))?;
    let key = SigningKey::from_bytes(&seed);
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    std::io::Write::write_all(&mut opts.open(&p)?, format!("{}\n", B64.encode(seed)).as_bytes())?;
    Ok((p, public_key(&key)))
}

fn public_key(key: &SigningKey) -> String {
    format!("{KEY_PREFIX}{}", B64.encode(key.verifying_key().to_bytes()))
}

pub fn change_payload(c: &Change) -> Vec<u8> {
    let u = Change { sig: None, ..c.clone() };
    [b"sprig-change-v1\n".as_slice(), &util::to_json(&u)].concat()
}

pub fn land_payload(p: &LandProof, message: &str, author: &str, meta: &Meta) -> Vec<u8> {
    // the fields in sorted order, as Go encodes a map
    #[derive(Serialize)]
    struct Payload<'a> {
        author: &'a str,
        message: &'a str,
        meta: &'a Meta,
        proof: &'a LandProof,
    }
    [b"sprig-land-v1\n".as_slice(), &util::to_json(&Payload { author, message, meta, proof: p })].concat()
}

pub fn sign(key: &SigningKey, payload: &[u8]) -> Signature {
    Signature { key: public_key(key), sig: B64.encode(key.sign(payload).to_bytes()), ..Default::default() }
}

pub fn check_sig(s: &Signature, payload: &[u8]) -> Result<()> {
    let pub_key: Option<[u8; 32]> =
        s.key.strip_prefix(KEY_PREFIX).and_then(|k| B64.decode(k).ok()).and_then(|k| k.try_into().ok());
    let Some(vk) = pub_key.and_then(|k| VerifyingKey::from_bytes(&k).ok()) else {
        bail!("malformed key");
    };
    let sig: Option<[u8; 64]> = B64.decode(&s.sig).ok().and_then(|b| b.try_into().ok());
    match sig {
        Some(sig) if vk.verify(payload, &ed25519_dalek::Signature::from_bytes(&sig)).is_ok() => Ok(()),
        _ => bail!("signature does not match"),
    }
}

impl Repo {
    /// Checks a change's signature. It returns the signing key, or an error
    /// saying why the change is not validly signed.
    pub fn verify_change(&self, c: &Change) -> Result<String, (String, crate::util::Error)> {
        if !c.projected.is_empty() {
            return Err((String::new(), err!("projected from {}; verify that change on the server", short(&c.projected))));
        }
        let Some(sig) = &c.sig else { return Err((String::new(), err!("unsigned"))) };
        let key = sig.key.clone();
        let fail = |e| Err((key.clone(), e));
        let Some(p) = &sig.land else {
            return check_sig(sig, &change_payload(c)).map(|_| key.clone()).or_else(fail);
        };
        if let Err(e) = check_sig(sig, &land_payload(p, &c.message, &c.author, &c.meta)) {
            return fail(e);
        }
        if c.parents.len() > 1 {
            return fail(err!("landed change has several parents"));
        }
        let parent = c.parents.first().cloned().unwrap_or_default();
        let (before, after) = match (self.change_flat(&parent), self.flatten(&c.tree)) {
            (Ok(b), Ok(a)) => (b, a),
            (Err(e), _) | (_, Err(e)) => return fail(e),
        };
        let mut allowed: HashSet<&str> = p.files.keys().map(String::as_str).collect();
        if !sig.moved.is_empty() {
            let based = match self.change_flat(&p.base) {
                Ok(f) => f,
                Err(e) => return fail(e),
            };
            for (from, to) in &sig.moved {
                let ok = p.files.contains_key(from)
                    && based.contains_key(from)
                    && !before.contains_key(from)
                    && before.contains_key(to);
                if !ok {
                    return fail(err!("unsupported move claim {from} -> {to}"));
                }
                allowed.insert(to);
            }
        }
        for d in diff_flat(&before, &after) {
            if !allowed.contains(d.path.as_str()) {
                return fail(err!("server changed unsigned path {}", d.path));
            }
        }
        Ok(key)
    }
}
