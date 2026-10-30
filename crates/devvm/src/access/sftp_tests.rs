use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use crate::access::SshSession;
use crate::access::sftp::{CHUNK, Failed, Sftp, pump};
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

    /// [`tree`] of the guest directory `dir` (a guest path), with the symlinks the server keeps.
    fn guest_tree(&self, dir: &str) -> Vec<String> {
        let mut out = tree(&self.root().join(dir.trim_start_matches('/')));
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        for (path, target) in self.rig.sshd.links() {
            if let Some(rel) = path.strip_prefix(&prefix) {
                out.push(format!("{rel} -> {target}"));
            }
        }
        out.sort();
        out
    }
}

/// Every file, directory and symlink below `dir`, `/`-separated and sorted: a directory ends in `/`, a symlink is
/// followed by ` -> ` and what it holds.
fn tree(dir: &Path) -> Vec<String> {
    fn visit(dir: &Path, prefix: &str, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = format!("{prefix}{}", entry.file_name().to_string_lossy());
            let kind = entry.file_type().unwrap();
            if kind.is_symlink() {
                let target = std::fs::read_link(entry.path()).unwrap();
                out.push(format!("{name} -> {}", target.to_string_lossy().replace('\\', "/")));
            } else if kind.is_dir() {
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

/// The host path `path` with a trailing separator, as a user writes `dir/`.
fn with_slash(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}{}", path.display(), std::path::MAIN_SEPARATOR))
}

const MTIME: u64 = 1_000_000_000;

/// Sets the time of a file or a directory; negative is before 1970.
fn set_time(path: &Path, secs: i64) {
    filetime::set_file_mtime(path, filetime::FileTime::from_unix_time(secs, 0)).unwrap();
}

fn set_mtime(path: &Path, secs: u64) {
    set_time(path, secs as i64);
}

fn mtime(path: &Path) -> u64 {
    std::fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// `proj` on the host: `a.txt`, `sub/b.txt`.
fn project(env: &Env) -> PathBuf {
    env.host_file("proj/a.txt", b"a");
    env.host_file("proj/sub/b.txt", b"b");
    env.local.join("proj")
}

/// `/proj` on the guest: the same tree.
fn guest_project(env: &Env) {
    env.guest_file("proj/a.txt", b"a");
    env.guest_file("proj/sub/b.txt", b"b");
}

const PROJECT: [&str; 3] = ["a.txt", "sub/", "sub/b.txt"];

// Directories =========================================================================================================

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
    assert_eq!(env.guest_tree("/tree"), ["empty/", "empty/deeper/", "file.txt"]);

    let back = env.local.join("back");
    env.copy_out("/tree", &back).unwrap();
    assert_eq!(tree(&back), ["empty/", "empty/deeper/", "file.txt"]);
}

/// The destination is the directory afterwards, whether or not it existed, so a rerun does not nest.
#[skuld::test]
fn a_directory_becomes_the_destination_and_a_rerun_gives_the_same_layout(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let proj = project(&env);
    guest_project(&env);
    std::fs::create_dir(env.root().join("existing")).unwrap();
    for dest in ["/fresh", "/existing", "/deep/er/still"] {
        for run in 1..=2 {
            env.copy_in(&proj, dest).unwrap();
            assert_eq!(env.guest_tree(dest), PROJECT, "in, {dest}, run {run}");
        }
    }
    for name in ["fresh", "existing", "deep"] {
        std::fs::create_dir_all(env.local.join("out").join(name)).unwrap();
    }
    std::fs::remove_dir(env.local.join("out").join("fresh")).unwrap();
    for dest in ["fresh", "existing", "deep/er/still"] {
        for run in 1..=2 {
            let dest = env.local.join("out").join(dest);
            env.copy_out("/proj", &dest).unwrap();
            assert_eq!(tree(&dest), PROJECT, "out, {}, run {run}", dest.display());
        }
    }
}

#[skuld::test]
fn dot_dotdot_and_a_trailing_slash_merge_into_an_existing_directory_and_nothing_is_deleted(
    #[fixture(unicode_home)] home: &Path,
) {
    let env = Env::new(home);
    let proj = project(&env);
    let sources = [proj.join("."), proj.join("sub").join(".."), with_slash(&proj)];
    for (i, src) in sources.iter().enumerate() {
        let dest = format!("/dest{i}");
        env.guest_file(&format!("dest{i}/extra.txt"), b"keep");
        env.copy_in(src, &dest).unwrap();
        assert_eq!(
            env.guest_tree(&dest),
            ["a.txt", "extra.txt", "sub/", "sub/b.txt"],
            "{}",
            src.display()
        );
    }

    guest_project(&env);
    for (i, src) in ["/proj/.", "/proj/sub/..", "/proj/", "/proj"].into_iter().enumerate() {
        let dest = env.local.join(format!("dest{i}"));
        std::fs::create_dir(&dest).unwrap();
        std::fs::write(dest.join("extra.txt"), b"keep").unwrap();
        env.copy_out(src, &dest).unwrap();
        assert_eq!(tree(&dest), ["a.txt", "extra.txt", "sub/", "sub/b.txt"], "{src}");
    }
}

#[skuld::test]
fn a_root_is_a_directory_too(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    guest_project(&env);
    let dest = env.local.join("root");
    env.copy_out("/", &dest).unwrap();
    assert_eq!(tree(&dest), ["proj/", "proj/a.txt", "proj/sub/", "proj/sub/b.txt"]);
}

#[skuld::test]
fn a_destination_with_a_missing_parent_gets_its_parents(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let file = env.host_file("f.txt", b"x");
    env.copy_in(&file, "/a/b/c.txt").unwrap();
    assert_eq!(std::fs::read(env.root().join("a/b/c.txt")).unwrap(), b"x");
    let out = env.local.join("p").join("q").join("f.txt");
    env.copy_out("/a/b/c.txt", &out).unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), b"x");
}

// Files ===============================================================================================================

