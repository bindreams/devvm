use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};

use crate::ErrorKind;
use crate::access::ssh_config::{SshConfigDir, SshConfigEntry, glob_literal, ssh_path};
use crate::host::Host;
use crate::instance::name::InstanceName;
use crate::instance::record::InstanceId;
use crate::store::lock::{FileLock, Mode};
use crate::test_support::fixtures::{ChannelProgress, unicode_home};
use crate::test_support::host::TestHost;

const ID: &str = "0123456789abcdef0123456789abcdef";
const KEY: &str = "AAAAC3NzaC1lZDI1NTE5AAAAIPlaceholderPlaceholderPlaceholderPlace";

fn host_public() -> String {
    format!("ssh-ed25519 {KEY} a comment")
}

fn entry(home: &Path, name: &str, port: u16) -> SshConfigEntry {
    SshConfigEntry {
        name: InstanceName::parse(name).unwrap(),
        id: ID.parse::<InstanceId>().unwrap(),
        port,
        identity: home.join("keys").join("client"),
        known_hosts: home.join("keys").join("known_hosts"),
    }
}

/// The path as ssh_config spells it: with `/` separators. A backslash in a Unix path is an ordinary character.
fn slashed(path: &Path) -> String {
    let text = path.to_str().unwrap();
    if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.to_owned()
    }
}

fn conf(dir: &SshConfigDir, name: &str) -> PathBuf {
    dir.path().join(format!("{name}.conf"))
}

#[skuld::test]
fn entry_quotes_values_with_spaces_and_uses_forward_slashes(#[fixture(unicode_home)] home: &Path) {
    let dir = SshConfigDir::new(home.join("ssh"));
    dir.write_entry(&entry(home, "web", 2222), &host_public()).unwrap();
    let identity = slashed(&home.join("keys").join("client"));
    let known_hosts = slashed(&home.join("keys").join("known_hosts"));
    let want = format!(
        "# Written by devvm; rewritten whenever the instance starts.\n\
         Host web\n\
         \x20   HostName 127.0.0.1\n\
         \x20   Port 2222\n\
         \x20   User user\n\
         \x20   IdentityFile \"{identity}\"\n\
         \x20   IdentitiesOnly yes\n\
         \x20   UserKnownHostsFile \"{known_hosts}\"\n\
         \x20   HostKeyAlias devvm-{ID}\n\
         \x20   StrictHostKeyChecking yes\n"
    );
    assert_eq!(std::fs::read_to_string(conf(&dir, "web")).unwrap(), want);
    assert_eq!(
        std::fs::read_to_string(home.join("keys").join("known_hosts")).unwrap(),
        format!("devvm-{ID} ssh-ed25519 {KEY}\n"),
        "the key comment is not part of the known_hosts line"
    );
}

#[skuld::test]
fn a_quote_in_a_value_is_escaped(#[fixture(unicode_home)] home: &Path) {
    let dir = SshConfigDir::new(home.join("ssh"));
    let mut e = entry(home, "web", 22);
    e.identity = home.join("it's \"q\"").join("client");
    dir.write_entry(&e, &host_public()).unwrap();
    let text = std::fs::read_to_string(conf(&dir, "web")).unwrap();
    let want = format!("IdentityFile \"{}\"", slashed(&e.identity).replace('"', "\\\""));
    assert!(text.contains(&want), "{text}");
}

#[skuld::test]
fn port_change_rewrites_only_the_conf_not_known_hosts(#[fixture(unicode_home)] home: &Path) {
    let dir = SshConfigDir::new(home.join("ssh"));
    let e = entry(home, "web", 2222);
    dir.write_entry(&e, &host_public()).unwrap();
    // A second name for the same file: if the file were replaced, writing through the link would not reach it.
    let link = home.join("known_hosts.link");
    std::fs::hard_link(&e.known_hosts, &link).unwrap();
    dir.write_entry(&entry(home, "web", 3333), &host_public()).unwrap();
    assert!(
        std::fs::read_to_string(conf(&dir, "web"))
            .unwrap()
            .contains("Port 3333")
    );
    std::fs::write(&link, "marker\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(&e.known_hosts).unwrap(),
        "marker\n",
        "known_hosts was replaced"
    );
}

