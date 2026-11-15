//! Agent identity: one Ed25519 key per agent.
//!
//! Design B of subtexture/docs/git-lex/2026_11_15_AGENT_IDENTITY_TOKEN_DESIGN.md
//! (chosen by goodlux, 2026-11-15). Each agent holds one key pair:
//!
//! - the private half is in the agent's own folder, in the part git never
//!   commits: `.lex/_ignore/identity/key` (an OpenSSH key file, so git can
//!   sign commits with it as it stands);
//! - the public half is on a list on the host, `~/.lex/identity/agents.json`,
//!   one entry per agent, keyed by the repo's first commit.
//!
//! The same key does two jobs. `git lex save` signs the commit with it, and
//! `git lex identity token` signs a short JSON Web Token (RFC 7519, EdDSA)
//! that the agent sends as `Authorization: Bearer <token>`. A service checks
//! either one against the host list; nothing an agent writes about itself is
//! believed.
//!
//! A copy of a soul folder made through git does not contain the key, so it
//! cannot sign as the agent, and `bind` refuses a second key for an agent
//! that already has one.
//!
//! The host list is the one thing written by hand-run commands; the two
//! files beside it are derived from it on every write and are never edited:
//! `allowed_signers` (what git reads) and `jwks.json` (RFC 7517, what
//! services read).

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The `aud` claim: one value for every Subtexture service.
pub const AUDIENCE: &str = "subtexture";
/// How long a token made here lasts.
pub const TOKEN_LIFETIME_SECS: u64 = 12 * 3600;
/// The longest lifetime a verifier accepts. The agent writes its own expiry,
/// so the verifier caps it.
pub const MAX_LIFETIME_SECS: u64 = 24 * 3600;
/// Clock difference tolerated between the signer and the verifier.
pub const SKEW_SECS: u64 = 60;

const SSH_ED25519: &str = "ssh-ed25519";

// ─── Where things are ──────────────────────────────────────────

/// `.lex/_ignore/identity/` in an agent's folder.
pub fn identity_dir(root: &Path) -> PathBuf {
    crate::layout::ignore_dir(root).join("identity")
}

/// The agent's private key (OpenSSH format, no passphrase, mode 600).
pub fn key_path(root: &Path) -> PathBuf {
    identity_dir(root).join("key")
}

/// The agent's current token, kept so a client can read a file.
pub fn token_path(root: &Path) -> PathBuf {
    identity_dir(root).join("token")
}

/// `~/.lex/identity/` — the host's list of agents' public keys.
pub fn host_dir() -> Option<PathBuf> {
    crate::gitlexd::lex_home().map(|h| h.join("identity"))
}

pub fn list_path(host: &Path) -> PathBuf {
    host.join("agents.json")
}

pub fn allowed_signers_path(host: &Path) -> PathBuf {
    host.join("allowed_signers")
}

pub fn jwks_path(host: &Path) -> PathBuf {
    host.join("jwks.json")
}

/// The IRI the graph already gives an agent; the token's `sub`.
pub fn agent_iri(genesis: &str) -> String {
    format!("https://repolex.ai/soul/Soul/{genesis}")
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The repo's first commit, asked of git itself. `.lex/repo.yml` also
/// records it, but that is a file the folder wrote about itself, and
/// identity does not take a folder's word.
pub fn first_commit(root: &Path) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-list", "--max-parents=0", "HEAD"])
        .output()
        .map_err(|e| format!("cannot run git: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    match (out.status.success(), lines.next(), lines.next()) {
        (true, Some(sha), None) => Ok(sha.to_string()),
        (true, Some(_), Some(_)) => Err(format!(
            "{} has more than one first commit, so it has no single id to bind a key to",
            root.display()
        )),
        _ => Err(format!("{} has no commits yet, so it has no id to bind a key to", root.display())),
    }
}

// ─── The host list ─────────────────────────────────────────────

/// One key of one agent. An agent that replaced its key has more than one
/// entry; the old one keeps the dates it was valid, so commits it signed
/// then still verify.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// The repo's first commit: the agent's id.
    pub genesis: String,
    /// For a person reading the list. Never used to decide anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `ssh-ed25519 <base64>`, as in the `.pub` file, without the comment.
    pub public_key: String,
    /// Seconds since 1970, UTC.
    pub valid_after: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_before: Option<u64>,
}