#[skuld::test]
fn a_file_goes_inside_a_directory_or_a_path_ending_in_a_slash_and_is_the_destination_otherwise(
    #[fixture(unicode_home)] home: &Path,
) {
    let env = Env::new(home);
    let src = env.host_file("f.txt", b"new");
    std::fs::create_dir(env.root().join("dir")).unwrap();
    env.guest_file("old.txt", b"old and longer");
    // (destination, where the file ends up)
    let table = [
        ("/dir", "dir/f.txt"),
        ("/dir/", "dir/f.txt"),
        ("/fresh/", "fresh/f.txt"),
        ("/deep/er/", "deep/er/f.txt"),
        ("/old.txt", "old.txt"),
        ("/renamed.txt", "renamed.txt"),
        ("/no/such/parent.txt", "no/such/parent.txt"),
    ];
    for (dest, lands) in table {
        env.copy_in(&src, dest).unwrap();
        assert_eq!(std::fs::read(env.root().join(lands)).unwrap(), b"new", "in {dest}");
    }

    env.guest_file("g.txt", b"guest");
    std::fs::create_dir(env.local.join("hd")).unwrap();
    env.host_file("old.txt", b"old and longer");
    let table = [
        (env.local.join("hd"), "hd/g.txt"),
        (with_slash(&env.local.join("hd")), "hd/g.txt"),
        (with_slash(&env.local.join("fresh")), "fresh/g.txt"),
        (env.local.join("old.txt"), "old.txt"),
        (env.local.join("renamed.txt"), "renamed.txt"),
        (
            env.local.join("no").join("such").join("parent.txt"),
            "no/such/parent.txt",
        ),
    ];
    for (dest, lands) in table {
        env.copy_out("/g.txt", &dest).unwrap();
        assert_eq!(
            std::fs::read(env.local.join(lands)).unwrap(),
            b"guest",
            "out {}",
            dest.display()
        );
    }
}

#[skuld::test]
fn a_slash_after_a_file_destination_is_an_error(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let src = env.host_file("f.txt", b"x");
    env.guest_file("plain.txt", b"old");
    let error = env.copy_in(&src, "/plain.txt/").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/plain.txt"), "{error}");
    assert_eq!(std::fs::read(env.root().join("plain.txt")).unwrap(), b"old");

    let there = env.host_file("there.txt", b"old");
    let error = env.copy_out("/plain.txt", &with_slash(&there)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert_eq!(std::fs::read(&there).unwrap(), b"old");
}

#[skuld::test]
fn a_directory_onto_a_file_and_a_file_onto_a_directory_are_errors_naming_both_paths(
    #[fixture(unicode_home)] home: &Path,
) {
    let env = Env::new(home);
    let file = env.host_file("a", b"x");
    std::fs::create_dir_all(env.root().join("d").join("a")).unwrap();
    // A file onto a directory it would replace.
    let error = env.copy_in(&file, "/d").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/d/a"), "{error}");
    assert!(error.message().contains(&file.display().to_string()), "{error}");
    // A directory onto an existing file.
    env.guest_file("plain.txt", b"x");
    let tree_src = env.local.join("tree");
    std::fs::create_dir(&tree_src).unwrap();
    let error = env.copy_in(&tree_src, "/plain.txt").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/plain.txt"), "{error}");
    assert!(error.message().contains(&tree_src.display().to_string()), "{error}");
    // A guest directory onto a host file.
    let error = env.copy_out("/d", &file).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains(&file.display().to_string()), "{error}");
    assert!(error.message().contains("/d"), "{error}");
    // A guest file onto a host directory it would replace.
    std::fs::create_dir_all(env.local.join("out").join("plain.txt")).unwrap();
    let error = env.copy_out("/plain.txt", &env.local.join("out")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("plain.txt"), "{error}");
}

#[skuld::test]
fn a_clash_inside_a_merged_tree_is_an_error_naming_both_paths(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    // The host has a file `x`, the guest a directory `x`, and the other way round.
    env.host_file("t/x", b"file");
    env.host_file("t/y/inner", b"i");
    std::fs::create_dir_all(env.root().join("dest1").join("x")).unwrap();
    let error = env.copy_in(&env.local.join("t"), "/dest1").unwrap_err();
    assert!(error.message().contains("/dest1/x"), "{error}");
    env.guest_file("dest2/y", b"a file");
    let error = env.copy_in(&env.local.join("t"), "/dest2").unwrap_err();
    assert!(error.message().contains("/dest2/y"), "{error}");
    assert_eq!(std::fs::read(env.root().join("dest2").join("y")).unwrap(), b"a file");

    env.guest_file("g/x", b"file");
    std::fs::create_dir_all(env.local.join("out").join("x")).unwrap();
    let error = env.copy_out("/g", &env.local.join("out")).unwrap_err();
    assert!(
        error
            .message()
            .contains(&env.local.join("out").join("x").display().to_string()),
        "{error}"
    );
    assert!(env.local.join("out").join("x").is_dir());
}

