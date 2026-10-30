use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::access::SshSession;
use crate::access::sftp::Sftp;
use crate::guest::GuestOs;
use crate::guest::Guests;
use crate::guest::fake::FakeGuests;
use crate::test_support::fixtures::{MemoryProgress, unicode_home};
use crate::test_support::host::RecordingHost;
use crate::test_support::sshd::{Rig, SshdOptions};
use crate::test_support::symlink::symlink;
use crate::{ErrorKind, GuestKind, Result};

struct Env {
    rig: Rig,
    // Kept alive: the SFTP channels belong to it.
    _session: SshSession,
    sftp: Sftp,
    host: RecordingHost,
    guest: Arc<dyn GuestOs>,
    progress: MemoryProgress,
    /// The host side: an empty directory.
    local: PathBuf,
}

impl Env {
    fn new(home: &Path) -> Env {
        Env::with(home, |_| {})
    }

    fn with(home: &Path, configure: impl FnOnce(&mut SshdOptions)) -> Env {
        let rig = Rig::with(home, configure);
        let session = rig.connect();
        let sftp = session.sftp().expect("the test server serves SFTP");
        let local = home.join("local");
        std::fs::create_dir(&local).unwrap();
        Env {
            rig,
            _session: session,
            sftp,
            host: RecordingHost::new(),
            guest: FakeGuests.os(GuestKind::Linux).unwrap(),
            progress: MemoryProgress::default(),
            local,
        }
    }

    /// The server's directory, where guest path `/x` lives as `x`.
    fn root(&self) -> &Path {
        self.rig.sshd.root()
    }

    fn copy_in(&self, src: &Path, dest: &str) -> Result<()> {
        self.sftp.copy_in(src, dest, &self.host, &*self.guest, &self.progress)
    }

    fn copy_out(&self, src: &str, dest: &Path) -> Result<()> {
        self.sftp.copy_out(src, dest, &self.host, &*self.guest, &self.progress)
    }

    /// A host file `name` below `local`, with `content`.
    fn host_file(&self, name: &str, content: &[u8]) -> PathBuf {
        let path = self.local.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    fn guest_file(&self, name: &str, content: &[u8]) -> PathBuf {
        let path = self.root().join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }
}

/// Every file and directory below `dir`, `/`-separated and sorted: a directory ends in `/`.
fn tree(dir: &Path) -> Vec<String> {
    fn visit(dir: &Path, prefix: &str, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
            if entry.file_type().unwrap().is_dir() {
                out.push(format!("{name}/"));
                visit(&entry.path(), &format!("{name}/"), out);
            } else {
                out.push(name);
            }
        }
    }
    let mut out = Vec::new();
    visit(dir, "", &mut out);
    out.sort();
    out
}

// Edge cases ==========================================================================================================

#[skuld::test]
fn cp_home_is_the_server_root(#[fixture(unicode_home)] home: &Path) {
    assert_eq!(Env::new(home).sftp.home().unwrap(), "/");
}

#[skuld::test]
fn cp_keeps_an_empty_directory_in_both_directions(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let source = env.local.join("tree");
    std::fs::create_dir_all(source.join("empty").join("deeper")).unwrap();
    std::fs::write(source.join("file.txt"), b"x").unwrap();
    env.copy_in(&source, "/tree").unwrap();
    assert_eq!(tree(&env.root().join("tree")), ["empty/", "empty/deeper/", "file.txt"]);

    let back = env.local.join("back");
    env.copy_out("/tree", &back).unwrap();
    assert_eq!(tree(&back), ["empty/", "empty/deeper/", "file.txt"]);
}

