//! gitlexd — the git-lex query and sync service.
//!
//!   gitlexd          run in the foreground; refuses if one is already up
//!   gitlexd start    stop every gitlexd on this machine, then run here
//!   gitlexd restart  the same as start
//!   gitlexd stop     stop every gitlexd on this machine and exit
//!   gitlexd status   say whether one is running and what it holds
//!
//! No flags, no config file. The souls come from `~/.lex/repos.json`, which
//! every git-lex run writes; the port is fixed (git_lex::gitlexd::PORT); the
//! log is `~/.lex/logs/gitlexd.log`. Process control follows pand: a
//! terminal starts the daemon and owns it, there is no launchd job
//! (goodlux removed pand's on 2026-09-04 after two ran at once), and start
//! and stop leave exactly one or zero running.
//!
//! `gitlexd worker <path>` is the daemon's own sync worker (one process per
//! sync, working directory set to the soul); it is not for typing by hand.

use git_lex::gitlexd;
use std::sync::Arc;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] => {
            refuse_if_already_running();
            serve()
        }
        ["start"] | ["restart"] => {
            stop_all();
            serve()
        }
        ["stop"] => {
            stop_all();
            0
        }
        ["status"] => status(),
        ["worker", path] => worker(path),
        _ => {
            eprintln!("usage: gitlexd | gitlexd start | gitlexd restart | gitlexd stop | gitlexd status");
            2
        }
    };
    std::process::exit(code);
}

/// The sync worker: run the engine in this process with the soul as the
/// working directory. Its output and exit status go back to the daemon.
fn worker(path: &str) -> i32 {
    if let Err(e) = std::env::set_current_dir(path) {
        eprintln!("cannot enter {path}: {e}");
        return 1;
    }
    git_lex::sync::cmd_sync();
    0
}

/// Every gitlexd process on this machine except this one and its workers.
fn other_daemons() -> Vec<u32> {
    let me = std::process::id();
    let out = std::process::Command::new("pgrep")
        .args(["-x", "gitlexd"])
        .output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().parse::<u32>().ok())
        .filter(|p| *p != me)
        .filter(|p| !is_worker(*p))
        .collect()
}

/// A worker's command line is `gitlexd worker <path>`.
fn is_worker(pid: u32) -> bool {
    std::process::Command::new("ps")
        .args(["-o", "command=", "-p", &pid.to_string()])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).contains(" worker "))
        .unwrap_or(false)
}

fn refuse_if_already_running() {
    let pids = other_daemons();
    if pids.is_empty() {
        return;
    }
    let who = pids.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ");
    eprintln!("gitlexd is already running (pid {who}).");
    match gitlexd::client::health() {
        Some(h) => eprintln!(
            "  it is answering on {}, version {}, up {}.",
            gitlexd::base_url(),
            h["version"].as_str().unwrap_or("?"),
            uptime(h["uptime_secs"].as_u64().unwrap_or(0))
        ),
        None => eprintln!("  it is not answering on {}, so it may be wedged.", gitlexd::base_url()),
    }
    eprintln!("  gitlexd restart   stop it and run this build here");
    eprintln!("  gitlexd stop      stop it and leave nothing running");
    eprintln!("  gitlexd status    what it holds right now");
    std::process::exit(1);
}

fn uptime(secs: u64) -> String {
    format!("{}h {:02}m {:02}s", secs / 3600, (secs % 3600) / 60, secs % 60)
}