#[skuld::test]
fn cp_reports_a_missing_source_naming_the_side(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let error = env.copy_in(&env.local.join("nope"), "/x").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert!(error.message().contains("host source"), "{error}");

    let error = env.copy_out("/nope", &env.local.join("x")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert!(error.message().contains("guest source /nope"), "{error}");
    assert!(!env.local.join("x").exists());
}

// Symlinks ============================================================================================================

/// Links in a tree are copied as links, whatever they lead to: a file, a directory, nothing, themselves or their own
/// directory.
#[skuld::test]
fn links_in_a_tree_are_copied_as_links_in_both_directions(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.host_file("pack/real.txt", b"real");
    std::fs::create_dir(env.local.join("pack").join("sub")).unwrap();
    for (name, target) in [
        ("file-link", "real.txt"),
        ("dir-link", "sub"),
        ("dangling", "nowhere"),
        ("self", "self"),
        ("up", "."),
    ] {
        symlink(Path::new(target), &env.local.join("pack").join(name));
    }
    let want = [
        "dangling -> nowhere",
        "dir-link -> sub",
        "file-link -> real.txt",
        "real.txt",
        "self -> self",
        "sub/",
        "up -> .",
    ];
    env.copy_in(&env.local.join("pack"), "/pack").unwrap();
    assert_eq!(env.guest_tree("/pack"), want);
    // Again, over the links that are there now.
    env.copy_in(&env.local.join("pack"), "/pack").unwrap();
    assert_eq!(env.guest_tree("/pack"), want);

    env.copy_out("/pack", &env.local.join("back")).unwrap();
    assert_eq!(tree(&env.local.join("back")), want);
    env.copy_out("/pack", &env.local.join("back")).unwrap();
    assert_eq!(tree(&env.local.join("back")), want);
}

#[skuld::test]
fn a_link_given_as_the_source_is_copied_as_a_link_and_with_a_slash_it_is_followed_once(
    #[fixture(unicode_home)] home: &Path,
) {
    let env = Env::new(home);
    env.host_file("real/a.txt", b"a");
    symlink(Path::new("shared"), &env.local.join("real").join("inner"));
    symlink(&env.local.join("real"), &env.local.join("lnk"));
    let target = env.local.join("real").display().to_string().replace('\\', "/");
    env.copy_in(&env.local.join("lnk"), "/as-link").unwrap();
    assert_eq!(env.rig.sshd.link_target("/as-link"), Some(target));
    assert!(!env.root().join("as-link").exists());
    env.copy_in(&with_slash(&env.local.join("lnk")), "/followed").unwrap();
    assert_eq!(env.guest_tree("/followed"), ["a.txt", "inner -> shared"]);

    env.guest_file("data/b.txt", b"b");
    env.rig.sshd.add_symlink("/data/inner", "shared");
    env.rig.sshd.add_symlink("/lnk", "/data");
    env.copy_out("/lnk", &env.local.join("out-link")).unwrap();
    let held = std::fs::read_link(env.local.join("out-link")).unwrap();
    assert_eq!(held.to_string_lossy().replace('\\', "/"), "/data");
    env.copy_out("/lnk/", &env.local.join("out-followed")).unwrap();
    assert_eq!(tree(&env.local.join("out-followed")), ["b.txt", "inner -> shared"]);
}

#[skuld::test]
fn a_link_the_host_cannot_store_is_an_error_naming_it(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.host = RecordingHost::without_symlinks();
    env.guest_file("t/a.txt", b"a");
    env.rig.sshd.add_symlink("/t/ln", "a.txt");
    let error = env.copy_out("/t", &env.local.join("out")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/t/ln"), "{error}");
    assert!(error.message().contains("cannot make symlinks"), "{error}");
    assert!(!env.local.join("out").join("ln").exists());
}

#[skuld::test]
fn a_guest_symlink_target_that_is_not_valid_utf8_is_refused_naming_the_link(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.guest_file("t/a.txt", b"a");
    env.rig.sshd.add_symlink("/t/ln", "bad\u{fffd}target");
    let error = env.copy_out("/t", &env.local.join("out")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/t/ln"), "{error}");
    assert!(error.message().contains("U+FFFD"), "{error}");
    assert!(!env.local.join("out").exists(), "refused before anything was created");
}

// Replacing ===========================================================================================================

#[skuld::test]
fn a_read_only_target_and_a_symlink_at_the_destination_are_replaced_never_written_through(
    #[fixture(unicode_home)] home: &Path,
) {
    let env = Env::new(home);
    let src = env.host_file("src.txt", b"new");
    // Guest: a read-only file, and a link to a file that must stay as it is.
    env.guest_file("ro.txt", b"old");
    env.rig.sshd.set_mode("/ro.txt", 0o444);
    env.guest_file("elsewhere.txt", b"keep");
    env.rig.sshd.add_symlink("/lnk.txt", "/elsewhere.txt");
    env.copy_in(&src, "/ro.txt").unwrap();
    assert_eq!(std::fs::read(env.root().join("ro.txt")).unwrap(), b"new");
    env.copy_in(&src, "/lnk.txt").unwrap();
    assert_eq!(std::fs::read(env.root().join("lnk.txt")).unwrap(), b"new");
    assert_eq!(
        env.rig.sshd.link_target("/lnk.txt"),
        None,
        "the link itself was replaced"
    );
    assert_eq!(std::fs::read(env.root().join("elsewhere.txt")).unwrap(), b"keep");

    // Host: the same two cases.
    env.guest_file("g.txt", b"guest");
    let ro = env.host_file("ro.txt", b"old");
    let mut permissions = std::fs::metadata(&ro).unwrap().permissions();
    permissions.set_readonly(true);
    std::fs::set_permissions(&ro, permissions).unwrap();
    env.copy_out("/g.txt", &ro).unwrap();
    assert_eq!(std::fs::read(&ro).unwrap(), b"guest");
    let elsewhere = env.host_file("elsewhere.txt", b"keep");
    let lnk = env.local.join("lnk.txt");
    symlink(&elsewhere, &lnk);
    env.copy_out("/g.txt", &lnk).unwrap();
    assert!(std::fs::read_link(&lnk).is_err(), "the link itself was replaced");
    assert_eq!(std::fs::read(&lnk).unwrap(), b"guest");
    assert_eq!(std::fs::read(&elsewhere).unwrap(), b"keep");
}

#[skuld::test]
fn a_symlink_child_met_while_merging_is_replaced_never_written_through(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.host_file("t/a.txt", b"new a");
    env.host_file("t/sub/b.txt", b"new b");
    // Guest: `a.txt` is a link to a file and `sub` a link to a directory, both outside the destination.
    env.guest_file("outside.txt", b"keep");
    std::fs::create_dir(env.root().join("outside")).unwrap();
    std::fs::create_dir(env.root().join("dest")).unwrap();
    env.rig.sshd.add_symlink("/dest/a.txt", "/outside.txt");
    env.rig.sshd.add_symlink("/dest/sub", "/outside");
    env.copy_in(&env.local.join("t"), "/dest").unwrap();
    assert_eq!(env.guest_tree("/dest"), ["a.txt", "sub/", "sub/b.txt"]);
    assert_eq!(std::fs::read(env.root().join("dest/a.txt")).unwrap(), b"new a");
    assert_eq!(std::fs::read(env.root().join("outside.txt")).unwrap(), b"keep");
    assert!(
        tree(&env.root().join("outside")).is_empty(),
        "nothing was written through `sub`"
    );

    // Host: the same.
    env.guest_file("g/a.txt", b"guest a");
    env.guest_file("g/sub/b.txt", b"guest b");
    let outside = env.host_file("outside.txt", b"keep");
    let outside_dir = env.local.join("outside");
    std::fs::create_dir(&outside_dir).unwrap();
    let dest = env.local.join("dest");
    std::fs::create_dir(&dest).unwrap();
    symlink(&outside, &dest.join("a.txt"));
    symlink(&outside_dir, &dest.join("sub"));
    env.copy_out("/g", &dest).unwrap();
    assert_eq!(tree(&dest), ["a.txt", "sub/", "sub/b.txt"]);
    assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
    assert!(tree(&outside_dir).is_empty(), "nothing was written through `sub`");
}

// Metadata ============================================================================================================

#[skuld::test]
fn the_modification_time_is_kept_in_both_directions(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let src = env.host_file("pack/f.txt", b"x");
    set_mtime(&src, MTIME);
    set_mtime(&env.host_file("single.txt", b"y"), MTIME + 7);
    env.copy_in(&env.local.join("pack"), "/pack").unwrap();
    env.copy_in(&env.local.join("single.txt"), "/single.txt").unwrap();
    assert_eq!(mtime(&env.root().join("pack/f.txt")), MTIME);
    assert_eq!(mtime(&env.root().join("single.txt")), MTIME + 7);

    set_mtime(&env.guest_file("g/h.txt", b"z"), MTIME + 99);
    set_mtime(&env.guest_file("one.txt", b"w"), MTIME + 5);
    env.copy_out("/g", &env.local.join("back")).unwrap();
    env.copy_out("/one.txt", &env.local.join("one.txt")).unwrap();
    assert_eq!(mtime(&env.local.join("back/h.txt")), MTIME + 99);
    assert_eq!(mtime(&env.local.join("one.txt")), MTIME + 5);
}

#[skuld::test]
fn the_executable_bit_is_kept_inside_a_tree_in_both_directions(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.host_file("pack/run.sh", b"#!/bin/sh\n");
    env.host_file("pack/plain.txt", b"p");
    env.host.mark_executable(&env.local.join("pack").join("run.sh"));
    env.copy_in(&env.local.join("pack"), "/pack").unwrap();
    assert_eq!(env.rig.sshd.mode_of("/pack/run.sh"), Some(0o755));
    assert_eq!(env.rig.sshd.mode_of("/pack/plain.txt"), Some(0o644));

    env.copy_out("/pack", &env.local.join("back")).unwrap();
    assert_eq!(env.host.set_executable_calls(), [env.local.join("back").join("run.sh")]);
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

// Names and bytes =====================================================================================================

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
    assert_eq!(env.guest_tree("/tree"), want);
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
fn cp_expands_tilde_paths_against_the_guest_home(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let src = env.host_file("src.txt", b"home");
    env.copy_in(&src, "~/in.txt").unwrap();
    assert_eq!(std::fs::read(env.root().join("in.txt")).unwrap(), b"home");
    env.copy_out("~/in.txt", &env.local.join("out.txt")).unwrap();
    assert_eq!(std::fs::read(env.local.join("out.txt")).unwrap(), b"home");
    env.copy_in(&src, "~").unwrap();
    assert_eq!(std::fs::read(env.root().join("src.txt")).unwrap(), b"home");
    env.copy_in(&src, "~/").unwrap();
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

// Refused before anything is written ==================================================================================

/// Windows hosts cannot hold the names this test needs, so it runs where they can be made.
#[cfg(unix)]
#[skuld::test]
fn a_name_a_windows_guest_cannot_store_is_refused_before_anything_is_written(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.guest = FakeGuests.os(GuestKind::Windows).unwrap();
    for (i, name) in ["a?b", "q*", "x.", "a:b"].into_iter().enumerate() {
        env.host_file(&format!("t{i}/fine.txt"), b"f");
        env.host_file(&format!("t{i}/sub/{name}"), b"x");
        let error = env
            .copy_in(&env.local.join(format!("t{i}")), &format!("/d{i}"))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{name}: {error}");
        assert!(error.message().contains(&format!("{name:?}")), "{name}: {error}");
        assert!(tree(env.root()).is_empty(), "{name}: nothing was created");
    }
    // A name taken from the source path for a file going into a directory is checked too.
    let file = env.host_file("a?b", b"x");
    std::fs::create_dir(env.root().join("dir")).unwrap();
    let error = env.copy_in(&file, "/dir").unwrap_err();
    assert!(error.message().contains("\"a?b\""), "{error}");
    assert!(tree(&env.root().join("dir")).is_empty());
}

#[skuld::test]
fn directory_times_are_kept_in_both_directions_over_a_fresh_destination(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    project(&env);
    env.guest_file("g/a.txt", b"a");
    env.guest_file("g/sub/b.txt", b"b");
    // Set last: writing a child changes the time of its directory.
    let src = env.local.join("proj");
    set_mtime(&src.join("sub"), MTIME + 1);
    set_mtime(&src, MTIME + 2);
    set_mtime(&env.root().join("g/sub"), MTIME + 3);
    set_mtime(&env.root().join("g"), MTIME + 4);
    env.copy_in(&src, "/proj").unwrap();
    assert_eq!(mtime(&env.root().join("proj/sub")), MTIME + 1);
    assert_eq!(mtime(&env.root().join("proj")), MTIME + 2);
    env.copy_out("/g", &env.local.join("back")).unwrap();
    assert_eq!(mtime(&env.local.join("back/sub")), MTIME + 3);
    assert_eq!(mtime(&env.local.join("back")), MTIME + 4);
}

#[skuld::test]
fn a_time_sftp_cannot_carry_is_named_and_the_copy_goes_on(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.host_file("t/ok.txt", b"o");
    env.host_file("t/late.txt", b"l");
    env.host_file("t/early.txt", b"e");
    // Spelled as the copy spells them.
    let [late, early, dir] = ["late.txt", "early.txt", "d"].map(|name| env.local.join("t").join(name));
    std::fs::create_dir(&dir).unwrap();
    set_time(&late, 5_000_000_000);
    set_time(&early, -86_400);
    set_time(&dir, 5_000_000_000);
    env.copy_in(&env.local.join("t"), "/t").unwrap();
    assert_eq!(env.guest_tree("/t"), ["d/", "early.txt", "late.txt", "ok.txt"]);
    let notes: Vec<String> = env
        .progress
        .messages()
        .into_iter()
        .filter(|line| line.contains("modification time"))
        .collect();
    for named in [&late, &early, &dir] {
        let named = named.display().to_string();
        assert_eq!(
            notes.iter().filter(|line| line.contains(&named)).count(),
            1,
            "{named}: {notes:?}"
        );
    }
    assert_eq!(notes.len(), 3, "{notes:?}");
    assert!(!notes.iter().any(|line| line.contains("ok.txt")), "{notes:?}");
}

#[skuld::test]
fn a_guest_time_the_server_does_not_show_is_named_and_the_copy_goes_on(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    // The test server shows no time for a file past 2106.
    set_mtime(&env.guest_file("g/late.txt", b"l"), 5_000_000_000);
    env.guest_file("g/ok.txt", b"o");
    let out = env.local.join("out");
    env.copy_out("/g", &out).unwrap();
    assert_eq!(tree(&out), ["late.txt", "ok.txt"]);
    let notes: Vec<String> = env
        .progress
        .messages()
        .into_iter()
        .filter(|line| line.contains("modification time"))
        .collect();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("/g/late.txt"), "{notes:?}");
}

// Checks before anything is written ===================================================================================

#[skuld::test]
fn links_a_guest_cannot_store_fail_the_copy_before_anything_is_written(#[fixture(unicode_home)] home: &Path) {
    let env = Env::with(home, |options| options.no_symlinks = true);
    // The plain file sorts before the link and must not arrive.
    env.host_file("t/a.txt", b"a");
    let link = env.local.join("t").join("ln");
    symlink(Path::new("a.txt"), &link);
    let error = env.copy_in(&env.local.join("t"), "/dest").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains(&link.display().to_string()), "{error}");
    assert!(error.message().contains("cannot make symlinks"), "{error}");
    assert!(!env.root().join("dest").exists(), "nothing was created");
    // A link given as the source is checked too.
    let error = env.copy_in(&link, "/lone").unwrap_err();
    assert!(error.message().contains("cannot make symlinks"), "{error}");
    assert!(!env.root().join("lone").exists());
    // A tree without links goes to the same guest.
    env.copy_in(&project(&env), "/plain").unwrap();
    assert_eq!(env.guest_tree("/plain"), PROJECT);
}

#[skuld::test]
fn links_a_host_cannot_store_fail_the_copy_before_anything_is_written(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.host = RecordingHost::without_symlinks();
    env.guest_file("g/a.txt", b"a");
    env.rig.sshd.add_symlink("/g/ln", "a.txt");
    let out = env.local.join("out");
    let error = env.copy_out("/g", &out).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/g/ln"), "{error}");
    assert!(error.message().contains("cannot make symlinks"), "{error}");
    assert!(!out.exists(), "nothing was created");
    // A tree without links goes to the same host.
    env.guest_file("p/a.txt", b"a");
    env.copy_out("/p", &env.local.join("plain")).unwrap();
    assert_eq!(tree(&env.local.join("plain")), ["a.txt"]);
}

#[skuld::test]
fn the_link_probe_leaves_nothing_behind(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.host_file("t/a.txt", b"a");
    symlink(Path::new("a.txt"), &env.local.join("t").join("ln"));
    env.copy_in(&env.local.join("t"), "/new/deep/dest").unwrap();
    assert_eq!(env.guest_tree("/new/deep/dest"), ["a.txt", "ln -> a.txt"]);
    assert_eq!(env.rig.sshd.links().len(), 1);
    assert_eq!(tree(&env.root().join("new/deep/dest")), ["a.txt"]);
    env.guest_file("g/a.txt", b"a");
    env.rig.sshd.add_symlink("/g/ln", "a.txt");
    env.copy_out("/g", &env.local.join("out")).unwrap();
    assert_eq!(tree(&env.local.join("out")), ["a.txt", "ln -> a.txt"]);
    // The destination did not exist, so the host probe was made in `local`, beside it.
    let beside: Vec<String> = tree(&env.local)
        .into_iter()
        .filter(|entry| entry.contains(".devvm-partial"))
        .collect();
    assert!(beside.is_empty(), "the probe link is still there: {beside:?}");
}

#[skuld::test]
fn a_clash_found_late_in_the_merged_tree_fails_the_copy_before_anything_is_written(
    #[fixture(unicode_home)] home: &Path,
) {
    let env = Env::new(home);
    // `a.txt` sorts before the clashing `x` and `y`.
    env.host_file("t/a.txt", b"a");
    env.host_file("t/x", b"file");
    env.host_file("t/y/inner", b"i");
    std::fs::create_dir_all(env.root().join("d1").join("x")).unwrap();
    let error = env.copy_in(&env.local.join("t"), "/d1").unwrap_err();
    assert!(error.message().contains("/d1/x"), "{error}");
    assert_eq!(tree(&env.root().join("d1")), ["x/"], "nothing was written");
    env.guest_file("d2/y", b"a file");
    let error = env.copy_in(&env.local.join("t"), "/d2").unwrap_err();
    assert!(error.message().contains("/d2/y"), "{error}");
    assert_eq!(tree(&env.root().join("d2")), ["y"], "nothing was written");

    env.guest_file("g/a.txt", b"a");
    env.guest_file("g/x", b"file");
    let out = env.local.join("out");
    std::fs::create_dir_all(out.join("x")).unwrap();
    let error = env.copy_out("/g", &out).unwrap_err();
    assert!(
        error.message().contains(&out.join("x").display().to_string()),
        "{error}"
    );
    assert_eq!(tree(&out), ["x/"], "nothing was written");
}

#[skuld::test]
fn a_file_going_inside_a_directory_onto_a_directory_of_its_name_writes_nothing(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let file = env.host_file("plain.txt", b"p");
    std::fs::create_dir_all(env.root().join("dir").join("plain.txt")).unwrap();
    let error = env.copy_in(&file, "/dir").unwrap_err();
    assert!(error.message().contains("/dir/plain.txt"), "{error}");
    assert_eq!(tree(&env.root().join("dir")), ["plain.txt/"]);
    env.guest_file("plain.txt", b"p");
    let out = env.local.join("out");
    std::fs::create_dir_all(out.join("plain.txt")).unwrap();
    let error = env.copy_out("/plain.txt", &out).unwrap_err();
    assert!(
        error.message().contains(&out.join("plain.txt").display().to_string()),
        "{error}"
    );
    assert_eq!(tree(&out), ["plain.txt/"]);
}

#[skuld::test]
fn what_the_copy_replaces_is_not_checked_for_clashes_below_it(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    // The destination's `lnk` leads to a directory holding a file `f`. The copy replaces `lnk` by a directory, so the
    // source's directory `lnk/f` meets nothing.
    env.host_file("t/lnk/f/g", b"g");
    env.guest_file("real/f", b"keep");
    std::fs::create_dir(env.root().join("dest")).unwrap();
    env.rig.sshd.add_symlink("/dest/lnk", "/real");
    env.copy_in(&env.local.join("t"), "/dest").unwrap();
    assert_eq!(env.guest_tree("/dest"), ["lnk/", "lnk/f/", "lnk/f/g"]);
    assert_eq!(std::fs::read(env.root().join("real/f")).unwrap(), b"keep");

    env.guest_file("g/lnk/f/g", b"g");
    let out = env.local.join("out");
    std::fs::create_dir_all(env.local.join("real")).unwrap();
    std::fs::write(env.local.join("real/f"), b"keep").unwrap();
    std::fs::create_dir(&out).unwrap();
    symlink(&env.local.join("real"), &out.join("lnk"));
    env.copy_out("/g", &out).unwrap();
    assert_eq!(tree(&out), ["lnk/", "lnk/f/", "lnk/f/g"]);
    assert_eq!(std::fs::read(env.local.join("real/f")).unwrap(), b"keep");
}

/// The link probe makes a link at the destination, so it runs after the checks that can fail the copy.
#[skuld::test]
fn a_clash_is_reported_before_the_guest_link_probe_makes_anything(#[fixture(unicode_home)] home: &Path) {
    let env = Env::with(home, |options| options.no_symlinks = true);
    env.host_file("t/x", b"file");
    symlink(Path::new("x"), &env.local.join("t").join("ln"));
    std::fs::create_dir_all(env.root().join("d").join("x")).unwrap();
    let error = env.copy_in(&env.local.join("t"), "/d").unwrap_err();
    assert!(error.message().contains("/d/x"), "{error}");
    assert!(!error.message().contains("cannot make symlinks"), "{error}");
}

#[skuld::test]
fn a_clash_is_reported_before_the_host_link_probe_makes_anything(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.host = RecordingHost::without_symlinks();
    env.guest_file("g/x", b"file");
    env.rig.sshd.add_symlink("/g/ln", "x");
    let out = env.local.join("out");
    std::fs::create_dir_all(out.join("x")).unwrap();
    let error = env.copy_out("/g", &out).unwrap_err();
    assert!(
        error.message().contains(&out.join("x").display().to_string()),
        "{error}"
    );
    assert!(!error.message().contains("cannot make symlinks"), "{error}");
}

// Directory times of a merge ------------------------------------------------------------------------------------------

/// Setting a time needs ownership, and the user may not own a directory they merge into (`/tmp`, `/var/www`). Only a
/// directory the copy made gets the source's time.
#[skuld::test]
fn a_directory_the_copy_did_not_create_keeps_its_own_time(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.host_file("proj/a.txt", b"a");
    env.host_file("proj/sub/b.txt", b"b");
    env.host_file("proj/sub/new/c.txt", b"c");
    let src = env.local.join("proj");
    set_mtime(&src.join("sub/new"), MTIME + 3);
    set_mtime(&src.join("sub"), MTIME + 1);
    set_mtime(&src, MTIME + 2);
    env.guest_file("dest/sub/old.txt", b"o");
    env.copy_in(&src, "/dest").unwrap();
    assert_eq!(mtime(&env.root().join("dest/sub/new")), MTIME + 3, "made by the copy");
    assert_ne!(mtime(&env.root().join("dest/sub")), MTIME + 1, "merged into");
    assert_ne!(mtime(&env.root().join("dest")), MTIME + 2, "merged into");

    env.guest_file("g/a.txt", b"a");
    env.guest_file("g/sub/b.txt", b"b");
    env.guest_file("g/sub/new/c.txt", b"c");
    set_mtime(&env.root().join("g/sub/new"), MTIME + 3);
    set_mtime(&env.root().join("g/sub"), MTIME + 1);
    set_mtime(&env.root().join("g"), MTIME + 2);
    let out = env.local.join("out");
    std::fs::create_dir_all(out.join("sub")).unwrap();
    std::fs::write(out.join("sub/old.txt"), b"o").unwrap();
    env.copy_out("/g", &out).unwrap();
    assert_eq!(mtime(&out.join("sub/new")), MTIME + 3, "made by the copy");
    assert_ne!(mtime(&out.join("sub")), MTIME + 1, "merged into");
    assert_ne!(mtime(&out), MTIME + 2, "merged into");
}

// Special files -------------------------------------------------------------------------------------------------------

/// A FIFO sorts after the regular file, so the refusal comes late in the walk and nothing may have been written.
#[cfg(unix)]
#[skuld::test]
fn a_host_special_file_fails_the_copy_naming_it_and_nothing_is_written(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.host_file("t/a.txt", b"a");
    let fifo = env.local.join("t").join("z");
    let made = std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("the test needs the `mkfifo` command");
    assert!(made.success(), "mkfifo failed");
    for (src, dest) in [(env.local.join("t"), "/dest"), (fifo.clone(), "/lone")] {
        let error = env.copy_in(&src, dest).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert!(error.message().contains(&fifo.display().to_string()), "{error}");
        assert!(error.message().contains("neither a regular file"), "{error}");
        assert!(tree(env.root()).is_empty(), "nothing was written");
    }
}

#[skuld::test]
fn a_guest_special_file_fails_the_copy_naming_it_and_nothing_is_written(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.guest_file("g/a.txt", b"a");
    env.rig.sshd.add_special_file("/g/z");
    for (src, dest) in [("/g", "out"), ("/g/z", "lone")] {
        let dest = env.local.join(dest);
        let error = env.copy_out(src, &dest).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert!(error.message().contains("/g/z"), "{error}");
        assert!(error.message().contains("neither a regular file"), "{error}");
        assert!(!dest.exists(), "nothing was written");
    }
}

// Names of the destination --------------------------------------------------------------------------------------------

/// What the user typed is checked like the names of the tree: the target would drop the dot or read the colon as a
/// stream.
#[skuld::test]
fn a_destination_name_a_windows_guest_cannot_store_is_refused_before_anything_is_written(
    #[fixture(unicode_home)] home: &Path,
) {
    let mut env = Env::new(home);
    env.guest = FakeGuests.os(GuestKind::Windows).unwrap();
    let file = env.host_file("t/f.txt", b"f");
    let dir = env.local.join("t");
    for (src, dest, name) in [
        (&file, "/out.", "out."),
        (&dir, "/out.", "out."),
        (&file, "/a:b", "a:b"),
        (&dir, "/new./deeper/dest", "new."),
        (&file, "/ok/new./f", "new."),
    ] {
        let error = env.copy_in(src, dest).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{dest}: {error}");
        assert!(error.message().contains(&format!("{name:?}")), "{dest}: {error}");
        assert!(tree(env.root()).is_empty(), "{dest}: nothing was created");
    }
    // A component that exists is not checked: the target already stores it.
    std::fs::create_dir(env.root().join("there")).unwrap();
    env.copy_in(&file, "/there/f.txt").unwrap();
    assert_eq!(env.guest_tree("/there"), ["f.txt"]);
}

#[skuld::test]
fn a_destination_name_a_windows_host_cannot_store_is_refused_before_anything_is_written(
    #[fixture(unicode_home)] home: &Path,
) {
    let mut env = Env::new(home);
    env.host = RecordingHost::with_windows_names();
    env.guest_file("g/a.txt", b"a");
    let local = env.local.clone();
    for (src, dest, name) in [
        ("/g/a.txt", local.join("out."), "out."),
        ("/g", local.join("out "), "out "),
        // Spelled as a string: `join` would read `a:` as a drive on Windows.
        ("/g/a.txt", PathBuf::from(format!("{}/a:b", local.display())), "a:b"),
        ("/g", local.join("new.").join("deeper").join("dest"), "new."),
    ] {
        let error = env.copy_out(src, &dest).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{}: {error}", dest.display());
        assert!(
            error.message().contains(&format!("{name:?}")),
            "{}: {error}",
            dest.display()
        );
        assert!(tree(&local).is_empty(), "{}: nothing was created", dest.display());
    }
    // A component that exists is not checked: the target already stores it.
    std::fs::create_dir(local.join("there")).unwrap();
    env.copy_out("/g/a.txt", &local.join("there").join("f.txt")).unwrap();
    assert_eq!(tree(&local.join("there")), ["f.txt"]);
}

// Name rules ----------------------------------------------------------------------------------------------------------

/// The guest's rules apply to the host's names through the guest, so a case pair is one name on a Windows guest. A host
/// that folds case (Windows, macOS) cannot hold the pair to begin with.
#[cfg(target_os = "linux")]
#[skuld::test]
fn a_windows_guest_takes_two_host_names_differing_in_case_for_one(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.guest = FakeGuests.os(GuestKind::Windows).unwrap();
    env.host_file("t/Readme", b"1");
    env.host_file("t/README", b"2");
    let error = env.copy_in(&env.local.join("t"), "/d").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("Readme"), "{error}");
    assert!(error.message().contains("README"), "{error}");
    assert!(tree(env.root()).is_empty(), "nothing was created");
}

/// SFTP v3 carries 32-bit seconds, and OpenSSH sends a time before 1970 as their two's complement: a date after 2038.
/// It cannot be told from a real date that late, so it is named, not applied.
#[skuld::test]
fn a_guest_time_before_1970_is_named_and_the_copy_goes_on(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    set_time(&env.guest_file("g/early.txt", b"e"), -86_400);
    env.guest_file("g/ok.txt", b"o");
    std::fs::create_dir(env.root().join("g/d")).unwrap();
    set_time(&env.root().join("g/d"), -86_400);
    let out = env.local.join("out");
    env.copy_out("/g", &out).unwrap();
    assert_eq!(tree(&out), ["d/", "early.txt", "ok.txt"]);
    let notes: Vec<String> = env
        .progress
        .messages()
        .into_iter()
        .filter(|line| line.contains("modification time"))
        .collect();
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert!(notes.iter().any(|line| line.contains("/g/early.txt")), "{notes:?}");
    assert!(notes.iter().any(|line| line.contains("/g/d")), "{notes:?}");
    let year_2100 = 4_102_444_800;
    assert!(
        mtime(&out.join("early.txt")) < year_2100,
        "the wrapped time was applied"
    );
}

// Failures ============================================================================================================

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
fn copy_in_into_a_read_only_guest_directory_reports_the_guest_error(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    std::fs::create_dir(env.root().join("ro")).unwrap();
    env.rig.sshd.make_dir_read_only("/ro");
    let src = env.host_file("src.txt", b"new");
    let error = env.copy_in(&src, "/ro/x.txt").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("Permission denied"), "{error}");
    assert!(error.message().contains("/ro"), "{error}");
    assert!(tree(&env.root().join("ro")).is_empty());
}

// Which side is named =================================================================================================

#[skuld::test]
fn a_guest_write_failure_names_the_guest_not_the_host_file(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    std::fs::create_dir(env.root().join("full")).unwrap();
    env.rig.sshd.fail_data_in("/full");
    // A small file fails when the handle is closed; one larger than the requests in flight also fails a write that
    // is still being queued.
    for (i, size) in [4096usize, 4 << 20].into_iter().enumerate() {
        let src = env.host_file(&format!("src{i}.txt"), &vec![1u8; size]);
        let error = env.copy_in(&src, &format!("/full/x{i}.txt")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{size}: {error}");
        assert!(error.message().contains("No space left on device"), "{size}: {error}");
        assert!(error.message().contains("the guest"), "{size}: {error}");
        assert!(error.message().contains("/full/"), "{size}: {error}");
        assert!(
            !error.message().contains(&src.display().to_string()),
            "{size}: the host source is not at fault: {error}"
        );
        assert!(
            tree(&env.root().join("full")).is_empty(),
            "{size}: the partial file was removed"
        );
    }
}

#[skuld::test]
fn a_guest_read_failure_names_the_guest_not_the_host_file(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.guest_file("full/x.bin", &vec![1u8; 4096]);
    env.rig.sshd.fail_data_in("/full");
    let out = env.local.join("out.bin");
    let error = env.copy_out("/full/x.bin", &out).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("No space left on device"), "{error}");
    assert!(error.message().contains("/full/x.bin"), "{error}");
    assert!(
        !error.message().contains(".devvm-partial"),
        "the host temporary file is not at fault: {error}"
    );
    assert!(tree(&env.local).is_empty(), "nothing is left on the host");
}

#[skuld::test]
fn pump_tells_which_side_failed() {
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    struct Broken;
    impl AsyncRead for Broken {
        fn poll_read(self: Pin<&mut Self>, _: &mut Context<'_>, _: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Err(std::io::Error::other("read")))
        }
    }
    impl AsyncWrite for Broken {
        fn poll_write(self: Pin<&mut Self>, _: &mut Context<'_>, _: &[u8]) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::Error::other("write")))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    let data = vec![9u8; 3 * CHUNK + 5];
    let outcome = crate::access::runtime::run("pumping", async move {
        let mut copied = Vec::new();
        let ok = pump(&data[..], &mut copied).await;
        let read = pump(Broken, &mut Vec::new()).await;
        let write = pump(&data[..], Broken).await;
        (ok.is_ok() && copied == data, read, write)
    })
    .unwrap();
    assert!(outcome.0, "the bytes arrive unchanged");
    assert!(matches!(outcome.1, Err(Failed::Read(_))), "{:?}", outcome.1);
    assert!(matches!(outcome.2, Err(Failed::Write(_))), "{:?}", outcome.2);
}

// Guest names =========================================================================================================

#[skuld::test]
fn a_guest_name_that_is_not_valid_utf8_is_refused_by_name_not_called_dangling(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    std::fs::create_dir(env.root().join("t")).unwrap();
    // The SFTP library decodes such a name lossily, so the listing holds U+FFFD where the bytes were, and a stat of
    // that name finds nothing.
    env.rig.sshd.list_phantom("/t/bad\u{fffd}name");
    let error = env.copy_out("/t", &env.local.join("out")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("not valid UTF-8"), "{error}");
    assert!(error.message().contains("bad"), "{error}");
    assert!(!error.message().contains("dangling"), "{error}");
    assert!(!env.local.join("out").exists());
}

/// The library hands over the bytes `a\xffb` and the text `a\u{fffd}b` as the same string, so a listing cannot say
/// which one it holds, and a name with U+FFFD is refused even when it exists.
#[skuld::test]
fn a_guest_name_with_a_replacement_character_is_refused_even_when_it_exists(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.guest_file("t/a\u{fffd}b", b"x");
    let error = env.copy_out("/t", &env.local.join("out")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(
        error.message().contains("not valid UTF-8 or contains U+FFFD"),
        "{error}"
    );
    assert!(error.message().contains("a\u{fffd}b"), "{error}");
    assert!(!env.local.join("out").exists());
}

#[skuld::test]
fn cp_out_refuses_a_guest_name_a_windows_host_cannot_hold(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.host = RecordingHost::with_windows_names();
    let names = ["a\\b", "..\\x", "C:\\z", "con", "x.", "a:b", "q?"];
    for (i, name) in names.iter().enumerate() {
        std::fs::create_dir(env.root().join(format!("t{i}"))).unwrap();
        env.rig.sshd.add_generated_file(&format!("/t{i}/{name}"), 3);
        let out = env.local.join(format!("out{i}"));
        let error = env.copy_out(&format!("/t{i}"), &out).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{name}: {error}");
        assert!(error.message().contains(&format!("{name:?}")), "{name}: {error}");
        assert!(!out.exists(), "{name}: refused before anything was created");
    }
}

#[skuld::test]
fn cp_out_refuses_two_guest_names_a_case_insensitive_host_takes_for_one(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.host = RecordingHost::with_windows_names();
    std::fs::create_dir(env.root().join("t")).unwrap();
    env.rig.sshd.add_generated_file("/t/Readme", 3);
    env.rig.sshd.add_generated_file("/t/README", 3);
    let out = env.local.join("out");
    let error = env.copy_out("/t", &out).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("Readme"), "{error}");
    assert!(error.message().contains("README"), "{error}");
    assert!(!out.exists(), "refused before anything was created");
}

#[skuld::test]
fn cp_out_into_a_directory_checks_the_name_it_takes_from_the_guest_path(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.host = RecordingHost::with_windows_names();
    env.rig.sshd.add_generated_file("/con", 3);
    let dest = env.local.join("dest");
    std::fs::create_dir(&dest).unwrap();
    let error = env.copy_out("/con", &dest).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("\"con\""), "{error}");
    assert!(tree(&dest).is_empty());
}