#[skuld::test]
fn a_changed_host_key_rewrites_known_hosts(#[fixture(unicode_home)] home: &Path) {
    let dir = SshConfigDir::new(home.join("ssh"));
    let e = entry(home, "web", 2222);
    dir.write_entry(&e, &host_public()).unwrap();
    dir.write_entry(&e, "ssh-ed25519 AAAAOther").unwrap();
    assert_eq!(
        std::fs::read_to_string(&e.known_hosts).unwrap(),
        format!("devvm-{ID} ssh-ed25519 AAAAOther\n")
    );
}

#[skuld::test]
fn remove_entry_removes_the_conf_and_is_idempotent(#[fixture(unicode_home)] home: &Path) {
    let dir = SshConfigDir::new(home.join("ssh"));
    dir.write_entry(&entry(home, "web", 22), &host_public()).unwrap();
    dir.remove_entry(&InstanceName::parse("web").unwrap()).unwrap();
    assert!(!conf(&dir, "web").exists());
    dir.remove_entry(&InstanceName::parse("web").unwrap()).unwrap();
}

#[skuld::test]
fn concurrent_write_and_remove_leave_every_live_entry_whole(#[fixture(unicode_home)] home: &Path) {
    const THREADS: usize = 8;
    const ROUNDS: usize = 25;
    let home = home.to_owned();
    let barrier = Arc::new(Barrier::new(THREADS));
    let workers: Vec<_> = (0..THREADS)
        .map(|i| {
            let (home, barrier) = (home.clone(), barrier.clone());
            std::thread::spawn(move || {
                let dir = SshConfigDir::new(home.join("ssh"));
                let mut e = entry(&home, &format!("vm-{i}"), 1000);
                e.known_hosts = home.join(format!("known-{i}"));
                barrier.wait();
                for round in 0..ROUNDS {
                    e.port = 1000 + round as u16;
                    dir.write_entry(&e, &host_public()).unwrap();
                    // Even threads end with their entry removed, odd ones with it present.
                    if i % 2 == 0 {
                        dir.remove_entry(&e.name).unwrap();
                    }
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().unwrap();
    }
    let dir = SshConfigDir::new(home.join("ssh"));
    for i in 0..THREADS {
        let name = format!("vm-{i}");
        let path = conf(&dir, &name);
        if i % 2 == 0 {
            assert!(!path.exists(), "{name}");
        } else {
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.starts_with("# Written by devvm"), "{text}");
            assert!(text.contains(&format!("Host {name}\n")), "{text}");
            assert!(text.contains(&format!("Port {}\n", 1000 + ROUNDS - 1)), "{text}");
            assert!(text.ends_with("StrictHostKeyChecking yes\n"), "{text}");
        }
    }
    let files: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        files.iter().all(|f| f == ".lock" || f.ends_with(".conf")),
        "no temporary file is left: {files:?}"
    );
}

#[skuld::test]
fn sweep_removes_only_confs_of_gone_instances(#[fixture(unicode_home)] home: &Path) {
    let dir = SshConfigDir::new(home.join("ssh"));
    for (name, port) in [("kept", 1), ("gone", 2)] {
        dir.write_entry(&entry(home, name, port), &host_public()).unwrap();
    }
    dir.ensure_top_level().unwrap();
    std::fs::write(
        dir.path().join("not_an_instance.conf"),
        "user's own
",
    )
    .unwrap();
    let warnings = dir.sweep(&|name| name.as_str() == "kept");
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(conf(&dir, "kept").exists());
    assert!(!conf(&dir, "gone").exists());
    assert!(
        dir.path().join("config").exists(),
        "the top-level config is not an instance"
    );
    assert!(
        dir.path().join("not_an_instance.conf").exists(),
        "a file that is not ours is left alone"
    );
}

#[skuld::test]
fn a_sweep_holds_the_lock_while_it_asks_whether_an_instance_lives(#[fixture(unicode_home)] home: &Path) {
    // A sweeper that read "no instance" must not delete the conf of an instance placed a moment later: the whole
    // sweep, the question included, is under the lock the writers take.
    let dir = SshConfigDir::new(home.join("ssh"));
    dir.write_entry(&entry(home, "reused", 3), &host_public()).unwrap();
    let held = std::cell::Cell::new(None);
    dir.sweep(&|_| {
        let mut other = FileLock::open(&dir.path().join(".lock")).unwrap();
        held.set(Some(!other.try_lock(Mode::Exclusive).unwrap()));
        true
    });
    assert_eq!(held.get(), Some(true), "the lock was free while the sweep asked");
}

/// Runs `operation` while another holder has the lock: it must say it waits and change nothing until the lock is free.
/// Returns a directory handle for looking at the result.
fn waits_for_the_lock(home: &Path, operation: impl FnOnce(SshConfigDir) + Send) -> SshConfigDir {
    let (progress, messages) = ChannelProgress::new();
    let operated = SshConfigDir::new(home.join("ssh")).with_progress(progress);
    let mut holder = FileLock::open(&operated.path().join(".lock")).unwrap();
    assert!(holder.try_lock(Mode::Exclusive).unwrap());
    std::thread::scope(|scope| {
        // The operation owns the handle, so the message channel closes if it finishes without waiting.
        let waiting = scope.spawn(move || operation(operated));
        // Sent just before the operation blocks on the lock; the operation cannot have changed anything yet.
        let message = messages
            .recv()
            .expect("the operation finished without waiting for the lock");
        assert_eq!(message, "waiting for the ssh configuration");
        holder.unlock().unwrap();
        waiting.join().unwrap();
    });
    SshConfigDir::new(home.join("ssh"))
}

#[skuld::test]
fn writing_an_entry_waits_for_the_lock(#[fixture(unicode_home)] home: &Path) {
    let dir = waits_for_the_lock(home, |dir| {
        dir.write_entry(&entry(home, "web", 1), &host_public()).unwrap()
    });
    assert!(conf(&dir, "web").exists());
}

#[skuld::test]
fn removing_an_entry_waits_for_the_lock(#[fixture(unicode_home)] home: &Path) {
    let plain = SshConfigDir::new(home.join("ssh"));
    plain.write_entry(&entry(home, "web", 1), &host_public()).unwrap();
    let dir = waits_for_the_lock(home, |dir| {
        dir.remove_entry(&InstanceName::parse("web").unwrap()).unwrap()
    });
    assert!(!conf(&dir, "web").exists());
}

#[skuld::test]
fn writing_the_top_level_config_waits_for_the_lock(#[fixture(unicode_home)] home: &Path) {
    let dir = waits_for_the_lock(home, |dir| dir.ensure_top_level().unwrap());
    assert!(dir.path().join("config").exists());
}

#[skuld::test]
fn a_sweep_waits_for_the_lock(#[fixture(unicode_home)] home: &Path) {
    let plain = SshConfigDir::new(home.join("ssh"));
    plain.write_entry(&entry(home, "gone", 1), &host_public()).unwrap();
    let dir = waits_for_the_lock(home, |dir| {
        assert!(dir.sweep(&|_| false).is_empty());
    });
    assert!(!conf(&dir, "gone").exists());
}

#[skuld::test]
fn sweep_removes_temporary_files_a_killed_writer_left(#[fixture(unicode_home)] home: &Path) {
    let dir = SshConfigDir::new(home.join("ssh"));
    dir.write_entry(&entry(home, "kept", 1), &host_public()).unwrap();
    let stale = dir.path().join("kept.conf.tmp-deadbeef");
    std::fs::write(&stale, "half").unwrap();
    assert!(dir.sweep(&|_| true).is_empty());
    assert!(!stale.exists());
    assert!(conf(&dir, "kept").exists());
}

#[skuld::test]
fn top_level_config_is_static_and_idempotent(#[fixture(unicode_home)] home: &Path) {
    let dir = SshConfigDir::new(home.join("ssh"));
    dir.ensure_top_level().unwrap();
    let config = dir.path().join("config");
    let want = format!("Include \"{}/*.conf\"\n", slashed(dir.path()));
    assert_eq!(std::fs::read_to_string(&config).unwrap(), want);
    let link = home.join("config.link");
    std::fs::hard_link(&config, &link).unwrap();
    dir.ensure_top_level().unwrap();
    std::fs::write(&link, "marker\n").unwrap();
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(),
        "marker\n",
        "an unchanged config was replaced"
    );
    // A config that differs (an older devvm, an edit) is put back.
    std::fs::write(&config, "Include nothing\n").unwrap();
    dir.ensure_top_level().unwrap();
    assert_eq!(std::fs::read_to_string(&config).unwrap(), want);
}

/// What `ssh -G` makes of the config for `web`, with the ssh directory below `state` and the entry's files below
/// `files`. Runs the real OpenSSH; fails naming it when it is not on `PATH`.
fn check_system_ssh(state: &Path, files: &Path) {
    let dir = SshConfigDir::new(state.join("ssh"));
    let e = entry(files, "web", 2222);
    dir.write_entry(&e, &host_public()).unwrap();
    dir.ensure_top_level().unwrap();
    let mut cmd = Command::new("ssh");
    cmd.arg("-G").arg("-F").arg(dir.path().join("config")).arg("web");
    TestHost.quiet(&mut cmd);
    let output = cmd
        .output()
        .expect("OpenSSH (`ssh`) must be installed and on PATH for this test");
    assert!(
        output.status.success(),
        "ssh -G failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    let value = |key: &str| -> String {
        text.lines()
            .find_map(|line| line.strip_prefix(&format!("{key} ")))
            .unwrap_or_else(|| {
                panic!(
                    "`ssh -G` printed no {key}:
{text}"
                )
            })
            .to_owned()
    };
    assert_eq!(value("hostname"), "127.0.0.1");
    assert_eq!(value("port"), "2222");
    assert_eq!(value("user"), "user");
    assert_eq!(value("stricthostkeychecking"), "true");
    assert_eq!(value("hostkeyalias"), format!("devvm-{ID}"));
    // `ssh -G` prints these as written or expanded, depending on the version: `%%` or `%`.
    for (key, path) in [("identityfile", &e.identity), ("userknownhostsfile", &e.known_hosts)] {
        let literal = slashed(path);
        let escaped = literal.replace('%', "%%");
        let printed = value(key);
        assert!(printed == literal || printed == escaped, "{key}: {printed}");
    }
}

#[skuld::test]
fn system_ssh_accepts_the_generated_config(#[fixture(unicode_home)] home: &Path) {
    check_system_ssh(home, home);
}

/// `[` and `]` are glob syntax and `$` starts a variable to ssh; a state directory can hold them (`*` and `?` cannot
/// be created on Windows, so `glob_literal_matches_only_itself` covers those).
#[skuld::test]
fn system_ssh_reads_a_state_directory_with_glob_characters_and_a_dollar(#[fixture(unicode_home)] home: &Path) {
    let base = home.join("[work] $HOME{x}");
    std::fs::create_dir(&base).unwrap();
    check_system_ssh(&base, &base);
}

/// A backslash is the escape character of the `Include` glob. Windows cannot create such a directory (the backslash is
/// a separator there), so on Windows this makes a nested one.
#[skuld::test]
fn system_ssh_reads_a_state_directory_with_a_backslash(#[fixture(unicode_home)] home: &Path) {
    let base = home.join(r"back\slash");
    std::fs::create_dir_all(&base).unwrap();
    check_system_ssh(&base, &base);
}

/// A UNC path keeps its backslashes out of the glob by being spelled with `/`; elsewhere a backslash is an ordinary
/// character.
#[skuld::test]
fn a_unc_prefix_is_spelled_with_forward_slashes() {
    let want = if cfg!(windows) {
        "//server/share/x/y"
    } else {
        r"\\server\share\x\y"
    };
    assert_eq!(ssh_path(Path::new(r"\\server\share\x\y")).unwrap(), want);
}

/// `%` is a token in `IdentityFile` and `UserKnownHostsFile`, where `%%` is a literal one on every OpenSSH.
#[skuld::test]
fn system_ssh_reads_key_files_below_a_directory_with_a_percent(#[fixture(unicode_home)] home: &Path) {
    let files = home.join("50% keys");
    std::fs::create_dir(&files).unwrap();
    check_system_ssh(home, &files);
}

#[skuld::test]
fn percent_signs_in_key_paths_and_glob_characters_in_the_state_path_are_escaped(#[fixture(unicode_home)] home: &Path) {
    let state = home.join("[w]");
    let files = home.join("50% keys");
    let dir = SshConfigDir::new(state.join("ssh"));
    let e = entry(&files, "web", 22);
    dir.write_entry(&e, &host_public()).unwrap();
    dir.ensure_top_level().unwrap();
    let text = std::fs::read_to_string(conf(&dir, "web")).unwrap();
    let identity = slashed(&e.identity).replace('%', "%%");
    assert!(text.contains(&format!("IdentityFile \"{identity}\"")), "{text}");
    let known_hosts = slashed(&e.known_hosts).replace('%', "%%");
    assert!(
        text.contains(&format!("UserKnownHostsFile \"{known_hosts}\"")),
        "{text}"
    );
    let config = std::fs::read_to_string(dir.path().join("config")).unwrap();
    let state = slashed(dir.path()).replace("[w]", "[[]w[]]");
    assert_eq!(
        config,
        format!(
            "Include \"{state}/*.conf\"
"
        )
    );
}

#[skuld::test]
fn glob_literal_matches_only_itself() {
    assert_eq!(glob_literal("a*b?c[d]e"), "a[*]b[?]c[[]d[]]e");
    // A backslash quotes the next character in a glob, so a literal one is quoted itself.
    assert_eq!(glob_literal(r"a\b"), r"a\\b");
    assert_eq!(glob_literal("plain/path"), "plain/path");
}

#[skuld::test]
fn a_path_ssh_would_read_as_an_environment_variable_is_refused(#[fixture(unicode_home)] home: &Path) {
    let base = home.join("${HOME}");
    let dir = SshConfigDir::new(base.join("ssh"));
    let error = dir.write_entry(&entry(&base, "web", 22), &host_public()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("${"), "{error}");
    assert!(!conf(&dir, "web").exists(), "nothing was written");
    let error = dir.ensure_top_level().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(!dir.path().join("config").exists());
}

/// No spelling of a literal `%` in an `Include` reads the same on OpenSSH 9 and 10, and a wildcard in its place would
/// also match a sibling directory's confs, so the directory is refused.
#[skuld::test]
fn a_state_directory_with_a_percent_is_refused(#[fixture(unicode_home)] home: &Path) {
    let base = home.join("50% done");
    let dir = SshConfigDir::new(base.join("ssh"));
    let error = dir.ensure_top_level().unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    let message = error.message();
    assert!(message.contains("50% done"), "names the path: {message}");
    assert!(message.contains("XDG_STATE_HOME"), "says what to set: {message}");
    assert!(!dir.path().join("config").exists(), "nothing was written");
}

/// A line break would end the `IdentityFile` line and start another one: an injected ssh_config directive.
#[skuld::test]
fn a_line_break_in_a_path_is_refused(#[fixture(unicode_home)] home: &Path) {
    for breaker in ["\n", "\r", "\r\n"] {
        let base = home.join(format!("x{breaker}ProxyCommand evil"));
        let dir = SshConfigDir::new(home.join("ssh"));
        let error = dir.write_entry(&entry(&base, "web", 22), &host_public()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{breaker:?}: {error}");
        assert!(error.message().contains("line break"), "{breaker:?}: {error}");
        assert!(!conf(&dir, "web").exists(), "nothing was written");

        let dir = SshConfigDir::new(base.join("ssh"));
        let error = dir.ensure_top_level().unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{breaker:?}: {error}");
        assert!(error.message().contains("line break"), "{breaker:?}: {error}");
        assert!(!dir.path().join("config").exists());
    }
}