impl Entry {
    pub fn valid_at(&self, now: u64) -> bool {
        self.valid_after <= now + SKEW_SECS && self.valid_before.is_none_or(|end| now < end)
    }

    /// The 32 bytes of the Ed25519 public key.
    pub fn key_bytes(&self) -> Result<[u8; 32], String> {
        public_key_bytes(&self.public_key)
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct HostList {
    #[serde(default)]
    pub agents: Vec<Entry>,
}

impl HostList {
    /// The list at `host`. A missing file is an empty list; a file that
    /// does not parse is an error, never an empty list.
    pub fn load(host: &Path) -> Result<HostList, String> {
        let path = list_path(host);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{} does not parse: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(HostList::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    /// The entry valid now for this agent, if it has one.
    pub fn current(&self, genesis: &str, now: u64) -> Option<&Entry> {
        self.agents.iter().find(|e| e.genesis == genesis && e.valid_at(now))
    }

    /// Write the list and the two files derived from it.
    pub fn save(&self, host: &Path) -> Result<(), String> {
        std::fs::create_dir_all(host).map_err(|e| format!("cannot create {}: {e}", host.display()))?;
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())? + "\n";
        write_atomic(&list_path(host), json.as_bytes(), 0o644)?;
        write_atomic(&allowed_signers_path(host), self.allowed_signers().as_bytes(), 0o644)?;
        write_atomic(&jwks_path(host), self.jwks()?.as_bytes(), 0o644)
    }

    /// git's allowed-signers file: one line per key, the agent's id as the
    /// principal, limited to signing git objects and to the key's dates.
    pub fn allowed_signers(&self) -> String {
        let mut out = String::from("# Derived from agents.json by git-lex. Do not edit.\n");
        for e in &self.agents {
            out.push_str(&format!("{} namespaces=\"git\",valid-after=\"{}\"", e.genesis, ssh_time(e.valid_after)));
            if let Some(end) = e.valid_before {
                out.push_str(&format!(",valid-before=\"{}\"", ssh_time(end)));
            }
            out.push_str(&format!(" {}\n", e.public_key));
        }
        out
    }

    /// The keys valid for tokens now, as a JSON Web Key Set. `kid` is the
    /// agent's id. Only keys with no end date: a retired key still verifies
    /// old commits, but must not verify a new token.
    pub fn jwks(&self) -> Result<String, String> {
        let mut keys = Vec::new();
        for e in self.agents.iter().filter(|e| e.valid_before.is_none()) {
            keys.push(serde_json::json!({
                "kty": "OKP",
                "crv": "Ed25519",
                "alg": "EdDSA",
                "use": "sig",
                "kid": e.genesis,
                "x": URL_SAFE_NO_PAD.encode(e.key_bytes()?),
            }));
        }
        serde_json::to_string_pretty(&serde_json::json!({ "keys": keys }))
            .map(|s| s + "\n")
            .map_err(|e| e.to_string())
    }
}

/// `YYYYMMDDHHMMSSZ`, the UTC form OpenSSH reads in allowed-signers options.
fn ssh_time(secs: u64) -> String {
    let (days, rem) = (secs / 86400, secs % 86400);
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}{m:02}{d:02}{:02}{:02}{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// The 32 key bytes inside `ssh-ed25519 <base64>`.
fn public_key_bytes(public_key: &str) -> Result<[u8; 32], String> {
    let mut parts = public_key.split_whitespace();
    let (kind, blob) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    if kind != SSH_ED25519 {
        return Err(format!("a public key of type {kind:?} is not accepted; only {SSH_ED25519} is"));
    }
    let blob = STANDARD.decode(blob).map_err(|e| format!("public key is not base64: {e}"))?;
    // string "ssh-ed25519", then string <32 bytes>, each with a 4-byte length.
    let expect = 4 + SSH_ED25519.len() + 4 + 32;
    if blob.len() != expect || &blob[4..4 + SSH_ED25519.len()] != SSH_ED25519.as_bytes() {
        return Err("public key is not a well-formed ssh-ed25519 key".into());
    }
    Ok(blob[expect - 32..].try_into().expect("length checked above"))
}

fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<(), String> {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
            .map_err(|e| format!("cannot set permissions on {}: {e}", tmp.display()))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    std::fs::rename(&tmp, path).map_err(|e| format!("cannot move {} into place: {e}", path.display()))
}

// ─── The agent's key ───────────────────────────────────────────

/// Does this folder hold a key?
pub fn has_key(root: &Path) -> bool {
    key_path(root).is_file()
}

/// Make the agent's key. Refuses when one is there: a key is made once.
fn create_key(root: &Path, genesis: &str) -> Result<(), String> {
    let dir = identity_dir(root);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let key = key_path(root);
    if key.exists() {
        return Err(format!("{} already exists", key.display()));
    }
    // ssh-keygen writes the format git signs with, and git needs ssh-keygen
    // to sign at all, so it is already a requirement.
    let out = std::process::Command::new("ssh-keygen")
        .args(["-q", "-t", "ed25519", "-N", "", "-C", genesis, "-f"])
        .arg(&key)
        .output()
        .map_err(|e| format!("cannot run ssh-keygen (git needs it to sign commits): {e}"))?;
    if !out.status.success() {
        return Err(format!("ssh-keygen could not make a key: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(())
}

/// The public half of the key in this folder, as `ssh-ed25519 <base64>`,
/// worked out from the private key itself (the `.pub` file beside it is a
/// convenience ssh-keygen leaves, and is not trusted to match).
pub fn public_key(root: &Path) -> Result<String, String> {
    let signing = signing_key(root)?;
    let mut blob = Vec::with_capacity(51);
    blob.extend_from_slice(&(SSH_ED25519.len() as u32).to_be_bytes());
    blob.extend_from_slice(SSH_ED25519.as_bytes());
    blob.extend_from_slice(&32u32.to_be_bytes());
    blob.extend_from_slice(signing.verifying_key().as_bytes());
    Ok(format!("{SSH_ED25519} {}", STANDARD.encode(blob)))
}

fn signing_key(root: &Path) -> Result<SigningKey, String> {
    let path = key_path(root);
    let key = ssh_key::PrivateKey::read_openssh_file(&path)
        .map_err(|e| format!("cannot read the key at {}: {e}", path.display()))?;
    let pair = key
        .key_data()
        .ed25519()
        .ok_or_else(|| format!("the key at {} is not an Ed25519 key", path.display()))?;
    Ok(SigningKey::from_bytes(&pair.private.to_bytes()))
}

/// The key to sign with as the agent whose folder this is: its private key
/// path, when the folder holds a key and that key is the agent's current
/// one on the host list. None otherwise; an unlisted key signs nothing,
/// since nobody could verify it.
pub fn bound_key(root: &Path, host: &Path, now: u64) -> Option<PathBuf> {
    if !has_key(root) {
        return None;
    }
    let genesis = first_commit(root).ok()?;
    let list = HostList::load(host).ok()?;
    let mine = public_key(root).ok()?;
    (list.current(&genesis, now)?.public_key == mine).then(|| key_path(root))
}

/// The git options that make one command sign with this key. Passed on
/// the command line and never written to the repo's git config: a key set
/// in a folder's config would sign a visitor's plain `git commit` as the
/// folder's owner.
pub fn signing_args(key: &Path) -> Vec<String> {
    vec!["-c".into(), "gpg.format=ssh".into(), "-c".into(), format!("user.signingkey={}", key.display())]
}

/// The git options that make one command check signatures against the
/// host list.
pub fn verifying_args(host: &Path) -> Vec<String> {
    vec![
        "-c".into(),
        "gpg.format=ssh".into(),
        "-c".into(),
        format!("gpg.ssh.allowedSignersFile={}", allowed_signers_path(host).display()),
    ]
}

// ─── Binding an agent to its key ───────────────────────────────

#[derive(Debug, PartialEq)]
pub enum Bound {
    /// A new entry was added to the host list.
    New,
    /// This folder's key was already the agent's key.
    Already,
}

/// Put this folder's key on the host list as the key of the agent whose
/// repo this is, making the key first if the folder has none.
///
/// This is the one act that says "this folder is this agent". It never
/// replaces a key: if the agent already has a different key on the list,
/// this folder is a copy (or the key was lost), and it is refused.
pub fn bind(root: &Path, host: &Path, now: u64) -> Result<Bound, String> {
    let genesis = first_commit(root)?;
    let mut list = HostList::load(host)?;
    let had_key = has_key(root);
    if let Some(current) = list.current(&genesis, now) {
        if had_key && public_key(root)? == current.public_key {
            return Ok(Bound::Already);
        }
        return Err(format!(
            "This agent ({}) already has a key, and it is not the one in this folder.\n\
             Either this folder is a copy of the agent's repo, or the agent's key was lost.\n\
             Nothing was changed. If the key was lost, a person ends the old key first:\n\
             \x20   git lex identity retire {}",
            &genesis[..8.min(genesis.len())],
            genesis
        ));
    }
    if !had_key {
        create_key(root, &genesis)?;
    }
    list.agents.push(Entry {
        genesis,
        name: crate::RepoYml::load(root).name,
        public_key: public_key(root)?,
        valid_after: now,
        valid_before: None,
    });
    list.save(host)?;
    Ok(Bound::New)
}

/// End an agent's current key as of now. Its entry stays on the list with
/// an end date, so what it signed before still verifies. Returns whether
/// there was a current key to end.
pub fn retire(genesis: &str, host: &Path, now: u64) -> Result<bool, String> {
    let mut list = HostList::load(host)?;
    let mut ended = false;
    for e in list.agents.iter_mut().filter(|e| e.genesis == genesis && e.valid_before.is_none()) {
        e.valid_before = Some(now);
        ended = true;
    }
    if ended {
        list.save(host)?;
    }
    Ok(ended)
}

// ─── Tokens ────────────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Claims {
    /// Self-issued, so the issuer is the agent (RFC 7523: `iss` = `sub`).
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub iat: u64,
    pub exp: u64,
    pub jti: String,
}

/// Sign a new token for the agent whose folder this is.
pub fn mint(root: &Path, now: u64) -> Result<String, String> {
    let genesis = first_commit(root)?;
    let key = signing_key(root)?;
    let mut id = [0u8; 16];
    getrandom::getrandom(&mut id).map_err(|e| format!("no random bytes from the system: {e}"))?;
    let iri = agent_iri(&genesis);
    let claims = Claims {
        iss: iri.clone(),
        sub: iri,
        aud: AUDIENCE.into(),
        iat: now,
        exp: now + TOKEN_LIFETIME_SECS,
        jti: id.iter().map(|b| format!("{b:02x}")).collect(),
    };
    let header = serde_json::json!({ "alg": "EdDSA", "typ": "JWT", "kid": genesis });
    let signed = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(serde_json::to_string(&claims).map_err(|e| e.to_string())?)
    );
    let signature = key.sign(signed.as_bytes());
    Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes())))
}

/// The agent's token: the one on file while it is this key's and has more
/// than half its life left, otherwise a new one, written to the file.
pub fn token(root: &Path, now: u64) -> Result<String, String> {
    let path = token_path(root);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let kept = text.trim();
        if let Ok(claims) = check_own(root, kept, now)
            && claims.exp.saturating_sub(now) > TOKEN_LIFETIME_SECS / 2 {
                return Ok(kept.to_string());
            }
    }
    let fresh = mint(root, now)?;
    write_atomic(&path, format!("{fresh}\n").as_bytes(), 0o600)?;
    Ok(fresh)
}

/// Is this token signed by the key in this folder, and in date?
fn check_own(root: &Path, token: &str, now: u64) -> Result<Claims, String> {
    let genesis = first_commit(root)?;
    let list = HostList {
        agents: vec![Entry { genesis, name: None, public_key: public_key(root)?, valid_after: 0, valid_before: None }],
    };
    verify(token, &list, now).map(|(claims, _)| claims)
}

/// Check a token against the host list. This is the whole of what a
/// service does, in the order the design gives; nothing inside the token
/// chooses what is trusted.
///
/// 1. The signature type is Ed25519, fixed here, never read from the header
///    to decide how to verify.
/// 2. The header's `kid` only selects which entry of the list to try.
/// 3. The signature is verified with that entry's key.
/// 4. `sub` must be the agent that entry names.
/// 5. `iat` not in the future, `exp` in the future, and no more than
///    `MAX_LIFETIME_SECS` between them.
/// 6. `aud` is `AUDIENCE`.
pub fn verify<'a>(token: &str, list: &'a HostList, now: u64) -> Result<(Claims, &'a Entry), String> {
    let mut parts = token.split('.');
    let (Some(h), Some(c), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err("not a token: expected three parts separated by dots".into());
    };
    let decode = |part: &str, what: &str| {
        URL_SAFE_NO_PAD.decode(part).map_err(|e| format!("the token's {what} is not base64url: {e}"))
    };
    let header: serde_json::Value =
        serde_json::from_slice(&decode(h, "header")?).map_err(|e| format!("the token's header is not JSON: {e}"))?;
    if header.get("alg").and_then(|a| a.as_str()) != Some("EdDSA") {
        return Err("the token is not signed with EdDSA, the only signature type accepted".into());
    }
    let kid = header.get("kid").and_then(|k| k.as_str()).ok_or("the token names no key (no kid)")?;
    // Only an entry with no end date verifies a token: a retired key still
    // stands behind the commits it signed, never behind a new request.
    let entry = list
        .agents
        .iter()
        .find(|e| e.genesis == kid && e.valid_before.is_none() && e.valid_at(now))
        .ok_or_else(|| format!("no agent on the list has the id {kid}"))?;
    let key = VerifyingKey::from_bytes(&entry.key_bytes()?).map_err(|e| format!("the listed key is not usable: {e}"))?;
    let signature = ed25519_dalek::Signature::from_slice(&decode(s, "signature")?)
        .map_err(|_| "the token's signature is the wrong length".to_string())?;
    key.verify(format!("{h}.{c}").as_bytes(), &signature)
        .map_err(|_| "the signature does not match the listed key for this agent".to_string())?;
    let claims: Claims =
        serde_json::from_slice(&decode(c, "claims")?).map_err(|e| format!("the token's claims are incomplete: {e}"))?;
    if claims.sub != agent_iri(&entry.genesis) {
        return Err("the token says it is a different agent than the one whose key signed it".into());
    }
    if claims.iat > now + SKEW_SECS {
        return Err("the token is dated in the future".into());
    }
    if claims.exp <= now {
        return Err("the token has expired".into());
    }
    if claims.exp < claims.iat || claims.exp - claims.iat > MAX_LIFETIME_SECS {
        return Err(format!("the token's lifetime is longer than the {} hours allowed", MAX_LIFETIME_SECS / 3600));
    }
    if claims.aud != AUDIENCE {
        return Err(format!("the token is for {:?}, not for {AUDIENCE:?}", claims.aud));
    }
    Ok((claims, entry))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let ok = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    /// A repo with one commit and a `.lex/`, plus an empty host directory.
    fn soul(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("gitlex-identity-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (repo, host) = (base.join("repo"), base.join("host"));
        std::fs::create_dir_all(repo.join(".lex")).unwrap();
        git(&repo, &["init", "-q"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        (repo, host)
    }

    const NOW: u64 = 1_794_700_000;

    #[test]
    fn bind_makes_a_key_and_a_token_that_verifies() {
        let (repo, host) = soul("bind");
        assert_eq!(bind(&repo, &host, NOW).unwrap(), Bound::New);
        assert_eq!(bind(&repo, &host, NOW).unwrap(), Bound::Already);
        let list = HostList::load(&host).unwrap();
        let genesis = first_commit(&repo).unwrap();
        let t = token(&repo, NOW).unwrap();
        let (claims, entry) = verify(&t, &list, NOW).unwrap();
        assert_eq!(entry.genesis, genesis);
        assert_eq!(claims.sub, agent_iri(&genesis));
        assert_eq!(claims.exp - claims.iat, TOKEN_LIFETIME_SECS);
        // Kept while fresh, replaced past half its life.
        assert_eq!(token(&repo, NOW + 60).unwrap(), t);
        assert_ne!(token(&repo, NOW + TOKEN_LIFETIME_SECS / 2 + 1).unwrap(), t);
        // The derived files name the same key.
        assert!(std::fs::read_to_string(allowed_signers_path(&host)).unwrap().contains(&entry.public_key));
        assert!(std::fs::read_to_string(jwks_path(&host)).unwrap().contains(&genesis));
    }

    #[test]
    fn a_copy_of_the_repo_cannot_become_the_agent() {
        let (repo, host) = soul("copy");
        bind(&repo, &host, NOW).unwrap();
        let clone = repo.parent().unwrap().join("clone");
        assert!(std::process::Command::new("git").arg("clone").arg("-q").arg(&repo).arg(&clone).status().unwrap().success());
        std::fs::create_dir_all(clone.join(".lex")).unwrap();
        assert_eq!(first_commit(&clone).unwrap(), first_commit(&repo).unwrap());
        // The clone has no key, and may not add one.
        assert!(!has_key(&clone));
        let refused = bind(&clone, &host, NOW).unwrap_err();
        assert!(refused.contains("already has a key"), "{refused}");
        assert!(!has_key(&clone), "a refused bind leaves no key behind");
        // A key it makes for itself signs nothing a verifier accepts.
        create_key(&clone, &first_commit(&clone).unwrap()).unwrap();
        let forged = mint(&clone, NOW).unwrap();
        let list = HostList::load(&host).unwrap();
        assert!(verify(&forged, &list, NOW).unwrap_err().contains("does not match"));
    }

    #[test]
    fn verify_refuses_what_the_design_says_it_must() {
        let (repo, host) = soul("verify");
        bind(&repo, &host, NOW - 7200).unwrap();
        let list = HostList::load(&host).unwrap();
        let t = mint(&repo, NOW).unwrap();
        assert!(verify(&t, &list, NOW + TOKEN_LIFETIME_SECS).unwrap_err().contains("expired"));
        assert!(verify(&t, &list, NOW - 3600).unwrap_err().contains("future"));
        assert!(verify(&t, &HostList::default(), NOW).unwrap_err().contains("no agent on the list"));

        // Claims the agent wrote for itself are not believed. Re-sign
        // altered claims with the agent's real key.
        let key = signing_key(&repo).unwrap();
        let genesis = first_commit(&repo).unwrap();
        let sign = |header: serde_json::Value, claims: serde_json::Value| {
            let signed = format!("{}.{}", URL_SAFE_NO_PAD.encode(header.to_string()), URL_SAFE_NO_PAD.encode(claims.to_string()));
            let sig = key.sign(signed.as_bytes());
            format!("{signed}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
        };
        let header = serde_json::json!({"alg": "EdDSA", "typ": "JWT", "kid": genesis});
        let good = serde_json::json!({"iss": agent_iri(&genesis), "sub": agent_iri(&genesis), "aud": AUDIENCE, "iat": NOW, "exp": NOW + 3600, "jti": "x"});
        assert!(verify(&sign(header.clone(), good.clone()), &list, NOW).is_ok());
        let with = |k: &str, v: serde_json::Value| {
            let mut c = good.clone();
            c[k] = v;
            c
        };
        let other = agent_iri("0000000000000000000000000000000000000000");
        assert!(verify(&sign(header.clone(), with("sub", other.into())), &list, NOW).unwrap_err().contains("different agent"));
        assert!(verify(&sign(header.clone(), with("exp", (NOW + MAX_LIFETIME_SECS + 1).into())), &list, NOW).unwrap_err().contains("lifetime"));
        assert!(verify(&sign(header.clone(), with("aud", "elsewhere".into())), &list, NOW).unwrap_err().contains("not for"));
        // Dated a year ahead with a short lifetime: the lifetime cap alone would pass it.
        let ahead = NOW + 365 * 86400;
        let mut future = with("iat", ahead.into());
        future["exp"] = (ahead + 3600).into();
        assert!(verify(&sign(header.clone(), future), &list, NOW).unwrap_err().contains("future"));
        // Missing iat is refused, not defaulted.
        let mut no_iat = good.clone();
        no_iat.as_object_mut().unwrap().remove("iat");
        assert!(verify(&sign(header, no_iat), &list, NOW).unwrap_err().contains("incomplete"));
        // The header cannot pick another signature type.
        let none = serde_json::json!({"alg": "none", "typ": "JWT", "kid": genesis});
        assert!(verify(&sign(none, good), &list, NOW).unwrap_err().contains("EdDSA"));
    }

    #[test]
    fn a_retired_key_stops_verifying_tokens_and_can_be_replaced() {
        let (repo, host) = soul("retire");
        bind(&repo, &host, NOW).unwrap();
        let genesis = first_commit(&repo).unwrap();
        let t = mint(&repo, NOW).unwrap();
        assert!(retire(&genesis, &host, NOW + 10).unwrap());
        assert!(!retire(&genesis, &host, NOW + 11).unwrap());
        let list = HostList::load(&host).unwrap();
        assert!(verify(&t, &list, NOW + 20).is_err());
        // The old entry is kept, with its end date, for old commits.
        assert_eq!(list.agents.len(), 1);
        assert!(list.allowed_signers().contains("valid-before"));
        assert!(!list.jwks().unwrap().contains(&genesis));
        // A new key can now be bound.
        std::fs::remove_file(key_path(&repo)).unwrap();
        let _ = std::fs::remove_file(key_path(&repo).with_extension("pub"));
        assert_eq!(bind(&repo, &host, NOW + 30).unwrap(), Bound::New);
        assert_eq!(HostList::load(&host).unwrap().agents.len(), 2);
    }

    /// The real thing, through git and ssh-keygen: a commit signed with a
    /// bound key verifies against the derived allowed-signers file and
    /// names the agent's id; one signed with a key that is not on the list
    /// does not.
    #[test]
    fn git_accepts_a_bound_agents_commit_and_refuses_a_copys() {
        let (repo, host) = soul("gitsign");
        let now = now_secs();
        bind(&repo, &host, now - 60).unwrap();
        let genesis = first_commit(&repo).unwrap();
        let key = bound_key(&repo, &host, now).expect("the bound key");
        let commit = |dir: &Path, key: &Path, msg: &str| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(signing_args(key))
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-S", "--allow-empty", "-m", msg])
                .output()
                .unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        let verify_head = |dir: &Path| {
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(verifying_args(&host))
                .args(["verify-commit", "HEAD"])
                .output()
                .unwrap();
            (out.status.success(), String::from_utf8_lossy(&out.stderr).to_string())
        };
        commit(&repo, &key, "signed by the agent");
        let (good, said) = verify_head(&repo);
        assert!(good, "{said}");
        assert!(said.contains(&genesis), "the agent's id is the name git reports: {said}");

        // A copy with a key of its own: it can sign, nobody accepts it.
        let clone = repo.parent().unwrap().join("clone");
        assert!(std::process::Command::new("git").arg("clone").arg("-q").arg(&repo).arg(&clone).status().unwrap().success());
        std::fs::create_dir_all(clone.join(".lex")).unwrap();
        assert_eq!(bound_key(&clone, &host, now), None);
        create_key(&clone, &genesis).unwrap();
        assert_eq!(bound_key(&clone, &host, now), None, "an unlisted key is not a signing key");
        commit(&clone, &key_path(&clone), "signed by the copy");
        let (good, said) = verify_head(&clone);
        assert!(!good, "{said}");
    }

    #[test]
    fn ssh_time_is_utc_calendar_time() {
        assert_eq!(ssh_time(0), "19700101000000Z");
        assert_eq!(ssh_time(1_794_700_000), "20261114234640Z");
        assert_eq!(ssh_time(951_782_400), "20000229000000Z");
    }
}
