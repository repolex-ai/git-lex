//! `git lex identity` — the agent's key and token (src/identity.rs).

use git_lex::identity::{self, Bound};
use git_lex::{Failure, Outcome, require_git_root};

fn host() -> Outcome<std::path::PathBuf> {
    identity::host_dir().ok_or_else(|| Failure::new("fatal: no home directory, so no ~/.lex/identity to keep the list of agents' keys in"))
}

/// The soul this session belongs to, by the same rule `git lex query` uses.
fn session_root() -> Outcome<std::path::PathBuf> {
    git_lex::gitlexd::session::session_soul().map(|s| s.root).map_err(|e| Failure::new(format!("fatal: {e}")))
}

/// Refuse inside an agent's harness session: what follows is a person's act.
fn person_only(what: &str) -> Outcome {
    if let Ok((home, how)) = git_lex::gitlexd::session::session_home()
        && how != git_lex::gitlexd::session::TERMINAL
    {
        return Err(Failure::new(format!(
            "fatal: this is {how} (started in {}). {what} is for a person to do, from a plain terminal or their own tools.",
            home.display()
        )));
    }
    Ok(())
}

/// The folder of the person this machine belongs to: the owner record names
/// the identity, the machine's list of repos says where it lives.
fn owner_root() -> Outcome<std::path::PathBuf> {
    person_only("Using the owner's token")?;
    let host = host()?;
    let Some(owner) = identity::load_owner(&host).map_err(Failure::new)? else {
        return Err(Failure::new(format!(
            "No owner is recorded on this machine ({} does not exist). The person it belongs to runs, in their own repo:\n    git lex identity owner",
            identity::owner_path(&host).display()
        )));
    };
    git_lex::registry_entries()
        .into_iter()
        .find(|e| e.genesis.as_deref() == Some(owner.genesis.as_str()))
        .map(|e| std::path::PathBuf::from(e.path))
        .filter(|p| p.is_dir())
        .ok_or_else(|| Failure::new(format!("The owner's repo ({}) is not in this machine's list of repos, so its key cannot be found.", owner.genesis)))
}

/// The address the machine's list gives the agent with this first commit.
fn address(host: &std::path::Path, genesis: &str) -> String {
    identity::HostList::load(host).map_or_else(|_| identity::agent_iri(genesis), |list| identity::address_of(&list, genesis))
}

/// Take the username SOUL.md declares onto the list if the agent has none
/// there yet. A username that cannot be taken is said, and is no reason to
/// refuse the token: the agent keeps the address it had.
fn adopt_username(root: &std::path::Path, host: &std::path::Path) {
    if let Err(why) = identity::adopt_username(root, host, identity::now_secs()) {
        eprintln!("warning: {why}");
    }
}

fn no_key(root: &std::path::Path) -> Failure {
    Failure::new(format!(
        "This agent has no key yet ({} holds none).\n\
         A person gives it one, once, by running this in the agent's folder:\n\
         \x20   git lex identity bind",
        identity::identity_dir(root).display()
    ))
}

/// `git lex identity bind`: say that this folder is this agent.
pub fn cmd_bind() -> Outcome {
    let root = require_git_root()?;
    git_lex::require_lex_repo(&root)?;
    if !git_lex::soul_md::soul_kit_installed(&root) {
        return Err(Failure::new(
            "fatal: only an agent's soul repo gets a key. This repository does not have the soul kit, \
             so it is a store that agents work in, not an agent.",
        ));
    }
    let host = host()?;
    let genesis = identity::first_commit(&root).map_err(Failure::new)?;
    match identity::bind(&root, &host, identity::now_secs()).map_err(Failure::new)? {
        Bound::New => {
            println!("Bound: {} is agent {}.", root.display(), &genesis[..8]);
            println!("  private key  {}  (never committed; back it up with the folder)", identity::key_path(&root).display());
            println!("  listed in    {}", identity::list_path(&host).display());
            println!("From now on `git lex save` in this agent's sessions signs its commits.");
        }
        Bound::Already => println!("Already bound: {} is agent {}. Nothing changed.", root.display(), &genesis[..8]),
    }
    Ok(())
}

/// `git lex identity token`: print the session agent's token, nothing else.
pub fn cmd_token(owner: bool, service: Option<&str>) -> Outcome {
    if let Some(service) = service {
        // A service's token is the service's to ask for, whoever started it:
        // that is what the grant is for.
        println!("{}", identity::service_token(&host()?, service, identity::now_secs()).map_err(Failure::new)?);
        return Ok(());
    }
    let root = if owner { owner_root()? } else { session_root()? };
    if !identity::has_key(&root) {
        return Err(no_key(&root));
    }
    let host = host()?;
    adopt_username(&root, &host);
    println!("{}", identity::token(&root, &host, identity::now_secs()).map_err(Failure::new)?);
    Ok(())
}

/// `git lex identity show`: who this session is, for a person.
pub fn cmd_show() -> Outcome {
    let root = session_root()?;
    if !identity::has_key(&root) {
        return Err(no_key(&root));
    }
    let now = identity::now_secs();
    let host = host()?;
    let genesis = identity::first_commit(&root).map_err(Failure::new)?;
    adopt_username(&root, &host);
    let token = identity::token(&root, &host, now).map_err(Failure::new)?;
    let list = identity::HostList::load(&host).map_err(Failure::new)?;
    println!("agent    {}", identity::address_of(&list, &genesis));
    println!("id       {genesis}");
    println!("folder   {}", root.display());
    match identity::load_owner(&host).map_err(Failure::new)? {
        Some(o) if o.genesis == genesis => println!("owner    yes, this is the person this machine belongs to"),
        Some(o) => println!("owner    {}", o.owner),
        None => println!("owner    none recorded on this machine"),
    }
    match identity::verify(&token, &list, now) {
        Ok((claims, _)) => {
            let left = claims.exp.saturating_sub(now);
            println!("key      on the list at {}", identity::list_path(&host).display());
            println!("token    good for another {} h {} min  ({})", left / 3600, left % 3600 / 60, identity::token_path(&root).display());
        }
        Err(why) => {
            println!("key      NOT accepted against {}: {why}", identity::list_path(&host).display());
            println!("token    services will refuse it");
        }
    }
    Ok(())
}

