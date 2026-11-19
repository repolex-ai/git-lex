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
use ed25519_dalek::{Signer as _, SigningKey, Verifier as _, VerifyingKey};
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

/// `~/.lex/identity/owner.json` — which identity this machine belongs to.
pub fn owner_path(host: &Path) -> PathBuf {
    host.join("owner.json")
}

/// The owner of this machine: the one identity that is a person's, not an
/// agent's (goodlux, 2026-11-16). A person is identified exactly as an agent
/// is, by a repo, its first commit and a key; this record is what tells the
/// two apart. Services read it to answer "may this caller read a store that
/// is not its own": the owner may read every store on the machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Owner {
    /// The owner's address, the same value a token carries in `sub`.
    pub owner: String,
    /// The first commit of the owner's repo.
    pub genesis: String,
    /// Unix seconds when this was recorded.
    pub since: u64,
}

/// The recorded owner. A missing file is no owner; a file that does not
/// parse is an error, never "no owner".
pub fn load_owner(host: &Path) -> Result<Option<Owner>, String> {
    let path = owner_path(host);
    match std::fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str(&text).map(Some).map_err(|e| format!("{} does not parse: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", path.display())),
    }
}

/// What [`set_owner`] found.
#[derive(Debug, PartialEq)]
pub enum OwnerSet {
    New,
    Already,
}

/// Record the repo at `root` as this machine's owner. It must already have
/// a key on the list. A machine has one owner: a second, different one is
/// refused, and the way to change it is for a person to remove the file.
pub fn set_owner(root: &Path, host: &Path, now: u64) -> Result<OwnerSet, String> {
    let genesis = first_commit(root)?;
    let list = HostList::load(host)?;
    if bound_key(root, host, now).is_none() || list.current(&genesis, now).is_none() {
        return Err(format!(
            "{} has no key on this machine's list yet. Run `git lex identity bind` in it first.",
            root.display()
        ));
    }
    if let Some(have) = load_owner(host)? {
        if have.genesis == genesis {
            return Ok(OwnerSet::Already);
        }
        return Err(format!(
            "this machine already has an owner ({}). A machine has one. To change it, remove {} by hand and run this again.",
            have.owner,
            owner_path(host).display()
        ));
    }
    let owner = Owner { owner: address_of(&list, &genesis), genesis, since: now };
    let json = serde_json::to_string_pretty(&owner).map_err(|e| e.to_string())? + "\n";
    write_atomic(&owner_path(host), json.as_bytes(), 0o644)?;
    Ok(OwnerSet::New)
}

/// The address of the agent with this id. The id is the agent's username,
/// or its first commit while it has no username on the list. Ask
/// [`Entry::address`] or [`address_of`] which of the two applies; do not
/// build an address from a first commit by hand.
pub fn agent_iri(id: &str) -> String {
    format!("https://repolex.ai/soul/Soul/{id}")
}

/// The address the list gives the agent with this first commit: its current
/// entry's, else any entry's, else the first-commit form for an agent the
/// list has never seen.
pub fn address_of(list: &HostList, genesis: &str) -> String {
    let mine = || list.agents.iter().filter(|e| e.genesis == genesis);
    mine()
        .find(|e| e.valid_before.is_none())
        .or_else(|| mine().next())
        .map_or_else(|| agent_iri(genesis), Entry::address)
}

/// Words a username may not be: each is the first segment of some other
/// kind of address.
const RESERVED_NAMES: &[&str] = &["ontology", "subtexture", "soul", "copia", "pan", "ravel", "horae", "pool", "syrinx"];

/// One word of lowercase letters and digits, with at least one letter that
/// is not a hex digit, so a name can never be read as the start of a first
/// commit (goodlux, 2026-11-18).
pub fn valid_username(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().any(|c| ('g'..='z').contains(&c))
        && !RESERVED_NAMES.contains(&name)
}

/// Take the username this folder's SOUL.md declares onto the host list, if
/// the agent has none there yet. Returns the username the list holds
/// afterwards.
///
/// The squad in `.lex/repo.yml` is taken the same way, once the agent has a
/// username; the two together are the agent's store, and its address.
///
/// The list's word is final: once an agent has a username, a different one
/// in SOUL.md changes nothing. Only the folder holding the agent's listed
/// key can do this, a name another agent holds is refused, and from this
/// moment the agent's tokens and grants carry the new address, so the token
/// on file is dropped and grants it signed before must be granted again.
pub fn adopt_username(root: &Path, host: &Path, now: u64) -> Result<Option<String>, String> {
    // `.lex/repo.yml` first: it is where the squad is read from below.
    crate::soul_md::heal_store_lines(root);
    let genesis = first_commit(root)?;
    let mut list = HostList::load(host)?;
    let Some(current) = list.current(&genesis, now).cloned() else {
        return Ok(None);
    };
    let mut username = current.username.clone();
    if username.is_none() {
        let Some(declared) = crate::soul_md::username_at(root) else {
            return Ok(None);
        };
        if !valid_username(&declared) {
            return Err(format!(
                "soul.Soul.username in {} is {declared:?}. A username is one word of lowercase letters and digits, \
                 with at least one letter from g to z, and not the name of a kit or engine.",
                root.join("SOUL.md").display()
            ));
        }
        if let Some(other) = list.agents.iter().find(|e| e.genesis != genesis && e.username.as_deref() == Some(declared.as_str())) {
            return Err(format!(
                "the username {declared} already belongs to another agent on this machine ({}). Two agents cannot share one.",
                &other.genesis[..8.min(other.genesis.len())]
            ));
        }
        username = Some(declared);
    }
    let squad = current.squad.clone().or_else(|| crate::RepoYml::load(root).squad.filter(|s| valid_username(s)));
    if username == current.username && squad == current.squad {
        return Ok(username);
    }
    if bound_key(root, host, now).is_none() {
        return Ok(current.username);
    }
    for e in list.agents.iter_mut().filter(|e| e.genesis == genesis) {
        e.username = username.clone();
        e.squad = squad.clone();
    }
    list.save(host)?;
    if let Some(mut owner) = load_owner(host)?
        && owner.genesis == genesis
    {
        owner.owner = address_of(&list, &genesis);
        let json = serde_json::to_string_pretty(&owner).map_err(|e| e.to_string())? + "\n";
        write_atomic(&owner_path(host), json.as_bytes(), 0o644)?;
    }
    let _ = std::fs::remove_file(token_path(root));
    Ok(username)
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
    /// The agent's one-word name, `soul.Soul.username` in its SOUL.md, once
    /// the list has taken it (see [`adopt_username`]). It never changes
    /// after that. Absent for an agent that has not declared one yet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,
    /// The squad the agent's store belongs to, `squad` in its
    /// `.lex/repo.yml`, taken onto the list the same way and as final.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub squad: Option<String>,
    /// `ssh-ed25519 <base64>`, as in the `.pub` file, without the comment.
    pub public_key: String,
    /// Seconds since 1970, UTC.
    pub valid_after: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub valid_before: Option<u64>,
}

