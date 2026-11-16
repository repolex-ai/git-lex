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
pub fn cmd_token() -> Outcome {
    let root = session_root()?;
    if !identity::has_key(&root) {
        return Err(no_key(&root));
    }
    println!("{}", identity::token(&root, identity::now_secs()).map_err(Failure::new)?);
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
    let token = identity::token(&root, now).map_err(Failure::new)?;
    let list = identity::HostList::load(&host).map_err(Failure::new)?;
    println!("agent    {}", identity::agent_iri(&genesis));
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
    // A person's act. Inside an agent's session the answer is no, whatever
    // folder the shell is in: an agent does not get to name the owner.
    if let Ok((home, how)) = git_lex::gitlexd::session::session_home()
        && how != git_lex::gitlexd::session::TERMINAL
    {
        return Err(Failure::new(format!(
            "fatal: this is {how} (started in {}). Naming the owner of this machine is for a person to do, \
             from a plain terminal, in their own repo:\n    git lex identity owner",
            home.display()
        )));
    }
    let root = require_git_root()?;
    git_lex::require_lex_repo(&root)?;
    let host = host()?;
    let genesis = identity::first_commit(&root).map_err(Failure::new)?;
    match identity::set_owner(&root, &host, identity::now_secs()).map_err(Failure::new)? {
        identity::OwnerSet::New => {
            println!("Owner: this machine belongs to {} ({}).", identity::agent_iri(&genesis), root.display());
            println!("  recorded in  {}", identity::owner_path(&host).display());
        }
        identity::OwnerSet::Already => println!("Already the owner: {} ({}). Nothing changed.", &genesis[..8], root.display()),
    }
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