/// `git lex identity owner`: say that this folder's identity is the person
/// this machine belongs to.
pub fn cmd_owner() -> Outcome {
    // An agent does not get to name the owner, whatever folder its shell is in.
    person_only("Naming the owner of this machine")?;
    let root = require_git_root()?;
    git_lex::require_lex_repo(&root)?;
    let host = host()?;
    let genesis = identity::first_commit(&root).map_err(Failure::new)?;
    match identity::set_owner(&root, &host, identity::now_secs()).map_err(Failure::new)? {
        identity::OwnerSet::New => {
            println!("Owner: this machine belongs to {} ({}).", address(&host, &genesis), root.display());
            println!("  recorded in  {}", identity::owner_path(&host).display());
        }
        identity::OwnerSet::Already => println!("Already the owner: {} ({}). Nothing changed.", &genesis[..8], root.display()),
    }
    Ok(())
}

/// The identity a grant is signed as: inside an agent's session, that
/// agent, whatever folder the shell is in; from a plain terminal, the repo
/// the person is standing in.
fn grantor_root() -> Outcome<std::path::PathBuf> {
    match git_lex::gitlexd::session::session_soul() {
        Ok(s) if s.from_harness() => Ok(s.root),
        _ => require_git_root(),
    }
}

fn days_left(exp: u64, now: u64) -> u64 {
    exp.saturating_sub(now) / 86_400
}

/// `git lex identity grant <service> --scope "..."`
pub fn cmd_grant(service: &str, scope: &str, days: u64) -> Outcome {
    let root = grantor_root()?;
    let host = host()?;
    let now = identity::now_secs();
    adopt_username(&root, &host);
    let record = identity::grant(&root, &host, service, scope, days, now).map_err(Failure::new)?;
    println!("Granted: {service} may act for {} ({}).", address(&host, &record.grantor), root.display());
    println!("  it may       {}", record.scope);
    println!("  for          {} days", days_left(record.exp, now));
    println!("  its key      {}", identity::service_dir(&host, service).join("key").display());
    println!("  listed in    {}", identity::grants_path(&host).display());
    println!("The service gets its token with:  git lex identity token --service {service}");
    println!("End it at any time with:          git lex identity revoke {service}");
    Ok(())
}

/// `git lex identity grants`
pub fn cmd_grants() -> Outcome {
    let host = host()?;
    let now = identity::now_secs();
    let list = identity::GrantList::load(&host).map_err(Failure::new)?;
    let live: Vec<_> = list.grants.iter().filter(|g| g.ended.is_none() && g.exp > now).collect();
    if live.is_empty() {
        println!("No grants are in force on this machine.");
        return Ok(());
    }
    for g in live {
        println!("{}  acts for {}  may: {}  ({} days left)", g.service, address(&host, &g.grantor), g.scope, days_left(g.exp, now));
    }
    Ok(())
}

/// `git lex identity revoke <service>`
pub fn cmd_revoke(service: &str) -> Outcome {
    let host = host()?;
    let now = identity::now_secs();
    // Inside an agent's session, only that agent's own grants are its to end.
    if let Ok(s) = git_lex::gitlexd::session::session_soul()
        && s.from_harness()
        && let Some(g) = identity::GrantList::load(&host).map_err(Failure::new)?.in_force(service, now)
        && g.grantor != s.genesis
    {
        return Err(Failure::new(format!(
            "fatal: {service} acts for {}, not for this session's agent. Its grant is theirs to end.",
            address(&host, &g.grantor)
        )));
    }
    match identity::revoke(&host, service, now).map_err(Failure::new)? {
        Some(g) => println!("Ended: {service} no longer acts for {}. Its tokens are refused from now.", address(&host, &g.grantor)),
        None => println!("No grant is in force for {service}. Nothing changed."),
    }
    Ok(())
}

/// `git lex identity check`: a token on standard input, who it speaks for
/// on standard output as one line of JSON.
pub fn cmd_check() -> Outcome {
    let mut token = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut token).map_err(|e| Failure::new(format!("cannot read standard input: {e}")))?;
    let caller = identity::verify_caller(token.trim(), &host()?, identity::now_secs())
        .map_err(|why| Failure::new(format!("refused: {why}")))?;
    println!(
        "{}",
        serde_json::json!({ "sub": caller.sub, "actor": caller.actor, "scope": caller.scope, "expires": caller.expires })
    );
    Ok(())
}

/// `git lex identity retire <id>`: end an agent's current key.
pub fn cmd_retire(genesis: &str) -> Outcome {
    let host = host()?;
    if identity::retire(genesis, &host, identity::now_secs()).map_err(Failure::new)? {
        println!("Ended the current key of agent {genesis}. What it signed before still verifies; nothing new will.");
        println!("To give the agent a new key, remove the old key file from its folder and run `git lex identity bind` there.");
        Ok(())
    } else {
        Err(Failure::new(format!("No current key is listed for an agent with the id {genesis} (the full first-commit hash is needed).")))
    }
}