impl Entry {
    /// The agent's address: what its tokens and grants carry in `sub`, and
    /// what its SOUL.md says in `soul.Soul.id`. Named by username once it
    /// has one (goodlux, 2026-11-19), by first commit until then.
    ///
    /// With a squad as well it is the address the agent's own store gives
    /// its Soul: `https://repolex.ai/{squad}/{username}/soul/Soul/{username}`.
    pub fn address(&self) -> String {
        match (&self.squad, &self.username) {
            (Some(squad), Some(user)) => format!("{}{squad}/{user}/soul/Soul/{user}", crate::PLATFORM_ROOT),
            (_, user) => agent_iri(user.as_deref().unwrap_or(&self.genesis)),
        }
    }

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
    /// agent's first commit; `sub` is its address. Only keys with no end date: a retired key still verifies
    /// old commits, but must not verify a new token.
    pub fn jwks(&self) -> Result<String, String> {
        let mut keys = Vec::new();
        for e in self.agents.iter().filter(|e| e.valid_before.is_none()) {
            let mut key = serde_json::json!({
                "kty": "OKP",
                "crv": "Ed25519",
                "alg": "EdDSA",
                "use": "sig",
                "kid": e.genesis,
                "x": URL_SAFE_NO_PAD.encode(e.key_bytes()?),
                // What a token or grant signed with this key must carry in
                // `sub`. A checker compares against this and never builds
                // the address itself.
                "sub": e.address(),
            });
            if let Some(name) = &e.username {
                key["username"] = name.clone().into();
            }
            keys.push(key);
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

/// The agent a commit made in this session is signed as.
#[derive(Debug)]
pub struct Signer {
    /// Its private key.
    pub key: PathBuf,
    /// Its soul folder (the session's home, not the folder being committed in).
    pub soul: PathBuf,
    /// Its name and email, when its `.lex/repo.yml` has both.
    pub who: Option<(String, String)>,
}

/// The agent whose session this is, when a harness places the session in a
/// soul that holds a bound key. None for a person at a terminal, for a
/// harness git-lex does not recognise, and for an agent with no key: those
/// commit as they always have, unsigned.
pub fn session_signer() -> Option<Signer> {
    let session = crate::gitlexd::session::session_soul().ok().filter(|s| s.from_harness())?;
    let key = bound_key(&session.root, &host_dir()?, now_secs())?;
    let yml = crate::RepoYml::load(&session.root);
    let who = match (yml.agent_name.filter(|n| !n.is_empty()), yml.agent_email.filter(|e| !e.is_empty())) {
        (Some(name), Some(email)) => Some((name, email)),
        _ => None,
    };
    Some(Signer { key, soul: session.root, who })
}

/// `git commit`, as the session's agent when there is one: signed with its
/// key, and with its name as author and committer when the name is known.
/// With no signer this is plain `git commit`. Every commit git-lex itself
/// makes starts here; the caller adds the message and the rest.
pub fn git_commit(signer: Option<&Signer>) -> std::process::Command {
    let mut git = std::process::Command::new("git");
    let Some(signer) = signer else {
        git.arg("commit");
        return git;
    };
    git.args(signing_args(&signer.key)).args(["commit", "-S"]);
    if let Some((name, email)) = &signer.who {
        git.arg(format!("--author={name} <{email}>"))
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email);
    }
    git
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
            adopt_username(root, host, now)?;
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
        username: None,
        squad: None,
        public_key: public_key(root)?,
        valid_after: now,
        valid_before: None,
    });
    list.save(host)?;
    adopt_username(root, host, now)?;
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
pub fn mint(root: &Path, host: &Path, now: u64) -> Result<String, String> {
    let genesis = first_commit(root)?;
    let key = signing_key(root)?;
    let mut id = [0u8; 16];
    getrandom::getrandom(&mut id).map_err(|e| format!("no random bytes from the system: {e}"))?;
    let iri = address_of(&HostList::load(host)?, &genesis);
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
pub fn token(root: &Path, host: &Path, now: u64) -> Result<String, String> {
    let path = token_path(root);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let kept = text.trim();
        if let Ok(claims) = check_own(root, host, kept, now)
            && claims.exp.saturating_sub(now) > TOKEN_LIFETIME_SECS / 2 {
                return Ok(kept.to_string());
            }
    }
    let fresh = mint(root, host, now)?;
    write_atomic(&path, format!("{fresh}\n").as_bytes(), 0o600)?;
    Ok(fresh)
}

/// Is this token signed by the key in this folder, in date, and for the
/// address the list gives this agent now?
fn check_own(root: &Path, host: &Path, token: &str, now: u64) -> Result<Claims, String> {
    let genesis = first_commit(root)?;
    let (username, squad) =
        HostList::load(host)?.current(&genesis, now).map_or((None, None), |e| (e.username.clone(), e.squad.clone()));
    let list = HostList {
        agents: vec![Entry { genesis, name: None, username, squad, public_key: public_key(root)?, valid_after: 0, valid_before: None }],
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
    if header.get("typ").and_then(|t| t.as_str()) == Some(GRANT_TYP) {
        return Err("this is a grant, not a token; a service presents it inside its own token".into());
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
    if claims.sub != entry.address() {
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

// ─── Services acting for someone ───────────────────────────────
//
// A service (a UI server, a worker) must not hold a person's or an agent's
// key. It gets its own key, and the one it acts for signs a GRANT: "this
// service, holding this key, may act for me, for these things, until this
// date". The service then signs short tokens with its own key and carries
// the grant inside them. A checker sees both names: who the request is for
// (`sub`) and which service is acting (`act.sub`, RFC 8693).
//
// Nothing is issued by a third party, as with agent tokens: the grant is
// signed by the grantor's listed key and names the service's public key
// (`cnf.jwk`, RFC 7800), so the service's key needs no list of its own.
// The machine's list of live grants (`grants.json`) is what ends one.

/// A grant lasts at least this long. Longer than any token may live, so a
/// grant can never pass a checker as a plain token.
pub const MIN_GRANT_SECS: u64 = 2 * 86_400;
/// And at most this long.
pub const MAX_GRANT_SECS: u64 = 366 * 86_400;
/// The `typ` of a grant, so it cannot be mistaken for a token.
pub const GRANT_TYP: &str = "grant+jwt";

/// `~/.lex/identity/grants.json` — the grants in force on this machine.
pub fn grants_path(host: &Path) -> PathBuf {
    host.join("grants.json")
}

/// `~/.lex/identity/services/<name>/` — a service's key and its grant.
pub fn service_dir(host: &Path, service: &str) -> PathBuf {
    host.join("services").join(service)
}

/// A service's name: lowercase letters, digits and dashes, starting with a
/// letter. It becomes a folder name and the `act.sub` of its tokens.
pub fn valid_service_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && name.len() <= 40
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrantRecord {
    pub jti: String,
    /// First commit of the identity that granted.
    pub grantor: String,
    pub service: String,
    /// What the service may do, as space-separated words (`pan:read pan:write`).
    pub scope: String,
    /// The service's public key, base64url, as in the grant's `cnf.jwk.x`.
    pub x: String,
    pub iat: u64,
    pub exp: u64,
    /// Unix seconds when it was ended early; absent while in force.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<u64>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct GrantList {
    #[serde(default)]
    pub grants: Vec<GrantRecord>,
}

impl GrantList {
    /// A missing file is no grants; a file that does not parse is an error.
    pub fn load(host: &Path) -> Result<GrantList, String> {
        let path = grants_path(host);
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{} does not parse: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(GrantList::default()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    pub fn save(&self, host: &Path) -> Result<(), String> {
        std::fs::create_dir_all(host).map_err(|e| format!("cannot create {}: {e}", host.display()))?;
        let json = serde_json::to_string_pretty(self).map_err(|e| e.to_string())? + "\n";
        write_atomic(&grants_path(host), json.as_bytes(), 0o644)
    }

    /// The grant in force for this service now, if any.
    pub fn in_force(&self, service: &str, now: u64) -> Option<&GrantRecord> {
        self.grants.iter().find(|g| g.service == service && g.ended.is_none() && g.exp > now)
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Jwk {
    pub kty: String,
    pub crv: String,
    pub x: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Confirmation {
    pub jwk: Jwk,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct GrantClaims {
    pub iss: String,
    pub sub: String,
    pub aud: String,
    pub iat: u64,
    pub exp: u64,
    pub jti: String,
    pub service: String,
    pub scope: String,
    pub cnf: Confirmation,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct Actor {
    pub sub: String,
}

/// The claims of a token a service signs when acting under a grant.
#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct ActingClaims {
    /// `service:<name>`: the signer.
    pub iss: String,
    /// Who the request is for: the grantor.
    pub sub: String,
    pub aud: String,
    pub iat: u64,
    pub exp: u64,
    pub jti: String,
    pub act: Actor,
    /// The grant, whole, as the grantor signed it.
    pub grant: String,
}

fn random_hex(bytes: usize) -> Result<String, String> {
    let mut id = vec![0u8; bytes];
    getrandom::getrandom(&mut id).map_err(|e| format!("no random bytes from the system: {e}"))?;
    Ok(id.iter().map(|b| format!("{b:02x}")).collect())
}

fn sign_jwt(header: serde_json::Value, claims: &impl Serialize, key: &SigningKey) -> Result<String, String> {
    let signed = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(serde_json::to_string(claims).map_err(|e| e.to_string())?)
    );
    let signature = key.sign(signed.as_bytes());
    Ok(format!("{signed}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes())))
}

/// The three parts of a compact JWT: header JSON, claims bytes, and the
/// signed text with its signature. Nothing here is trusted yet.
struct Parts<'t> {
    header: serde_json::Value,
    claims: Vec<u8>,
    signed: &'t str,
    signature: ed25519_dalek::Signature,
}

fn split_jwt<'t>(token: &'t str, what: &str) -> Result<Parts<'t>, String> {
    let mut parts = token.split('.');
    let (Some(h), Some(c), Some(s), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(format!("not a {what}: expected three parts separated by dots"));
    };
    let decode = |part: &str, piece: &str| {
        URL_SAFE_NO_PAD.decode(part).map_err(|e| format!("the {what}'s {piece} is not base64url: {e}"))
    };
    let header: serde_json::Value =
        serde_json::from_slice(&decode(h, "header")?).map_err(|e| format!("the {what}'s header is not JSON: {e}"))?;
    if header.get("alg").and_then(|a| a.as_str()) != Some("EdDSA") {
        return Err(format!("the {what} is not signed with EdDSA, the only signature type accepted"));
    }
    let signature = ed25519_dalek::Signature::from_slice(&decode(s, "signature")?)
        .map_err(|_| format!("the {what}'s signature is the wrong length"))?;
    Ok(Parts { header, claims: decode(c, "claims")?, signed: &token[..h.len() + 1 + c.len()], signature })
}

fn service_key_path(host: &Path, service: &str) -> PathBuf {
    service_dir(host, service).join("key")
}

fn service_grant_path(host: &Path, service: &str) -> PathBuf {
    service_dir(host, service).join("grant.jwt")
}

fn service_token_path(host: &Path, service: &str) -> PathBuf {
    service_dir(host, service).join("token")
}

fn service_signing_key(host: &Path, service: &str) -> Result<SigningKey, String> {
    let path = service_key_path(host, service);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("cannot read the service's key at {}: {e}", path.display()))?;
    let seed: [u8; 32] = URL_SAFE_NO_PAD
        .decode(text.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| format!("the service's key at {} is not a 32-byte Ed25519 key", path.display()))?;
    Ok(SigningKey::from_bytes(&seed))
}

/// Grant a service the right to act for the identity whose folder `root`
/// is. Makes the service's key the first time; a later grant keeps the key
/// and replaces the grant. A service acts for one identity: a grant to the
/// same name from a different one is refused while the first is in force.
pub fn grant(root: &Path, host: &Path, service: &str, scope: &str, days: u64, now: u64) -> Result<GrantRecord, String> {
    if !valid_service_name(service) {
        return Err(format!(
            "{service:?} is not a usable service name: lowercase letters, digits and dashes, starting with a letter"
        ));
    }
    let scope = scope.split_whitespace().collect::<Vec<_>>().join(" ");
    if scope.is_empty() {
        return Err("a grant must say what the service may do (for example: pan:read pan:write)".into());
    }
    let lifetime = days.saturating_mul(86_400);
    if !(MIN_GRANT_SECS..=MAX_GRANT_SECS).contains(&lifetime) {
        return Err(format!(
            "a grant lasts between {} and {} days",
            MIN_GRANT_SECS / 86_400,
            MAX_GRANT_SECS / 86_400
        ));
    }
    let genesis = first_commit(root)?;
    if bound_key(root, host, now).is_none() {
        return Err(format!("{} has no key on this machine's list, so it cannot sign a grant", root.display()));
    }
    let mut list = GrantList::load(host)?;
    if let Some(have) = list.in_force(service, now)
        && have.grantor != genesis
    {
        return Err(format!(
            "the service {service} already acts for a different identity ({}). End that grant first:\n    git lex identity revoke {service}",
            &have.grantor[..8.min(have.grantor.len())]
        ));
    }

    let dir = service_dir(host, service);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    if !service_key_path(host, service).is_file() {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).map_err(|e| format!("no random bytes from the system: {e}"))?;
        write_atomic(&service_key_path(host, service), format!("{}\n", URL_SAFE_NO_PAD.encode(seed)).as_bytes(), 0o600)?;
    }
    let x = URL_SAFE_NO_PAD.encode(service_signing_key(host, service)?.verifying_key().as_bytes());

    let iri = address_of(&HostList::load(host)?, &genesis);
    let claims = GrantClaims {
        iss: iri.clone(),
        sub: iri,
        aud: AUDIENCE.into(),
        iat: now,
        exp: now + lifetime,
        jti: random_hex(16)?,
        service: service.to_string(),
        scope: scope.clone(),
        cnf: Confirmation { jwk: Jwk { kty: "OKP".into(), crv: "Ed25519".into(), x: x.clone() } },
    };
    let header = serde_json::json!({ "alg": "EdDSA", "typ": GRANT_TYP, "kid": genesis });
    let signed = sign_jwt(header, &claims, &signing_key(root)?)?;

    for g in list.grants.iter_mut().filter(|g| g.service == service && g.ended.is_none()) {
        g.ended = Some(now);
    }
    let record = GrantRecord {
        jti: claims.jti.clone(),
        grantor: genesis,
        service: service.to_string(),
        scope,
        x,
        iat: claims.iat,
        exp: claims.exp,
        ended: None,
    };
    list.grants.push(record.clone());
    write_atomic(&service_grant_path(host, service), format!("{signed}\n").as_bytes(), 0o600)?;
    let _ = std::fs::remove_file(service_token_path(host, service));
    list.save(host)?;
    Ok(record)
}

/// End a service's grant now. Its tokens stop being accepted at once.
/// Returns the grant that was ended, if one was in force.
pub fn revoke(host: &Path, service: &str, now: u64) -> Result<Option<GrantRecord>, String> {
    let mut list = GrantList::load(host)?;
    let mut ended = None;
    for g in list.grants.iter_mut().filter(|g| g.service == service && g.ended.is_none()) {
        g.ended = Some(now);
        ended = Some(g.clone());
    }
    if ended.is_some() {
        list.save(host)?;
        let _ = std::fs::remove_file(service_grant_path(host, service));
        let _ = std::fs::remove_file(service_token_path(host, service));
    }
    Ok(ended)
}

/// Sign a new token for a service acting under its grant.
pub fn mint_service(host: &Path, service: &str, now: u64) -> Result<String, String> {
    let grant_path = service_grant_path(host, service);
    let grant = std::fs::read_to_string(&grant_path)
        .map_err(|_| format!("the service {service} has no grant on this machine ({} does not exist)", grant_path.display()))?;
    let grant = grant.trim().to_string();
    let terms: GrantClaims = serde_json::from_slice(&split_jwt(&grant, "grant")?.claims)
        .map_err(|e| format!("the grant at {} is incomplete: {e}", grant_path.display()))?;
    if terms.exp <= now {
        return Err(format!("the grant for {service} has expired; the one it acts for grants again"));
    }
    let claims = ActingClaims {
        iss: format!("service:{service}"),
        sub: terms.sub,
        aud: AUDIENCE.into(),
        iat: now,
        exp: (now + TOKEN_LIFETIME_SECS).min(terms.exp),
        jti: random_hex(16)?,
        act: Actor { sub: service.to_string() },
        grant,
    };
    sign_jwt(serde_json::json!({ "alg": "EdDSA", "typ": "JWT" }), &claims, &service_signing_key(host, service)?)
}

/// The service's token: the one on file while it is still accepted and has
/// more than half its life left, otherwise a new one.
pub fn service_token(host: &Path, service: &str, now: u64) -> Result<String, String> {
    if !valid_service_name(service) {
        return Err(format!("{service:?} is not a service name"));
    }
    let path = service_token_path(host, service);
    if let Ok(text) = std::fs::read_to_string(&path) {
        let kept = text.trim();
        if let Ok(caller) = verify_caller(kept, host, now)
            && caller.expires.saturating_sub(now) > TOKEN_LIFETIME_SECS / 2
        {
            return Ok(kept.to_string());
        }
    }
    let fresh = mint_service(host, service, now)?;
    // A token nobody would accept is not handed out.
    verify_caller(&fresh, host, now).map_err(|why| format!("the service {service} cannot act: {why}"))?;
    write_atomic(&path, format!("{fresh}\n").as_bytes(), 0o600)?;
    Ok(fresh)
}

/// Who a verified token speaks for.
#[derive(Debug, Clone, PartialEq)]
pub struct Caller {
    /// The address of the one the request is for.
    pub sub: String,
    /// That identity's first commit.
    pub genesis: String,
    /// The service acting for it, when the token came from one.
    pub actor: Option<String>,
    /// What the grant allows, when there is an actor. None for an identity
    /// acting for itself: nothing limits it here.
    pub scope: Option<Vec<String>>,
    /// When the token stops being accepted.
    pub expires: u64,
}

impl Caller {
    /// May this caller do `what` (for example `pan:write`)? An identity
    /// acting for itself may; a service may when its grant says so.
    pub fn allows(&self, what: &str) -> bool {
        self.scope.as_ref().is_none_or(|s| s.iter().any(|w| w == what))
    }
}

fn check_times(iat: u64, exp: u64, now: u64, min: u64, max: u64, what: &str) -> Result<(), String> {
    if iat > now + SKEW_SECS {
        return Err(format!("the {what} is dated in the future"));
    }
    if exp <= now {
        return Err(format!("the {what} has expired"));
    }
    if exp < iat || exp - iat > max || exp - iat < min {
        return Err(format!("the {what}'s lifetime is outside what is allowed"));
    }
    Ok(())
}

/// Check any token a service may be handed: an identity's own token, or a
/// service's token carrying a grant. This is what a checker calls.
///
/// For a service's token, in order, nothing inside it choosing what is
/// trusted:
/// 1. The grant is checked first, exactly as a token is (EdDSA fixed, `kid`
///    selects the listed key, signature, `sub` is that identity, `aud`),
///    with a grant's own lifetime limits and `typ`.
/// 2. The grant is in this machine's list of grants and has not been ended,
///    and the list agrees with it on grantor, service and key.
/// 3. The token's signature is verified with the key the grant names.
/// 4. The token is for the grantor (`sub`), by the granted service
///    (`act.sub`, `iss`).
/// 5. The token's own dates: not future, not expired, at most
///    `MAX_LIFETIME_SECS` long, and not outliving the grant.
/// 6. `aud` is `AUDIENCE`.
pub fn verify_caller(token: &str, host: &Path, now: u64) -> Result<Caller, String> {
    let list = HostList::load(host)?;
    let outer = split_jwt(token, "token")?;
    let peek: serde_json::Value =
        serde_json::from_slice(&outer.claims).map_err(|e| format!("the token's claims are not JSON: {e}"))?;
    if peek.get("grant").is_none() && peek.get("act").is_none() {
        if outer.header.get("typ").and_then(|t| t.as_str()) == Some(GRANT_TYP) {
            return Err("this is a grant, not a token; a service presents it inside its own token".into());
        }
        let (claims, entry) = verify(token, &list, now)?;
        return Ok(Caller { sub: claims.sub, genesis: entry.genesis.clone(), actor: None, scope: None, expires: claims.exp });
    }
    let claims: ActingClaims =
        serde_json::from_slice(&outer.claims).map_err(|e| format!("the service token's claims are incomplete: {e}"))?;

    // 1. The grant.
    let g = split_jwt(&claims.grant, "grant")?;
    if g.header.get("typ").and_then(|t| t.as_str()) != Some(GRANT_TYP) {
        return Err("what the token carries as its grant is not a grant".into());
    }
    let kid = g.header.get("kid").and_then(|k| k.as_str()).ok_or("the grant names no key (no kid)")?;
    let entry = list
        .agents
        .iter()
        .find(|e| e.genesis == kid && e.valid_before.is_none() && e.valid_at(now))
        .ok_or_else(|| format!("no identity on the list has the id {kid}"))?;
    VerifyingKey::from_bytes(&entry.key_bytes()?)
        .map_err(|e| format!("the listed key is not usable: {e}"))?
        .verify(g.signed.as_bytes(), &g.signature)
        .map_err(|_| "the grant's signature does not match the listed key of the one it says granted it".to_string())?;
    let terms: GrantClaims =
        serde_json::from_slice(&g.claims).map_err(|e| format!("the grant's claims are incomplete: {e}"))?;
    if terms.sub != entry.address() || terms.iss != terms.sub {
        return Err("the grant says it is from a different identity than the one whose key signed it".into());
    }
    check_times(terms.iat, terms.exp, now, MIN_GRANT_SECS, MAX_GRANT_SECS, "grant")?;
    if terms.aud != AUDIENCE {
        return Err(format!("the grant is for {:?}, not for {AUDIENCE:?}", terms.aud));
    }

    // 2. Still in force on this machine.
    let grants = GrantList::load(host)?;
    let record = grants
        .grants
        .iter()
        .find(|r| r.jti == terms.jti)
        .ok_or("the grant is not on this machine's list of grants")?;
    if record.ended.is_some() {
        return Err("the grant has been ended".into());
    }
    if record.grantor != entry.genesis || record.service != terms.service || record.x != terms.cnf.jwk.x {
        return Err("the grant does not match what this machine's list says was granted".into());
    }

    // 3. The service's signature, with the key the grant names.
    if terms.cnf.jwk.kty != "OKP" || terms.cnf.jwk.crv != "Ed25519" {
        return Err("the grant names a key that is not Ed25519".into());
    }
    let service_key: [u8; 32] = URL_SAFE_NO_PAD
        .decode(&terms.cnf.jwk.x)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("the grant names a key of the wrong length")?;
    VerifyingKey::from_bytes(&service_key)
        .map_err(|e| format!("the service's key is not usable: {e}"))?
        .verify(outer.signed.as_bytes(), &outer.signature)
        .map_err(|_| "the token is not signed by the service the grant names".to_string())?;

    // 4. For the grantor, by the granted service.
    if claims.sub != terms.sub {
        return Err("the token is for someone other than the one who granted".into());
    }
    if claims.act.sub != terms.service || claims.iss != format!("service:{}", terms.service) {
        return Err("the token names a different service than the grant".into());
    }

    // 5 and 6.
    check_times(claims.iat, claims.exp, now, 0, MAX_LIFETIME_SECS, "token")?;
    if claims.exp > terms.exp {
        return Err("the token outlives its grant".into());
    }
    if claims.aud != AUDIENCE {
        return Err(format!("the token is for {:?}, not for {AUDIENCE:?}", claims.aud));
    }
    Ok(Caller {
        sub: claims.sub,
        genesis: entry.genesis.clone(),
        actor: Some(terms.service),
        scope: Some(terms.scope.split_whitespace().map(str::to_string).collect()),
        expires: claims.exp,
    })
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
        let t = token(&repo, &host, NOW).unwrap();
        let (claims, entry) = verify(&t, &list, NOW).unwrap();
        assert_eq!(entry.genesis, genesis);
        assert_eq!(claims.sub, agent_iri(&genesis));
        assert_eq!(claims.exp - claims.iat, TOKEN_LIFETIME_SECS);
        // Kept while fresh, replaced past half its life.
        assert_eq!(token(&repo, &host, NOW + 60).unwrap(), t);
        assert_ne!(token(&repo, &host, NOW + TOKEN_LIFETIME_SECS / 2 + 1).unwrap(), t);
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
        let forged = mint(&clone, &host, NOW).unwrap();
        let list = HostList::load(&host).unwrap();
        assert!(verify(&forged, &list, NOW).unwrap_err().contains("does not match"));
    }

    #[test]
    fn verify_refuses_what_the_design_says_it_must() {
        let (repo, host) = soul("verify");
        bind(&repo, &host, NOW - 7200).unwrap();
        let list = HostList::load(&host).unwrap();
        let t = mint(&repo, &host, NOW).unwrap();
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
        let t = mint(&repo, &host, NOW).unwrap();
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

    /// A machine has one owner, it must be an identity with a key on the
    /// list, and a second different one is refused.
    #[test]
    fn a_machine_has_one_owner_and_it_must_be_bound() {
        let (repo, host) = soul("owner");
        assert_eq!(load_owner(&host).unwrap(), None);
        assert!(set_owner(&repo, &host, NOW).unwrap_err().contains("no key"));

        bind(&repo, &host, NOW).unwrap();
        assert_eq!(set_owner(&repo, &host, NOW + 1).unwrap(), OwnerSet::New);
        let genesis = first_commit(&repo).unwrap();
        let owner = load_owner(&host).unwrap().unwrap();
        assert_eq!(owner.owner, agent_iri(&genesis));
        assert_eq!(owner.genesis, genesis);
        assert_eq!(set_owner(&repo, &host, NOW + 2).unwrap(), OwnerSet::Already);

        // A second identity on the same machine cannot take the place.
        let other = repo.parent().unwrap().join("other");
        std::fs::create_dir_all(other.join(".lex")).unwrap();
        git(&other, &["init", "-q"]);
        git(&other, &["commit", "-q", "--allow-empty", "-m", "another first"]);
        bind(&other, &host, NOW + 3).unwrap();
        assert!(set_owner(&other, &host, NOW + 4).unwrap_err().contains("already has an owner"));
        assert_eq!(load_owner(&host).unwrap().unwrap().genesis, genesis);
    }

    /// A service acts for an identity through a grant, and every way of
    /// cheating that the design names is refused.
    #[test]
    fn a_service_acts_only_through_a_grant_in_force() {
        let (repo, host) = soul("grant");
        // An unbound identity cannot grant.
        assert!(grant(&repo, &host, "viewer", "pan:read", 90, NOW).unwrap_err().contains("no key"));
        bind(&repo, &host, NOW).unwrap();
        let genesis = first_commit(&repo).unwrap();

        assert!(grant(&repo, &host, "Bad Name", "pan:read", 90, NOW).is_err());
        assert!(grant(&repo, &host, "viewer", "  ", 90, NOW).is_err());
        assert!(grant(&repo, &host, "viewer", "pan:read", 1, NOW).is_err());
        assert!(mint_service(&host, "viewer", NOW).unwrap_err().contains("no grant"));

        let record = grant(&repo, &host, "viewer", "pan:read  pan:write", 90, NOW + 1).unwrap();
        assert_eq!(record.scope, "pan:read pan:write");
        let token = service_token(&host, "viewer", NOW + 2).unwrap();
        let caller = verify_caller(&token, &host, NOW + 3).unwrap();
        assert_eq!(caller.sub, agent_iri(&genesis));
        assert_eq!(caller.actor.as_deref(), Some("viewer"));
        assert!(caller.allows("pan:write") && !caller.allows("ravel:read"));
        // Kept while fresh.
        assert_eq!(service_token(&host, "viewer", NOW + 60).unwrap(), token);

        // The identity's own token still verifies, with no actor and no limit.
        let own = mint(&repo, &host, NOW + 2).unwrap();
        let me = verify_caller(&own, &host, NOW + 3).unwrap();
        assert_eq!((me.actor.clone(), me.scope.clone()), (None, None));
        assert!(me.allows("ravel:read"));

        // A checker that only knows plain tokens refuses a service's token,
        // and nobody accepts the grant itself as a token.
        let list = HostList::load(&host).unwrap();
        assert!(verify(&token, &list, NOW + 3).is_err());
        let grant_jwt = std::fs::read_to_string(service_grant_path(&host, "viewer")).unwrap();
        assert!(verify(grant_jwt.trim(), &list, NOW + 3).unwrap_err().contains("grant"));
        assert!(verify_caller(grant_jwt.trim(), &host, NOW + 3).unwrap_err().contains("grant"));

        // Expired token; token altered after signing.
        assert!(verify_caller(&token, &host, NOW + 2 + TOKEN_LIFETIME_SECS).unwrap_err().contains("expired"));
        let mut parts: Vec<String> = token.split('.').map(str::to_string).collect();
        let mut claims: serde_json::Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(&parts[1]).unwrap()).unwrap();
        claims["act"]["sub"] = "other".into();
        parts[1] = URL_SAFE_NO_PAD.encode(claims.to_string());
        assert!(verify_caller(&parts.join("."), &host, NOW + 3).unwrap_err().contains("not signed by the service"));

        // A different key in the service's folder: the grant does not name it.
        let key_file = service_key_path(&host, "viewer");
        let real = std::fs::read_to_string(&key_file).unwrap();
        std::fs::write(&key_file, URL_SAFE_NO_PAD.encode([7u8; 32])).unwrap();
        let forged = mint_service(&host, "viewer", NOW + 4).unwrap();
        assert!(verify_caller(&forged, &host, NOW + 5).unwrap_err().contains("not signed by the service"));
        std::fs::write(&key_file, real).unwrap();

        // A second identity cannot take over the service's name, and a grant
        // it signs for itself does not make the service speak for the first.
        let other = repo.parent().unwrap().join("other");
        std::fs::create_dir_all(other.join(".lex")).unwrap();
        git(&other, &["init", "-q"]);
        git(&other, &["commit", "-q", "--allow-empty", "-m", "another first"]);
        bind(&other, &host, NOW).unwrap();
        assert!(grant(&other, &host, "viewer", "pan:read", 90, NOW + 6).unwrap_err().contains("different identity"));

        // Granting again keeps the key, replaces the grant, and the old token dies.
        let again = grant(&repo, &host, "viewer", "pan:read", 30, NOW + 10).unwrap();
        assert_eq!(again.x, record.x);
        assert!(verify_caller(&token, &host, NOW + 11).unwrap_err().contains("ended"));
        let narrower = verify_caller(&service_token(&host, "viewer", NOW + 11).unwrap(), &host, NOW + 12).unwrap();
        assert!(narrower.allows("pan:read") && !narrower.allows("pan:write"));

        // Revoked: nothing the service signs is accepted, and it cannot mint.
        let live = service_token(&host, "viewer", NOW + 12).unwrap();
        assert!(revoke(&host, "viewer", NOW + 13).unwrap().is_some());
        assert!(verify_caller(&live, &host, NOW + 14).unwrap_err().contains("ended"));
        assert!(service_token(&host, "viewer", NOW + 14).is_err());
        assert!(revoke(&host, "viewer", NOW + 15).unwrap().is_none());

        // The grantor's key is retired: its grants stop with it.
        grant(&repo, &host, "viewer", "pan:read", 30, NOW + 20).unwrap();
        let t = service_token(&host, "viewer", NOW + 21).unwrap();
        retire(&genesis, &host, NOW + 22).unwrap();
        assert!(verify_caller(&t, &host, NOW + 23).is_err());
    }

    /// An agent that declares a username is addressed by it from then on:
    /// in its tokens, in the key list a checker reads, in the owner record
    /// and in grants. Before that it keeps the first-commit address, and a
    /// name is taken once, by one agent, for good.
    #[test]
    fn an_agent_is_addressed_by_its_username_once_it_declares_one() {
        let (repo, host) = soul("username");
        bind(&repo, &host, NOW).unwrap();
        let genesis = first_commit(&repo).unwrap();
        set_owner(&repo, &host, NOW).unwrap();
        grant(&repo, &host, "viewer", "pan:read", 90, NOW).unwrap();
        let old_token = token(&repo, &host, NOW).unwrap();
        let old_service = service_token(&host, "viewer", NOW + 1).unwrap();
        let sub_of = |host: &Path| -> serde_json::Value {
            serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(jwks_path(host)).unwrap()).unwrap()["keys"][0].clone()
        };
        // No username yet: the first-commit address, and the list says so.
        assert_eq!(adopt_username(&repo, &host, NOW).unwrap(), None);
        assert_eq!(sub_of(&host)["sub"], agent_iri(&genesis));
        assert!(sub_of(&host).get("username").is_none());
        assert_eq!(verify_caller(&old_token, &host, NOW + 1).unwrap().sub, agent_iri(&genesis));

        // A name that is not one usable word is refused and changes nothing.
        for bad in ["c0ffee", "tr1p-l3x", "W4R3Z", "pan"] {
            std::fs::write(repo.join("SOUL.md"), format!("---\nsoul.Soul.username: {bad}\n---\n")).unwrap();
            assert!(adopt_username(&repo, &host, NOW).is_err(), "{bad}");
        }
        assert_eq!(sub_of(&host)["sub"], agent_iri(&genesis));

        std::fs::write(repo.join("SOUL.md"), "---\nsoul.Soul.username: \"w4r3z\"\n---\n").unwrap();
        assert_eq!(adopt_username(&repo, &host, NOW + 2).unwrap().as_deref(), Some("w4r3z"));
        let new = "https://repolex.ai/soul/Soul/w4r3z";
        let key = sub_of(&host);
        assert_eq!((key["sub"].as_str(), key["username"].as_str(), key["kid"].as_str()), (Some(new), Some("w4r3z"), Some(genesis.as_str())));
        assert_eq!(load_owner(&host).unwrap().unwrap().owner, new);
        assert_eq!(load_owner(&host).unwrap().unwrap().genesis, genesis);

        // The old token and the old grant are refused; new ones carry the name.
        assert!(verify_caller(&old_token, &host, NOW + 3).unwrap_err().contains("different agent"));
        assert!(verify_caller(&old_service, &host, NOW + 3).unwrap_err().contains("different identity"));
        assert!(service_token(&host, "viewer", NOW + 3).is_err(), "a grant from before the switch is granted again");
        let fresh = token(&repo, &host, NOW + 3).unwrap();
        assert_ne!(fresh, old_token);
        let me = verify_caller(&fresh, &host, NOW + 4).unwrap();
        assert_eq!((me.sub.as_str(), me.genesis.as_str()), (new, genesis.as_str()));
        grant(&repo, &host, "viewer", "pan:read", 90, NOW + 5).unwrap();
        let acting = verify_caller(&service_token(&host, "viewer", NOW + 6).unwrap(), &host, NOW + 7).unwrap();
        assert_eq!((acting.sub.as_str(), acting.actor.as_deref()), (new, Some("viewer")));

        // With the squad its store belongs to, the address is the one that
        // store gives its own Soul.
        let both = Entry { squad: Some("7r1pl3f0rc3".into()), ..HostList::load(&host).unwrap().agents[0].clone() };
        assert_eq!(both.address(), "https://repolex.ai/7r1pl3f0rc3/w4r3z/soul/Soul/w4r3z");

        // The list's word is final: a different name in SOUL.md changes nothing.
        std::fs::write(repo.join("SOUL.md"), "---\nsoul.Soul.username: warez\n---\n").unwrap();
        assert_eq!(adopt_username(&repo, &host, NOW + 8).unwrap().as_deref(), Some("w4r3z"));

        // Another agent cannot take the same name, and keeps its old address.
        let other = repo.parent().unwrap().join("other");
        std::fs::create_dir_all(other.join(".lex")).unwrap();
        git(&other, &["init", "-q"]);
        git(&other, &["commit", "-q", "--allow-empty", "-m", "another first"]);
        std::fs::write(other.join("SOUL.md"), "---\nsoul.Soul.username: w4r3z\n---\n").unwrap();
        assert!(bind(&other, &host, NOW + 9).unwrap_err().contains("already belongs to another agent"));
        let theirs = verify_caller(&token(&other, &host, NOW + 10).unwrap(), &host, NOW + 11).unwrap();
        assert_eq!(theirs.sub, agent_iri(&first_commit(&other).unwrap()));

        // A copy of the repo, holding no listed key, cannot name the agent.
        let (lone, lone_host) = soul("username-copy");
        bind(&lone, &lone_host, NOW).unwrap();
        std::fs::remove_file(key_path(&lone)).unwrap();
        std::fs::write(lone.join("SOUL.md"), "---\nsoul.Soul.username: thief\n---\n").unwrap();
        assert_eq!(adopt_username(&lone, &lone_host, NOW + 1).unwrap(), None);
    }

    #[test]
    fn ssh_time_is_utc_calendar_time() {
        assert_eq!(ssh_time(0), "19700101000000Z");
        assert_eq!(ssh_time(1_794_700_000), "20261114234640Z");
        assert_eq!(ssh_time(951_782_400), "20000229000000Z");
    }
}