#[skuld::test]
fn cp_follows_a_symlink_to_a_file_and_to_a_directory(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.guest_file("data/real.txt", b"real");
    env.rig.sshd.add_symlink("/file-link", "/data/real.txt");
    env.rig.sshd.add_symlink("/dir-link", "data");
    env.rig.sshd.add_symlink("/data/inner-link", "real.txt");

    env.copy_out("/file-link", &env.local.join("f.txt")).unwrap();
    assert_eq!(std::fs::read(env.local.join("f.txt")).unwrap(), b"real");

    env.copy_out("/dir-link", &env.local.join("d")).unwrap();
    assert_eq!(tree(&env.local.join("d")), ["inner-link", "real.txt"]);
    assert_eq!(std::fs::read(env.local.join("d").join("inner-link")).unwrap(), b"real");
}

#[skuld::test]
fn cp_in_follows_host_symlinks_to_a_file_and_to_a_directory(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let real = env.host_file("real/data.txt", b"data");
    symlink(&real, &env.local.join("file-link"));
    symlink(&env.local.join("real"), &env.local.join("dir-link"));
    env.copy_in(&env.local.join("file-link"), "/f.txt").unwrap();
    assert_eq!(std::fs::read(env.root().join("f.txt")).unwrap(), b"data");
    env.copy_in(&env.local.join("dir-link"), "/d").unwrap();
    assert_eq!(std::fs::read(env.root().join("d").join("data.txt")).unwrap(), b"data");
}

#[skuld::test]
fn cp_in_refuses_a_host_symlink_loop_and_a_dangling_link_naming_the_path(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.host_file("loop/file.txt", b"x");
    symlink(&env.local.join("loop"), &env.local.join("loop").join("up"));
    let error = env.copy_in(&env.local.join("loop"), "/l").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("symlink loop"), "{error}");
    assert!(error.message().contains("up"), "{error}");

    env.host_file("dangling/file.txt", b"x");
    symlink(&env.local.join("nowhere"), &env.local.join("dangling").join("broken"));
    let error = env.copy_in(&env.local.join("dangling"), "/g").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("dangling symlink"), "{error}");
    assert!(error.message().contains("broken"), "{error}");
}