fn serve() -> i32 {
    let daemon = match gitlexd::daemon::Daemon::open() {
        Ok(d) => Arc::new(d),
        Err(e) => {
            eprintln!("gitlexd: {e}");
            return 1;
        }
    };
    if let Some(p) = daemon.log_path() {
        daemon.log(&format!("gitlexd {} starting; log: {}", env!("CARGO_PKG_VERSION"), p.display()));
    }
    for s in &daemon.souls {
        let st = s.status();
        daemon.log(&format!(
            "{} {} {}{}",
            s.short(),
            s.path.display(),
            match &st.synced_to {
                Some(sha) => format!("synced to {}", &sha[..8.min(sha.len())]),
                None => "no store yet".to_string(),
            },
            st.open_error.as_deref().map(|e| format!(" (open failed: {e})")).unwrap_or_default()
        ));
    }
    let rt = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("gitlexd: cannot start the runtime: {e}");
            return 1;
        }
    };
    match rt.block_on(gitlexd::http::serve(daemon)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("gitlexd: {e}");
            1
        }
    }
}

fn status() -> i32 {
    let pids = other_daemons();
    match gitlexd::client::health() {
        Some(h) => {
            println!(
                "gitlexd is RUNNING — pid {}, up {}, version {}",
                h["pid"].as_u64().unwrap_or(0),
                uptime(h["uptime_secs"].as_u64().unwrap_or(0)),
                h["version"].as_str().unwrap_or("?")
            );
            println!("  serving {} for {} soul(s), {} syncing now", gitlexd::base_url(), h["souls"], h["syncing"]);
            if let Ok(list) = gitlexd::client::souls()
                && let Some(souls) = list["souls"].as_array() {
                    for s in souls {
                        let g = s["genesis"].as_str().unwrap_or("?");
                        let synced = s["synced_to"].as_str().map(|x| x[..8.min(x.len())].to_string());
                        let state = if s["syncing"].as_bool() == Some(true) {
                            "syncing".to_string()
                        } else if let Some(e) = s["open_error"].as_str() {
                            format!("store not open: {e}")
                        } else if let Some(e) = s["last_error"].as_str() {
                            format!("last sync FAILED: {e}")
                        } else {
                            match synced {
                                Some(x) => format!("synced to {x}"),
                                None => "no store yet".to_string(),
                            }
                        };
                        println!("  {}  {}  {}", &g[..8.min(g.len())], s["path"].as_str().unwrap_or("?"), state);
                    }
                }
            if let Some(p) = gitlexd::log_path() {
                println!("  log: {}", p.display());
            }
            if pids.len() > 1 {
                println!("  WARNING: {} gitlexd processes exist ({:?}); `gitlexd stop` stops them all", pids.len(), pids);
            }
            0
        }
        None if pids.is_empty() => {
            println!(
                "gitlexd is NOT running (nothing answers on {} and no gitlexd process exists). Start it with: gitlexd start",
                gitlexd::base_url()
            );
            1
        }
        None => {
            println!(
                "gitlexd is NOT answering on {} but a gitlexd process exists (pid {:?}).\n  `gitlexd stop` stops it; then `gitlexd start`",
                gitlexd::base_url(),
                pids
            );
            1
        }
    }
}

/// Stop every other gitlexd on this machine, in plain words.
fn stop_all() {
    use std::process::Command;
    let first = other_daemons();
    if first.is_empty() {
        eprintln!("gitlexd stop: no other gitlexd process was running");
        return;
    }
    for p in &first {
        let _ = Command::new("kill").args(["-TERM", &p.to_string()]).status();
    }
    eprintln!("gitlexd stop: sent SIGTERM to gitlexd process(es) {first:?}");
    for _ in 0..50 {
        if other_daemons().is_empty() {
            eprintln!("gitlexd stop: all gitlexd processes have exited");
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let left = other_daemons();
    for p in &left {
        let _ = Command::new("kill").args(["-KILL", &p.to_string()]).status();
    }
    eprintln!("gitlexd stop: {left:?} did not exit in 5 s; sent SIGKILL");
    std::thread::sleep(std::time::Duration::from_millis(200));
    let still = other_daemons();
    if still.is_empty() {
        eprintln!("gitlexd stop: all gitlexd processes have exited");
    } else {
        eprintln!("gitlexd stop: STILL RUNNING after SIGKILL: {still:?}");
    }
}