#[skuld::test]
fn cp_refuses_a_link_that_leads_to_itself_naming_the_path(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.rig.sshd.add_symlink("/self", "/self");
    let error = env.copy_out("/self", &env.local.join("a")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/self"), "{error}");
}

#[skuld::test]
fn cp_refuses_a_symlink_loop_inside_a_tree(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.guest_file("t/sub/file.txt", b"x");
    env.rig.sshd.add_symlink("/t/sub/up", "/t");
    let error = env.copy_out("/t", &env.local.join("a")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("symlink loop"), "{error}");
    assert!(error.message().contains("/t/sub/up"), "{error}");
}

#[skuld::test]
fn cp_refuses_a_dangling_link_inside_a_tree(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.guest_file("t/file.txt", b"x");
    env.rig.sshd.add_symlink("/t/dangle", "/t/nowhere");
    let error = env.copy_out("/t", &env.local.join("a")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("dangling symlink"), "{error}");
    assert!(error.message().contains("/t/dangle"), "{error}");
}

#[skuld::test]
fn cp_carries_awkward_names(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let names = ["a b", "\u{416}", "it's", "$x", "-dash"];
    for name in names {
        env.host_file(&format!("tree/{name}"), name.as_bytes());
    }
    env.copy_in(&env.local.join("tree"), "/tree").unwrap();
    let mut want: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    want.sort();
    assert_eq!(tree(&env.root().join("tree")), want);
    for name in names {
        assert_eq!(
            std::fs::read(env.root().join("tree").join(name)).unwrap(),
            name.as_bytes()
        );
    }
    env.copy_out("/tree", &env.local.join("back")).unwrap();
    assert_eq!(tree(&env.local.join("back")), want);
}

#[skuld::test]
fn cp_out_of_a_guest_name_with_a_newline(#[fixture(unicode_home)] home: &Path) {
    // A name no Windows directory can hold, served by the test server's generated files.
    let env = Env::new(home);
    env.rig.sshd.add_generated_file("/line\nbreak", 5);
    env.copy_out("/line\nbreak", &env.local.join("out.bin")).unwrap();
    assert_eq!(std::fs::metadata(env.local.join("out.bin")).unwrap().len(), 5);
}

#[skuld::test]
fn cp_over_an_existing_file_overwrites_and_into_an_existing_directory_nests(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let src = env.host_file("src.txt", b"new");
    env.guest_file("existing.txt", b"old and longer");
    env.copy_in(&src, "/existing.txt").unwrap();
    assert_eq!(std::fs::read(env.root().join("existing.txt")).unwrap(), b"new");

    std::fs::create_dir(env.root().join("dir")).unwrap();
    env.copy_in(&src, "/dir").unwrap();
    assert_eq!(std::fs::read(env.root().join("dir").join("src.txt")).unwrap(), b"new");

    // A directory lands inside an existing directory, as `<dest>/<basename>`, and is created at a missing path.
    let tree_src = env.local.join("pack");
    std::fs::create_dir(&tree_src).unwrap();
    std::fs::write(tree_src.join("one"), b"1").unwrap();
    env.copy_in(&tree_src, "/dir").unwrap();
    assert_eq!(
        std::fs::read(env.root().join("dir").join("pack").join("one")).unwrap(),
        b"1"
    );
    env.copy_in(&tree_src, "/fresh").unwrap();
    assert_eq!(std::fs::read(env.root().join("fresh").join("one")).unwrap(), b"1");

    // And the same on the way out.
    env.copy_out("/existing.txt", &src).unwrap();
    assert_eq!(std::fs::read(&src).unwrap(), b"new");
    let there = env.local.join("there");
    std::fs::create_dir(&there).unwrap();
    env.copy_out("/dir/pack", &there).unwrap();
    assert_eq!(std::fs::read(there.join("pack").join("one")).unwrap(), b"1");
    env.copy_out("/dir/src.txt", &there).unwrap();
    assert_eq!(std::fs::read(there.join("src.txt")).unwrap(), b"new");
}

#[skuld::test]
fn cp_onto_something_it_would_replace_wrongly_is_an_error(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let file = env.host_file("a", b"x");
    std::fs::create_dir_all(env.root().join("d").join("a")).unwrap();
    // A file onto a directory it would replace.
    let error = env.copy_in(&file, "/d").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/d/a"), "{error}");
    // A directory onto an existing file.
    env.guest_file("plain.txt", b"x");
    let tree_src = env.local.join("tree");
    std::fs::create_dir(&tree_src).unwrap();
    let error = env.copy_in(&tree_src, "/plain.txt").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/plain.txt"), "{error}");
    // A guest directory onto a host file.
    let error = env.copy_out("/d", &file).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains(&file.display().to_string()), "{error}");
    // A guest file onto a host directory it would replace.
    std::fs::create_dir_all(env.local.join("out").join("plain.txt")).unwrap();
    let error = env.copy_out("/plain.txt", &env.local.join("out")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
}

#[skuld::test]
fn cp_reports_a_missing_source_and_a_missing_destination_parent_naming_the_side(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let file = env.host_file("a", b"x");
    let error = env.copy_in(&env.local.join("nope"), "/x").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert!(error.message().contains("host source"), "{error}");

    let error = env.copy_out("/nope", &env.local.join("x")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert!(error.message().contains("guest source /nope"), "{error}");

    let error = env.copy_in(&file, "/missing-dir/x").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert!(
        error.message().contains("guest destination directory /missing-dir"),
        "{error}"
    );
    assert!(!env.root().join("missing-dir").exists(), "the parent is not created");

    env.guest_file("g.txt", b"x");
    let error = env
        .copy_out("/g.txt", &env.local.join("missing-dir").join("x"))
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert!(error.message().contains("host destination directory"), "{error}");
}

#[skuld::test]
fn cp_leaves_crlf_alone_both_ways(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let content = b"one\r\ntwo\r\n\r\nthree\n";
    let src = env.host_file("crlf.txt", content);
    env.copy_in(&src, "/crlf.txt").unwrap();
    assert_eq!(std::fs::read(env.root().join("crlf.txt")).unwrap(), content);
    env.copy_out("/crlf.txt", &env.local.join("back.txt")).unwrap();
    assert_eq!(std::fs::read(env.local.join("back.txt")).unwrap(), content);
}

#[skuld::test]
fn cp_modes_follow_the_host_executable_bit_in_and_the_guest_bit_out(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let plain = env.host_file("plain", b"p");
    let script = env.host_file("script", b"#!/bin/sh\n");
    env.host.mark_executable(&script);
    env.copy_in(&plain, "/plain").unwrap();
    env.copy_in(&script, "/script").unwrap();
    assert_eq!(env.rig.sshd.mode_of("/plain"), Some(0o644));
    assert_eq!(env.rig.sshd.mode_of("/script"), Some(0o755));

    env.copy_out("/plain", &env.local.join("plain-back")).unwrap();
    assert!(env.host.set_executable_calls().is_empty());
    env.copy_out("/script", &env.local.join("script-back")).unwrap();
    assert_eq!(env.host.set_executable_calls(), [env.local.join("script-back")]);
}

#[skuld::test]
fn cp_expands_tilde_paths_against_the_guest_home(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let src = env.host_file("src.txt", b"home");
    env.copy_in(&src, "~/in.txt").unwrap();
    assert_eq!(std::fs::read(env.root().join("in.txt")).unwrap(), b"home");
    env.copy_out("~/in.txt", &env.local.join("out.txt")).unwrap();
    assert_eq!(std::fs::read(env.local.join("out.txt")).unwrap(), b"home");
    env.copy_in(&src, "~").unwrap();
    assert_eq!(std::fs::read(env.root().join("src.txt")).unwrap(), b"home");
}

#[skuld::test]
fn a_tilde_user_guest_path_is_a_usage_error_before_any_transfer(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let src = env.host_file("src.txt", b"x");
    let error = env.copy_in(&src, "~root/x").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Usage, "{error}");
    let error = env.copy_out("~root/x", &env.local.join("o")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Usage, "{error}");
    assert!(tree(env.root()).is_empty(), "nothing reached the guest");
    assert!(!env.local.join("o").exists());
    assert!(env.progress.messages().is_empty(), "no transfer was announced");
}

#[skuld::test]
fn a_dropped_connection_mid_copy_leaves_no_partial_file_at_the_final_path(#[fixture(unicode_home)] home: &Path) {
    let env = Env::with(home, |options| options.kill_sftp_after = Some(100 * 1024));
    let big = env.host_file("big.bin", &vec![7u8; 2 << 20]);
    let error = env.copy_in(&big, "/dest.bin").unwrap_err();
    assert!(
        matches!(error.kind(), ErrorKind::ConnectionLost | ErrorKind::Io),
        "{error}"
    );
    assert!(!env.root().join("dest.bin").exists());

    let home = home.join("second");
    std::fs::create_dir(&home).unwrap();
    let env = Env::with(&home, |options| options.kill_sftp_after = Some(100 * 1024));
    env.rig.sshd.add_generated_file("/big.bin", 2 << 20);
    let out = env.local.join("out.bin");
    let error = env.copy_out("/big.bin", &out).unwrap_err();
    assert!(
        matches!(error.kind(), ErrorKind::ConnectionLost | ErrorKind::Io),
        "{error}"
    );
    assert!(!out.exists());
    assert!(
        tree(&env.local).is_empty(),
        "the temporary file is removed too: {:?}",
        tree(&env.local)
    );
}

#[skuld::test]
fn copy_in_over_a_read_only_guest_file_reports_the_guest_error(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.guest_file("ro.txt", b"keep");
    env.rig.sshd.make_read_only("/ro.txt");
    let src = env.host_file("src.txt", b"new");
    let error = env.copy_in(&src, "/ro.txt").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("Permission denied"), "{error}");
    assert!(error.message().contains("/ro.txt"), "{error}");
    assert_eq!(std::fs::read(env.root().join("ro.txt")).unwrap(), b"keep");
    assert_eq!(tree(env.root()), ["ro.txt"], "the partial file was removed");
}
