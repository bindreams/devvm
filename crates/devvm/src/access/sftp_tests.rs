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
use crate::test_support::{file_mode, unix_files};
use crate::{ErrorKind, GuestKind, Result};

// The server ==========================================================================================================

/// Set by the CI job that runs these tests against OpenSSH's own `sftp-server` instead of the scripted one: the copy
/// is then checked against what a real guest does, and a test that cannot run there says why.
const OPENSSH_CLAIM: &str = "DEVVM_COVERS_OPENSSH_SFTP";

/// `Ok` when this run claims OpenSSH's server. The reason it does not is for a test to show as not run.
fn openssh_claimed() -> std::result::Result<(), String> {
    match std::env::var(OPENSSH_CLAIM) {
        Ok(claimed) if !claimed.is_empty() => Ok(()),
        _ => Err(format!(
            "{OPENSSH_CLAIM} is not set; the scripted server is used here, OpenSSH's is covered by CI (Test job on \
             ubuntu-latest)"
        )),
    }
}

/// For a test that needs what only the scripted server does (faults, a dropped connection, a server that refuses a
/// request); where OpenSSH's runs, it is not run.
fn scripted_server() -> std::result::Result<(), String> {
    match openssh_claimed() {
        Ok(()) => Err("needs the scripted server; this run uses OpenSSH's".to_owned()),
        Err(_) => Ok(()),
    }
}

/// A guest of `kind`. With OpenSSH's server its `/` is `root`, the directory the server runs in.
fn guest_os(openssh: bool, root: &Path, kind: GuestKind) -> Arc<dyn GuestOs> {
    if openssh {
        FakeGuests.os_rooted_at(kind, root)
    } else {
        FakeGuests.os(kind).unwrap()
    }
}

/// The claim is true: the SFTP server of this run is OpenSSH's, which has no root of its own and starts in the
/// directory it serves, while the scripted server's home is `/`.
#[skuld::test(requires = [openssh_claimed])]
fn the_server_of_this_run_is_openssh_as_claimed(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    assert!(
        env.openssh,
        "{OPENSSH_CLAIM} is set but the run did not start OpenSSH's server"
    );
    assert_eq!(env.sftp.home().unwrap(), env.g("/"));
}

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
    /// Whether the guest's SFTP server is OpenSSH's own (see [`OPENSSH_CLAIM`]).
    openssh: bool,
}

impl Env {
    fn new(home: &Path) -> Env {
        Env::with(home, |_| {})
    }

    fn with(home: &Path, configure: impl FnOnce(&mut SshdOptions)) -> Env {
        let openssh = openssh_claimed().is_ok();
        let rig = Rig::with(home, |options| {
            configure(options);
            options.openssh_sftp = openssh;
        });
        let session = rig.connect();
        let sftp = session.sftp().expect("the test server serves SFTP");
        let local = home.join("local");
        std::fs::create_dir(&local).unwrap();
        let guest = guest_os(openssh, rig.sshd.root(), GuestKind::Linux);
        Env {
            rig,
            _session: session,
            sftp,
            host: RecordingHost::new(),
            guest,
            progress: MemoryProgress::default(),
            local,
            openssh,
        }
    }

    /// A guest of `kind` whose paths reach the server the way this run's server expects them.
    fn os(&self, kind: GuestKind) -> Arc<dyn GuestOs> {
        guest_os(self.openssh, self.root(), kind)
    }

    /// The guest path `path` as the copy names it in a message: OpenSSH's server has the machine's own paths, so the
    /// guest's `/` is the directory it serves.
    fn g(&self, path: &str) -> String {
        match (self.openssh, path) {
            (false, _) => path.to_owned(),
            (true, "/") => self.root().display().to_string(),
            (true, _) => format!("{}{path}", self.root().display()),
        }
    }

    /// The file system path of the guest path `path`.
    fn at(&self, path: &str) -> PathBuf {
        self.root().join(path.trim_start_matches('/'))
    }

    /// `text` with the served directory taken out of the guest paths in it.
    fn unrooted(&self, text: &str) -> String {
        match self.openssh {
            true => text.replace(&format!("{}/", self.root().display()), "/"),
            false => text.to_owned(),
        }
    }

    /// The guest path `path` is a symlink holding `target`. An absolute target is a guest path.
    fn guest_link(&self, path: &str, target: &str) {
        if self.openssh {
            let target = if target.starts_with('/') {
                self.g(target)
            } else {
                target.to_owned()
            };
            std::fs::create_dir_all(self.at(path).parent().unwrap()).unwrap();
            symlink(Path::new(&target), &self.at(path));
        } else {
            self.rig.sshd.add_symlink(path, target);
        }
    }

    /// What the guest symlink `path` holds, if it is one.
    fn guest_link_target(&self, path: &str) -> Option<String> {
        if self.openssh {
            let held = std::fs::read_link(self.at(path)).ok()?;
            Some(self.unrooted(&held.to_string_lossy()))
        } else {
            self.rig.sshd.link_target(path)
        }
    }

    /// The guest symlink `path` holds bytes that are not UTF-8.
    fn guest_link_with_bad_target(&self, path: &str) {
        if self.openssh {
            unix_files::make_symlink_with_target_bytes(b"bad\xfftarget", &self.at(path));
        } else {
            self.rig.sshd.add_symlink(path, "bad\u{fffd}target");
        }
    }

    /// The permission bits of the guest file `path`; `None` where no one set any.
    fn guest_mode(&self, path: &str) -> Option<u32> {
        if self.openssh {
            file_mode::mode(&self.at(path))
        } else {
            self.rig.sshd.mode_of(path)
        }
    }

    fn set_guest_mode(&self, path: &str, mode: u32) {
        if self.openssh {
            file_mode::set_mode(&self.at(path), mode);
        } else {
            self.rig.sshd.set_mode(path, mode);
        }
    }

    /// The guest directory `path` cannot be changed: no entry in it can be made, removed or renamed.
    fn make_guest_dir_read_only(&self, path: &str) {
        if self.openssh {
            file_mode::set_mode(&self.at(path), 0o555);
        } else {
            self.rig.sshd.make_dir_read_only(path);
        }
    }

    /// A guest file of `size` bytes whose name the host may not be able to hold. The scripted server makes it up.
    fn guest_named_file(&self, path: &str, size: u64) {
        if self.openssh {
            std::fs::create_dir_all(self.at(path).parent().unwrap()).unwrap();
            std::fs::write(self.at(path), vec![0u8; size as usize]).unwrap();
        } else {
            self.rig.sshd.add_generated_file(path, size);
        }
    }

    /// The guest `path` is neither a file, a directory nor a symlink.
    fn guest_special_file(&self, path: &str) {
        if self.openssh {
            unix_files::make_fifo(&self.at(path));
        } else {
            self.rig.sshd.add_special_file(path);
        }
    }

    /// The guest directory `dir` lists a name that is `bad`, a byte that is not UTF-8, and `name`, which the library
    /// decodes lossily and which cannot then be found.
    fn guest_file_with_bad_name(&self, dir: &str) {
        if self.openssh {
            unix_files::make_file_with_name_bytes(&self.at(dir), b"bad\xffname");
        } else {
            self.rig.sshd.list_phantom(&format!("{dir}/bad\u{fffd}name"));
        }
    }

    /// The paths of the symlinks in the guest, sorted.
    fn guest_links(&self) -> Vec<String> {
        let mut found: Vec<String> = tree(self.root())
            .iter()
            .filter_map(|entry| entry.split_once(" -> "))
            .map(|(path, _)| format!("/{path}"))
            .collect();
        found.extend(self.rig.sshd.links().into_iter().map(|(path, _)| path));
        found.sort();
        found
    }

    /// The server's directory, where guest path `/x` lives as `x`.
    fn root(&self) -> &Path {
        self.rig.sshd.root()
    }

    fn copy_in(&self, src: &Path, dest: &str) -> Result<()> {
        self.sftp.copy_in(src, dest, &self.host, &self.guest, &self.progress)
    }

    fn copy_out(&self, src: &str, dest: &Path) -> Result<()> {
        self.sftp.copy_out(src, dest, &self.host, &self.guest, &self.progress)
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
        let mut out: Vec<String> = tree(&self.root().join(dir.trim_start_matches('/')))
            .iter()
            .map(|entry| self.unrooted(entry))
            .collect();
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
    let env = Env::new(home);
    assert_eq!(env.sftp.home().unwrap(), env.g("/"));
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

/// `.` and `..` in a typed destination are read by name, so the destination is made as written and no stray directory
/// is left by the `a` of `a/../b`.
#[skuld::test]
fn a_destination_with_dot_and_dotdot_segments_is_created_as_it_reads(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let proj = project(&env);
    let file = env.host_file("f.txt", b"x");
    env.copy_in(&proj, "/new/.").unwrap();
    assert_eq!(env.guest_tree("/new"), PROJECT);
    env.copy_in(&proj, "/a/../b").unwrap();
    assert_eq!(env.guest_tree("/b"), PROJECT);
    assert!(!env.root().join("a").exists(), "a stray `a` was made");
    env.copy_in(&file, "/d/e/../f.txt").unwrap();
    assert_eq!(std::fs::read(env.root().join("d/f.txt")).unwrap(), b"x");
    assert!(!env.root().join("d/e").exists(), "a stray `e` was made");
    env.copy_in(&file, "/slash/./").unwrap();
    assert_eq!(std::fs::read(env.root().join("slash/f.txt")).unwrap(), b"x");
    env.copy_in(&proj, "~/home/.").unwrap();
    assert_eq!(env.guest_tree("/home"), PROJECT);

    guest_project(&env);
    env.guest_file("g.txt", b"y");
    env.copy_out("/proj", &env.local.join("new").join(".")).unwrap();
    assert_eq!(tree(&env.local.join("new")), PROJECT);
    env.copy_out("/proj", &env.local.join("a").join("..").join("b"))
        .unwrap();
    assert_eq!(tree(&env.local.join("b")), PROJECT);
    assert!(!env.local.join("a").exists(), "a stray `a` was made");
    env.copy_out("/g.txt", &env.local.join("d").join("e").join("..").join("g.txt"))
        .unwrap();
    assert_eq!(std::fs::read(env.local.join("d/g.txt")).unwrap(), b"y");
    assert!(!env.local.join("d/e").exists(), "a stray `e` was made");
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
    assert!(
        error.message().contains(&src.display().to_string()),
        "names the source: {error}"
    );
    assert_eq!(std::fs::read(env.root().join("plain.txt")).unwrap(), b"old");

    let there = env.host_file("there.txt", b"old");
    let error = env.copy_out("/plain.txt", &with_slash(&there)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/plain.txt"), "names the source: {error}");
    assert!(error.message().contains("there.txt"), "names the destination: {error}");
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
    assert_eq!(env.guest_link_target("/as-link"), Some(target));
    assert!(
        !tree(env.root()).contains(&"as-link/".to_owned()),
        "the link was copied as a directory"
    );
    env.copy_in(&with_slash(&env.local.join("lnk")), "/followed").unwrap();
    assert_eq!(env.guest_tree("/followed"), ["a.txt", "inner -> shared"]);

    env.guest_file("data/b.txt", b"b");
    env.guest_link("/data/inner", "shared");
    env.guest_link("/lnk", "/data");
    env.copy_out("/lnk", &env.local.join("out-link")).unwrap();
    let held = std::fs::read_link(env.local.join("out-link")).unwrap();
    assert_eq!(held.to_string_lossy().replace('\\', "/"), env.g("/data"));
    env.copy_out("/lnk/", &env.local.join("out-followed")).unwrap();
    assert_eq!(tree(&env.local.join("out-followed")), ["b.txt", "inner -> shared"]);
}

#[skuld::test]
fn a_link_the_host_cannot_store_is_an_error_naming_it(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.host = RecordingHost::without_symlinks();
    env.guest_file("t/a.txt", b"a");
    env.guest_link("/t/ln", "a.txt");
    let error = env.copy_out("/t", &env.local.join("out")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/t/ln"), "{error}");
    assert!(error.message().contains("refused a test symlink"), "{error}");
    assert!(!env.local.join("out").join("ln").exists());
}

#[skuld::test]
fn a_guest_symlink_target_that_is_not_valid_utf8_is_refused_naming_the_link(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.guest_file("t/a.txt", b"a");
    env.guest_link_with_bad_target("/t/ln");
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
    env.set_guest_mode("/ro.txt", 0o444);
    env.guest_file("elsewhere.txt", b"keep");
    env.guest_link("/lnk.txt", "/elsewhere.txt");
    env.copy_in(&src, "/ro.txt").unwrap();
    assert_eq!(std::fs::read(env.root().join("ro.txt")).unwrap(), b"new");
    env.copy_in(&src, "/lnk.txt").unwrap();
    assert_eq!(std::fs::read(env.root().join("lnk.txt")).unwrap(), b"new");
    assert_eq!(env.guest_link_target("/lnk.txt"), None, "the link itself was replaced");
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
    env.guest_link("/dest/a.txt", "/outside.txt");
    env.guest_link("/dest/sub", "/outside");
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

#[skuld::test]
fn a_file_or_link_replaces_a_symlink_to_a_directory_met_while_merging(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.guest_file("g/plain", b"guest plain");
    env.guest_file("outside.txt", b"guest outside");
    env.guest_link("/g/lnk", "/outside.txt");
    let outside_dir = env.local.join("outside");
    std::fs::create_dir(&outside_dir).unwrap();
    let dest = env.local.join("dest");
    std::fs::create_dir(&dest).unwrap();
    symlink(&outside_dir, &dest.join("plain"));
    symlink(&outside_dir, &dest.join("lnk"));
    env.copy_out("/g", &dest).unwrap();
    assert_eq!(std::fs::read(dest.join("plain")).unwrap(), b"guest plain");
    assert!(
        !std::fs::symlink_metadata(dest.join("plain"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the file replaced the link"
    );
    assert!(
        std::fs::symlink_metadata(dest.join("lnk"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the guest link replaced the directory link"
    );
    assert!(!dest.join("lnk").is_dir());
    assert!(tree(&outside_dir).is_empty(), "nothing was written through either");
}

// A destination that is a symlink =====================================================================================

/// The typed destination that is a link to a directory is followed, for a directory and a file source, with and without
/// `/`. The link stays; a link met inside the merged tree is still replaced.
#[skuld::test]
fn a_destination_that_is_a_symlink_to_a_directory_is_followed_in_both_directions(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.host_file("t/a.txt", b"new a");
    env.host_file("t/sub/b.txt", b"new b");
    let file = env.host_file("f.txt", b"new f");
    env.guest_file("outside.txt", b"keep");
    // (source is a directory, spelling of the destination)
    let cases = [(true, ""), (true, "/"), (false, ""), (false, "/")];
    for (n, (is_dir, spelling)) in cases.into_iter().enumerate() {
        let (real, lnk) = (format!("/real{n}"), format!("/lnk{n}"));
        env.guest_file(&format!("real{n}/old.txt"), b"old");
        env.guest_link(&format!("{real}/a.txt"), "/outside.txt");
        env.guest_link(&lnk, &real);
        let src = if is_dir { env.local.join("t") } else { file.clone() };
        env.copy_in(&src, &format!("{lnk}{spelling}")).unwrap();
        // A directory replaces the link child `a.txt` with its own file; a file leaves it alone.
        let want: &[&str] = if is_dir {
            &["a.txt", "old.txt", "sub/", "sub/b.txt"]
        } else {
            &["a.txt -> /outside.txt", "f.txt", "old.txt"]
        };
        assert_eq!(
            env.guest_tree(&real),
            want,
            "in, {lnk}{spelling}, directory source {is_dir}"
        );
        assert_eq!(
            env.guest_link_target(&lnk),
            Some(real),
            "the link stays: {lnk}{spelling}"
        );
        assert_eq!(
            std::fs::read(env.root().join("outside.txt")).unwrap(),
            b"keep",
            "{lnk}{spelling}"
        );
    }

    env.guest_file("g/a.txt", b"guest a");
    env.guest_file("g/sub/b.txt", b"guest b");
    env.guest_file("g.txt", b"guest f");
    let outside = env.host_file("outside.txt", b"keep");
    let held = format!("a.txt -> {}", outside.display().to_string().replace('\\', "/"));
    for (n, (is_dir, spelling)) in cases.into_iter().enumerate() {
        let (real, lnk) = (env.local.join(format!("hreal{n}")), env.local.join(format!("hlnk{n}")));
        std::fs::create_dir(&real).unwrap();
        std::fs::write(real.join("old.txt"), b"old").unwrap();
        symlink(&outside, &real.join("a.txt"));
        symlink(&real, &lnk);
        let dest = if spelling.is_empty() {
            lnk.clone()
        } else {
            with_slash(&lnk)
        };
        env.copy_out(if is_dir { "/g" } else { "/g.txt" }, &dest).unwrap();
        let want: Vec<&str> = if is_dir {
            vec!["a.txt", "old.txt", "sub/", "sub/b.txt"]
        } else {
            vec![&held, "g.txt", "old.txt"]
        };
        assert_eq!(tree(&real), want, "out, {}, directory source {is_dir}", dest.display());
        assert!(
            std::fs::symlink_metadata(&lnk).unwrap().file_type().is_symlink(),
            "the link stays: {}",
            dest.display()
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep", "{}", dest.display());
    }
}

/// A typed destination that is a link to a file (or to nothing) counts as a file: a directory onto it is an error
/// naming both paths and leaves it alone; a file onto it replaces the link.
#[skuld::test]
fn a_destination_that_is_a_symlink_to_a_file_counts_as_a_file_in_both_directions(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let dir = env.local.join("t");
    env.host_file("t/a.txt", b"a");
    let file = env.host_file("f.txt", b"new");
    env.guest_file("target.txt", b"keep");
    env.guest_link("/to-file", "/target.txt");
    env.guest_link("/dangling", "/nowhere");
    for lnk in ["/to-file", "/to-file/", "/dangling", "/dangling/"] {
        let error = env.copy_in(&dir, lnk).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{lnk}: {error}");
        assert!(error.message().contains(&dir.display().to_string()), "{lnk}: {error}");
        assert!(error.message().contains(lnk.trim_end_matches('/')), "{lnk}: {error}");
    }
    assert_eq!(env.guest_link_target("/to-file"), Some("/target.txt".to_owned()));
    assert_eq!(env.guest_link_target("/dangling"), Some("/nowhere".to_owned()));
    assert_eq!(std::fs::read(env.root().join("target.txt")).unwrap(), b"keep");
    // A file replaces the link without a slash, and a slash after a link to a file is an error.
    let error = env.copy_in(&file, "/to-file/").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    env.copy_in(&file, "/to-file").unwrap();
    assert_eq!(env.guest_link_target("/to-file"), None);
    assert_eq!(std::fs::read(env.root().join("to-file")).unwrap(), b"new");
    assert_eq!(std::fs::read(env.root().join("target.txt")).unwrap(), b"keep");

    // Out.
    env.guest_file("g/a.txt", b"guest a");
    env.guest_file("g.txt", b"guest f");
    let target = env.host_file("target.txt", b"keep");
    symlink(&target, &env.local.join("to-file"));
    symlink(Path::new("nowhere"), &env.local.join("dangling"));
    for name in ["to-file", "dangling"] {
        let link = env.local.join(name);
        for dest in [link.clone(), with_slash(&link)] {
            let error = env.copy_out("/g", &dest).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Io, "{}: {error}", dest.display());
            assert!(error.message().contains("/g"), "{}: {error}", dest.display());
            assert!(
                error.message().contains(&link.display().to_string()),
                "{}: {error}",
                dest.display()
            );
            assert!(
                std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(),
                "the link stays"
            );
        }
    }
    assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    let link = env.local.join("to-file");
    env.copy_out("/g.txt", &link).unwrap();
    assert!(std::fs::read_link(&link).is_err(), "the link itself was replaced");
    assert_eq!(std::fs::read(&link).unwrap(), b"guest f");
    assert_eq!(std::fs::read(&target).unwrap(), b"keep");
}

/// A file onto a link to nothing replaces the link; with a `/` it is an error, in both directions.
#[skuld::test]
fn a_file_onto_a_dangling_link_replaces_it_and_with_a_slash_is_an_error(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let file = env.host_file("f.txt", b"new");
    env.guest_link("/dangling", "/nowhere");
    let error = env.copy_in(&file, "/dangling/").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains(&file.display().to_string()), "{error}");
    assert_eq!(env.guest_link_target("/dangling"), Some("/nowhere".to_owned()));
    env.copy_in(&file, "/dangling").unwrap();
    assert_eq!(env.guest_link_target("/dangling"), None);
    assert_eq!(std::fs::read(env.root().join("dangling")).unwrap(), b"new");

    env.guest_file("g.txt", b"guest");
    let link = env.local.join("dangling");
    symlink(Path::new("nowhere"), &link);
    let error = env.copy_out("/g.txt", &with_slash(&link)).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/g.txt"), "{error}");
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
    env.copy_out("/g.txt", &link).unwrap();
    assert!(std::fs::read_link(&link).is_err(), "the link itself was replaced");
    assert_eq!(std::fs::read(&link).unwrap(), b"guest");
}

/// A destination link whose target cannot be looked at (a loop here) is an error naming it and the cause, not a link to
/// a file that a file replaces or a directory refuses as "not a directory".
#[skuld::test]
fn a_destination_link_that_cannot_be_looked_at_is_an_error_naming_it_and_the_cause(
    #[fixture(unicode_home)] home: &Path,
) {
    let env = Env::new(home);
    let dir = env.local.join("t");
    env.host_file("t/a.txt", b"a");
    let file = env.host_file("f.txt", b"new");
    env.guest_link("/loop-a", "/loop-b");
    env.guest_link("/loop-b", "/loop-a");
    for src in [&dir, &file] {
        let error = env.copy_in(src, "/loop-a").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{}: {error}", src.display());
        assert!(error.message().contains("/loop-a"), "{error}");
        assert!(error.message().contains("symbolic links"), "the cause: {error}");
        assert!(!error.message().contains("not a directory"), "{error}");
        assert_eq!(env.guest_link_target("/loop-a"), Some("/loop-b".to_owned()));
    }

    env.guest_file("g/a.txt", b"a");
    env.guest_file("g.txt", b"guest");
    let (a, b) = (env.local.join("loop-a"), env.local.join("loop-b"));
    symlink(&b, &a);
    symlink(&a, &b);
    for src in ["/g", "/g.txt"] {
        let error = env.copy_out(src, &a).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{src}: {error}");
        assert!(error.message().contains(&a.display().to_string()), "{error}");
        assert!(!error.message().contains("not a directory"), "{error}");
        assert!(std::fs::symlink_metadata(&a).unwrap().file_type().is_symlink());
    }
}

/// A `..` in a link's target is read against the directory the link really is in, so a symlinked parent decides which
/// chain is walked: a loop through one is a loop, and a link that only looks like a loop by name is dangling.
#[skuld::test]
fn a_chain_of_guest_links_through_a_symlinked_parent_is_walked_where_the_links_really_are(
    #[fixture(unicode_home)] home: &Path,
) {
    let env = Env::new(home);
    let file = env.host_file("f.txt", b"new");

    // `/proj` is `/data/proj`; the links `/data/proj/l` and `/data/other/l` lead to each other through `..`. By name
    // from `/proj/l`, `../other/l` is `/other/l`, which is absent.
    std::fs::create_dir_all(env.at("/data/proj")).unwrap();
    std::fs::create_dir_all(env.at("/data/other")).unwrap();
    env.guest_link("/proj", "/data/proj");
    env.guest_link("/data/proj/l", "../other/l");
    env.guest_link("/data/other/l", "../proj/l");
    let error = env.copy_in(&file, "/proj/l").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/proj/l"), "{error}");
    assert!(error.message().contains("symbolic links"), "{error}");
    assert_eq!(env.guest_link_target("/data/proj/l"), Some("../other/l".to_owned()));

    // `/t/s` is `/real/deep`; the link `/real/deep/l` holds `../s/l`, which is `/real/s/l`, absent: dangling. By name
    // from `/t/s/l` it would be `/t/s/l` itself.
    std::fs::create_dir_all(env.at("/real/deep")).unwrap();
    std::fs::create_dir_all(env.at("/t")).unwrap();
    env.guest_link("/t/s", "/real/deep");
    env.guest_link("/real/deep/l", "../s/l");
    env.copy_in(&file, "/t/s/l").unwrap();
    assert_eq!(
        env.guest_link_target("/real/deep/l"),
        None,
        "the dangling link was replaced"
    );
    assert_eq!(std::fs::read(env.at("/real/deep/l")).unwrap(), b"new");
}

/// OpenSSH says "no such file" for a loop as for a link to nothing, so the guest's links are followed by hand: a chain
/// that ends in nothing is dangling whatever its length, and relative links that lead back to themselves are a loop.
#[skuld::test]
fn a_chain_of_guest_links_is_dangling_or_a_loop_by_where_it_ends(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let file = env.host_file("f.txt", b"new");
    env.guest_link("/c1", "/c2");
    env.guest_link("/c2", "c3");
    env.guest_link("/c3", "/nowhere");
    env.copy_in(&file, "/c1").unwrap();
    assert_eq!(env.guest_link_target("/c1"), None, "the dangling link was replaced");
    assert_eq!(std::fs::read(env.root().join("c1")).unwrap(), b"new");

    std::fs::create_dir(env.root().join("d")).unwrap();
    env.guest_link("/d/r1", "r2");
    env.guest_link("/d/r2", "../d/./r3");
    env.guest_link("/d/r3", "r1");
    for dest in ["/d/r1", "/d/r2", "/d/r3"] {
        let error = env.copy_in(&file, dest).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{dest}: {error}");
        assert!(error.message().contains("symbolic links"), "{dest}: {error}");
    }
    assert_eq!(
        env.guest_link_target("/d/r1"),
        Some("r2".to_owned()),
        "the loop is left alone"
    );
}

/// A link that holds a path through itself (`l` -> `l/l`) is a loop, though the guest answers "no such file" for the
/// directory component `l` as it does for one that is missing; a link through a directory that is really missing is
/// dangling.
#[skuld::test]
fn a_guest_link_that_leads_through_itself_is_a_loop_and_one_through_a_missing_directory_is_dangling(
    #[fixture(unicode_home)] home: &Path,
) {
    let env = Env::new(home);
    let file = env.host_file("f.txt", b"new");
    env.guest_link("/l", "l/l");
    env.guest_link("/m", "/m/x");
    env.guest_link("/p", "q/x");
    env.guest_link("/q", "p/y");
    for dest in ["/l", "/m", "/p", "/q"] {
        let error = env.copy_in(&file, dest).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{dest}: {error}");
        assert!(error.message().contains(dest), "{dest}: {error}");
        assert!(error.message().contains("symbolic links"), "{dest}: {error}");
    }
    assert_eq!(
        env.guest_link_target("/l"),
        Some("l/l".to_owned()),
        "the loop is left alone"
    );

    env.guest_link("/gone", "nowhere/x");
    env.copy_in(&file, "/gone").unwrap();
    assert_eq!(env.guest_link_target("/gone"), None, "the dangling link was replaced");
    assert_eq!(std::fs::read(env.root().join("gone")).unwrap(), b"new");
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

/// The host's execute bit was set `count` times, each on a temporary file in `dir`: the target is only ever replaced
/// by a file that has the bit already.
fn assert_executable_set_on_partials(env: &Env, dir: &Path, count: usize) {
    let calls = env.host.set_executable_calls();
    assert_eq!(calls.len(), count, "{calls:?}");
    for call in calls {
        assert_eq!(call.parent(), Some(dir), "{call:?}");
        assert!(
            call.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".devvm-partial-"),
            "{call:?}"
        );
    }
}

#[skuld::test]
fn the_executable_bit_is_kept_inside_a_tree_in_both_directions(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.host_file("pack/run.sh", b"#!/bin/sh\n");
    env.host_file("pack/plain.txt", b"p");
    env.host.mark_executable(&env.local.join("pack").join("run.sh"));
    env.copy_in(&env.local.join("pack"), "/pack").unwrap();
    assert_eq!(env.guest_mode("/pack/run.sh"), Some(0o755));
    assert_eq!(env.guest_mode("/pack/plain.txt"), Some(0o644));

    env.copy_out("/pack", &env.local.join("back")).unwrap();
    assert_executable_set_on_partials(&env, &env.local.join("back"), 1);
}

#[skuld::test]
fn cp_modes_follow_the_host_executable_bit_in_and_the_guest_bit_out(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let plain = env.host_file("plain", b"p");
    let script = env.host_file("script", b"#!/bin/sh\n");
    env.host.mark_executable(&script);
    env.copy_in(&plain, "/plain").unwrap();
    env.copy_in(&script, "/script").unwrap();
    assert_eq!(env.guest_mode("/plain"), Some(0o644));
    assert_eq!(env.guest_mode("/script"), Some(0o755));

    env.copy_out("/plain", &env.local.join("plain-back")).unwrap();
    assert!(env.host.set_executable_calls().is_empty());
    env.copy_out("/script", &env.local.join("script-back")).unwrap();
    assert_executable_set_on_partials(&env, &env.local, 1);
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
    env.guest_named_file("/line\nbreak", 5);
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

/// The copy loop ends on an empty read: an empty file, a size that ends exactly on a chunk, and one byte either side.
#[skuld::test]
fn files_of_every_size_around_a_chunk_arrive_unchanged_in_both_directions(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    for size in [0, 1, CHUNK - 1, CHUNK, CHUNK + 1, 2 * CHUNK, 3 * CHUNK + 5] {
        let content: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        let src = env.host_file(&format!("in{size}.bin"), &content);
        env.copy_in(&src, &format!("/in{size}.bin")).unwrap();
        assert_eq!(
            std::fs::read(env.root().join(format!("in{size}.bin"))).unwrap(),
            content,
            "in, {size} bytes"
        );
        env.guest_file(&format!("out{size}.bin"), &content);
        let out = env.local.join(format!("out{size}.bin"));
        env.copy_out(&format!("/out{size}.bin"), &out).unwrap();
        assert_eq!(std::fs::read(&out).unwrap(), content, "out, {size} bytes");
    }
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

/// A guest whose user's home is not the root: `~` is what the server says it is, and the copy lands there.
#[skuld::test(requires = [scripted_server])]
fn tilde_paths_are_read_against_a_home_that_is_not_the_root(#[fixture(unicode_home)] home: &Path) {
    let env = Env::with(home, |options| options.home = Some("/home/user".to_owned()));
    std::fs::create_dir_all(env.root().join("home/user")).unwrap();
    assert_eq!(env.sftp.home().unwrap(), "/home/user");
    let proj = project(&env);
    let file = env.host_file("f.txt", b"f");
    env.copy_in(&file, "~/in.txt").unwrap();
    env.copy_in(&file, "~").unwrap();
    env.copy_in(&file, "~/").unwrap();
    env.copy_in(&proj, "~/proj").unwrap();
    env.copy_in(&proj, "~").unwrap();
    assert_eq!(
        tree(&env.root().join("home/user")),
        [
            "a.txt",
            "f.txt",
            "in.txt",
            "proj/",
            "proj/a.txt",
            "proj/sub/",
            "proj/sub/b.txt",
            "sub/",
            "sub/b.txt"
        ]
    );
    assert!(
        tree(env.root()).iter().all(|entry| entry.starts_with("home")),
        "nothing landed outside the home"
    );

    let out = env.local.join("out");
    env.copy_out("~/proj", &out).unwrap();
    assert_eq!(tree(&out), PROJECT);
    env.copy_out("~/in.txt", &env.local.join("in.txt")).unwrap();
    assert_eq!(std::fs::read(env.local.join("in.txt")).unwrap(), b"f");
    let error = env.copy_out("~/missing", &env.local.join("m")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
}

/// A guest that spells paths otherwise than its SFTP server does (a drive letter, say): the user's path is mapped once,
/// the destination and the source alike, and the copy lands where the server says.
#[skuld::test(requires = [scripted_server])]
fn a_guest_path_is_given_to_the_server_as_the_guest_maps_it(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.guest = FakeGuests.os_on_drive(GuestKind::Linux, "Z:");
    let proj = project(&env);
    let file = env.host_file("f.txt", b"f");
    env.copy_in(&file, "Z:/in.txt").unwrap();
    env.copy_in(&file, "Z:/dir/").unwrap();
    env.copy_in(&proj, "Z:/proj").unwrap();
    env.copy_in(&proj, "Z:/deep/er/proj/.").unwrap();
    assert_eq!(
        tree(env.root()),
        [
            "deep/",
            "deep/er/",
            "deep/er/proj/",
            "deep/er/proj/a.txt",
            "deep/er/proj/sub/",
            "deep/er/proj/sub/b.txt",
            "dir/",
            "dir/f.txt",
            "in.txt",
            "proj/",
            "proj/a.txt",
            "proj/sub/",
            "proj/sub/b.txt",
        ]
    );

    let out = env.local.join("out");
    env.copy_out("Z:/proj", &out).unwrap();
    assert_eq!(tree(&out), PROJECT);
    env.copy_out("Z:/in.txt", &env.local.join("in.txt")).unwrap();
    assert_eq!(std::fs::read(env.local.join("in.txt")).unwrap(), b"f");
    let error = env.copy_out("Z:/missing", &env.local.join("m")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
}

/// A drive root is a root: a copy to `Z:/`, `Z:` or to a `..` above it lands at the server's root, not in its working
/// directory, and merges there.
#[skuld::test(requires = [scripted_server])]
fn a_destination_that_is_a_drive_root_or_dotdot_above_it_is_the_root(#[fixture(unicode_home)] home: &Path) {
    // The user's home is not the root, where a destination left relative would land.
    let mut env = Env::with(home, |options| options.home = Some("/home/user".to_owned()));
    env.guest = FakeGuests.os_on_drive(GuestKind::Linux, "Z:");
    std::fs::create_dir_all(env.root().join("home/user")).unwrap();
    let proj = project(&env);
    for typed in ["Z:/", "Z:", "Z:/..", "Z:/a/../..", "Z:/./"] {
        env.copy_in(&proj, typed).unwrap();
        assert_eq!(
            tree(env.root()),
            ["a.txt", "home/", "home/user/", "sub/", "sub/b.txt"],
            "{typed}"
        );
        std::fs::remove_dir_all(env.root().join("sub")).unwrap();
        std::fs::remove_file(env.root().join("a.txt")).unwrap();
    }
}

/// A message names the guest path as the user typed it, not as the server is asked for it (here without the drive).
#[skuld::test(requires = [scripted_server])]
fn errors_name_the_guest_path_the_user_typed(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.guest = FakeGuests.os_on_drive(GuestKind::Linux, "Z:");
    let proj = project(&env);
    let file = env.host_file("f.txt", b"f");
    env.copy_in(&file, "Z:/in.txt").unwrap();

    let named = |error: &crate::Error, typed: &str| {
        assert!(error.message().contains(typed), "{typed} is not named: {error}");
    };
    named(
        &env.copy_out("Z:/missing", &env.local.join("m")).unwrap_err(),
        "Z:/missing",
    );
    // A directory onto a file, and a file below a file.
    named(&env.copy_in(&proj, "Z:/in.txt").unwrap_err(), "Z:/in.txt");
    named(&env.copy_in(&file, "Z:/in.txt/below").unwrap_err(), "Z:/in.txt");
}

/// What the user typed is what a message names, whatever the copy did to it to find the file: `~` stays `~`, a trailing
/// `/`, a `..` and a doubled `/` stay.
#[skuld::test]
fn errors_name_the_guest_destination_exactly_as_typed(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let proj = project(&env);
    let file = env.host_file("f.txt", b"f");
    env.copy_in(&file, "/in.txt").unwrap();
    env.copy_in(&file, "/d/in.txt").unwrap();
    env.copy_in(&file, "/dd/sub").unwrap();

    // The destination itself is a file the directory cannot go onto.
    for typed in ["~/in.txt", "/in.txt/", "/a/../in.txt", "/d//in.txt", "/./in.txt"] {
        let error = env.copy_in(&proj, typed).unwrap_err();
        let wanted = format!("onto the guest path {typed}, which is not a directory");
        assert!(error.message().contains(&wanted), "{wanted:?} is not in: {error}");
    }
    // An entry below the destination clashes: the path is the typed one, joined with the entry's name.
    for typed in ["~/dd", "/dd/", "/a/../dd", "/x//../dd", "/dd//"] {
        let error = env.copy_in(&proj, typed).unwrap_err();
        let wanted = format!("onto {}/sub on the guest", typed.trim_end_matches('/'));
        assert!(error.message().contains(&wanted), "{wanted:?} is not in: {error}");
    }
}

#[skuld::test]
fn errors_name_the_guest_source_exactly_as_typed(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    for typed in ["~/missing", "/missing/", "/a/../missing", "/d//missing", "/./missing"] {
        let error = env.copy_out(typed, &env.local.join("m")).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
        let wanted = format!("the guest source {typed} does not exist");
        assert!(error.message().contains(&wanted), "{wanted:?} is not in: {error}");
    }
}

#[skuld::test]
fn errors_name_the_host_destination_exactly_as_typed(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    guest_project(&env);
    env.host_file("out.txt", b"x");
    env.host_file("out/sub", b"x");
    let local = env.local.display().to_string();
    let sep = std::path::MAIN_SEPARATOR;

    for typed in [
        format!("{local}{sep}out.txt{sep}"),
        format!("{local}{sep}x{sep}..{sep}out.txt"),
        format!("{local}{sep}{sep}out.txt"),
        format!("{local}{sep}.{sep}out.txt"),
    ] {
        let error = env.copy_out("/proj", Path::new(&typed)).unwrap_err();
        let wanted = format!("onto the host path {typed}, which is not a directory");
        assert!(error.message().contains(&wanted), "{wanted:?} is not in: {error}");
    }
    for typed in [
        format!("{local}{sep}out{sep}"),
        format!("{local}{sep}x{sep}..{sep}out"),
        format!("{local}{sep}{sep}out"),
        format!("{local}{sep}.{sep}out"),
    ] {
        let error = env.copy_out("/proj", Path::new(&typed)).unwrap_err();
        let wanted = format!("onto {}{sep}sub on this host", typed.trim_end_matches(sep));
        assert!(error.message().contains(&wanted), "{wanted:?} is not in: {error}");
    }
}

/// The same with a guest whose server is rooted elsewhere (OpenSSH's, in CI): the root is not the user's.
#[skuld::test]
fn errors_do_not_name_the_directory_the_server_is_rooted_at(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    let file = env.host_file("f.txt", b"f");
    env.copy_in(&file, "/in.txt").unwrap();
    let root = env.root().display().to_string();
    let proj = project(&env);
    for error in [
        env.copy_out("/missing", &env.local.join("m")).unwrap_err(),
        env.copy_in(&proj, "/in.txt").unwrap_err(),
        env.copy_in(&file, "/in.txt/below").unwrap_err(),
    ] {
        assert!(!error.message().contains(&root), "{root} is named: {error}");
        assert!(
            error.message().contains("/in.txt") || error.message().contains("/missing"),
            "{error}"
        );
    }
}

// Refused before anything is written ==================================================================================

/// Windows hosts cannot hold the names this test needs, so it runs where they can be made.
#[cfg(unix)]
#[skuld::test]
fn a_name_a_windows_guest_cannot_store_is_refused_before_anything_is_written(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.guest = env.os(GuestKind::Windows);
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

#[skuld::test(requires = [scripted_server])]
fn a_guest_time_the_server_does_not_show_is_named_and_the_copy_goes_on(#[fixture(unicode_home)] home: &Path) {
    let env = Env::with(home, |options| options.no_times = true);
    env.guest_file("g/a.txt", b"a");
    env.guest_file("g/sub/b.txt", b"b");
    let out = env.local.join("out");
    env.copy_out("/g", &out).unwrap();
    assert_eq!(tree(&out), ["a.txt", "sub/", "sub/b.txt"]);
    let notes: Vec<String> = env
        .progress
        .messages()
        .into_iter()
        .filter(|line| line.contains("modification time"))
        .collect();
    assert_eq!(notes.len(), 4, "{notes:?}");
    for named in ["/g", "/g/a.txt", "/g/sub", "/g/sub/b.txt"] {
        assert_eq!(
            notes.iter().filter(|line| line.contains(&format!("{named} "))).count(),
            1,
            "{named}: {notes:?}"
        );
    }
}

// Checks before anything is written ===================================================================================

#[skuld::test(requires = [scripted_server])]
fn links_a_guest_cannot_store_fail_the_copy_before_anything_is_written(#[fixture(unicode_home)] home: &Path) {
    let env = Env::with(home, |options| options.no_symlinks = true);
    // The plain file sorts before the link and must not arrive.
    env.host_file("t/a.txt", b"a");
    let link = env.local.join("t").join("ln");
    symlink(Path::new("a.txt"), &link);
    let error = env.copy_in(&env.local.join("t"), "/dest").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains(&link.display().to_string()), "{error}");
    assert!(error.message().contains("refused a test symlink"), "{error}");
    assert!(!env.root().join("dest").exists(), "nothing was created");
    // A link given as the source is checked too.
    let error = env.copy_in(&link, "/lone").unwrap_err();
    assert!(error.message().contains("refused a test symlink"), "{error}");
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
    env.guest_link("/g/ln", "a.txt");
    let out = env.local.join("out");
    let error = env.copy_out("/g", &out).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("/g/ln"), "{error}");
    assert!(error.message().contains("refused a test symlink"), "{error}");
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
    assert_eq!(env.guest_links().len(), 1);
    env.guest_file("g/a.txt", b"a");
    env.guest_link("/g/ln", "a.txt");
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
    env.guest_link("/dest/lnk", "/real");
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
#[skuld::test(requires = [scripted_server])]
fn a_clash_is_reported_before_the_guest_link_probe_makes_anything(#[fixture(unicode_home)] home: &Path) {
    let env = Env::with(home, |options| options.no_symlinks = true);
    env.host_file("t/x", b"file");
    symlink(Path::new("x"), &env.local.join("t").join("ln"));
    std::fs::create_dir_all(env.root().join("d").join("x")).unwrap();
    let error = env.copy_in(&env.local.join("t"), "/d").unwrap_err();
    assert!(error.message().contains("/d/x"), "{error}");
    assert!(!error.message().contains("refused a test symlink"), "{error}");
}

#[skuld::test]
fn a_clash_is_reported_before_the_host_link_probe_makes_anything(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.host = RecordingHost::without_symlinks();
    env.guest_file("g/x", b"file");
    env.guest_link("/g/ln", "x");
    let out = env.local.join("out");
    std::fs::create_dir_all(out.join("x")).unwrap();
    let error = env.copy_out("/g", &out).unwrap_err();
    assert!(
        error.message().contains(&out.join("x").display().to_string()),
        "{error}"
    );
    assert!(!error.message().contains("refused a test symlink"), "{error}");
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
    unix_files::make_fifo(&fifo);
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
    env.guest_special_file("/g/z");
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
    env.guest = env.os(GuestKind::Windows);
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

/// `C:a` is relative to the current directory of drive `C:`: a copy out of it is refused, naming it, like a copy into it,
/// and nothing is written on the host.
#[skuld::test]
fn a_drive_relative_guest_source_is_refused_naming_it(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.guest = env.os(GuestKind::Windows);
    let dest = env.local.join("out");
    for src in ["C:a", "c:a/b", "D:.."] {
        let error = env.copy_out(src, &dest).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Usage, "{src}: {error}");
        assert!(error.message().contains(src), "{src}: {error}");
        assert!(!dest.exists(), "{src}: nothing was written");
    }
}

/// The guest's rules apply to the host's names through the guest, so a case pair is one name on a Windows guest. A host
/// that folds case (Windows, macOS) cannot hold the pair to begin with.
#[cfg(target_os = "linux")]
#[skuld::test]
fn a_windows_guest_takes_two_host_names_differing_in_case_for_one(#[fixture(unicode_home)] home: &Path) {
    let mut env = Env::new(home);
    env.guest = env.os(GuestKind::Windows);
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
        "the time is past 2100: the wrapped time was applied"
    );
}

// Failures ============================================================================================================

#[skuld::test(requires = [scripted_server])]
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
    env.make_guest_dir_read_only("/ro");
    let src = env.host_file("src.txt", b"new");
    let error = env.copy_in(&src, "/ro/x.txt").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("Permission denied"), "{error}");
    assert!(error.message().contains("/ro"), "{error}");
    assert!(tree(&env.root().join("ro")).is_empty());
}

// Which side is named =================================================================================================

#[skuld::test(requires = [scripted_server])]
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

#[skuld::test(requires = [scripted_server])]
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
    env.guest_file_with_bad_name("/t");
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
        env.guest_named_file(&format!("/t{i}/{name}"), 3);
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
    env.guest_named_file("/t/Readme", 3);
    env.guest_named_file("/t/README", 3);
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
    env.guest_named_file("/con", 3);
    let dest = env.local.join("dest");
    std::fs::create_dir(&dest).unwrap();
    let error = env.copy_out("/con", &dest).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("\"con\""), "{error}");
    assert!(tree(&dest).is_empty());
}

/// A server answers a long listing in several replies, and the copy takes every one of them. The scripted server is
/// told to answer four entries at a time; OpenSSH's cuts a listing of this size by itself.
#[skuld::test]
fn a_directory_listed_in_several_batches_is_copied_whole(#[fixture(unicode_home)] home: &Path) {
    let env = Env::with(home, |options| options.readdir_batch = Some(4));
    let names: Vec<String> = (0..300).map(|i| format!("f{i:03}.txt")).collect();
    for name in &names {
        env.guest_file(&format!("t/{name}"), name.as_bytes());
    }
    env.guest_file("t/sub/inner.txt", b"i");
    let out = env.local.join("out");
    env.copy_out("/t", &out).unwrap();
    let mut want: Vec<String> = names.clone();
    want.extend(["sub/".to_owned(), "sub/inner.txt".to_owned()]);
    want.sort();
    assert_eq!(tree(&out), want);
}

/// A name that comes again in a later batch is a duplicate of the first, though no one reply holds both.
#[skuld::test(requires = [scripted_server])]
fn a_name_listed_again_in_a_later_batch_is_refused(#[fixture(unicode_home)] home: &Path) {
    let env = Env::with(home, |options| options.readdir_batch = Some(1));
    for name in ["a", "b", "c"] {
        env.guest_file(&format!("t/{name}"), b"x");
    }
    env.rig.sshd.list_again("/t/a");
    let error = env.copy_out("/t", &env.local.join("out")).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("names \"a\" twice"), "{error}");
    assert!(!env.local.join("out").exists());
}

/// A guest that refuses the rename extension fails the copy naming it, and the temporary file does not stay.
#[skuld::test(requires = [scripted_server])]
fn a_guest_that_refuses_posix_rename_fails_the_copy_and_leaves_no_temporary(#[fixture(unicode_home)] home: &Path) {
    let env = Env::with(home, |options| options.no_posix_rename = true);
    let file = env.host_file("f.txt", b"new");
    env.guest_file("old.txt", b"old");
    let error = env.copy_in(&file, "/old.txt").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert!(error.message().contains("cannot move"), "{error}");
    assert_eq!(std::fs::read(env.root().join("old.txt")).unwrap(), b"old");
    assert_eq!(tree(env.root()), ["old.txt"], "the temporary is gone");

    let proj = project(&env);
    let error = env.copy_in(&proj, "/dest").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "{error}");
    assert_eq!(
        tree(env.root()),
        ["dest/", "old.txt"],
        "the directory was made, no file or temporary"
    );
}

// Guest requests ======================================================================================================

/// Asks the guest side to make the directory `path`.
fn guest_create_dir(env: &Env, path: &str) -> Result<bool> {
    let path = env.g(path);
    env.sftp
        .call("creating a directory", move |remote| remote.create_dir(path))
}

#[skuld::test]
fn the_guest_makes_missing_parents_and_says_whether_it_made_the_directory(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    assert!(guest_create_dir(&env, "/a/b/c").unwrap(), "made, with its parents");
    assert!(env.root().join("a/b/c").is_dir());
    assert!(!guest_create_dir(&env, "/a/b/c").unwrap(), "it was there already");
    assert!(!guest_create_dir(&env, "/a/b").unwrap(), "so was its parent");
    assert!(
        guest_create_dir(&env, "/a/b/d").unwrap(),
        "a sibling next to what exists"
    );
}

#[skuld::test]
fn the_guest_refuses_a_file_in_the_way_of_a_directory_naming_it(#[fixture(unicode_home)] home: &Path) {
    let env = Env::new(home);
    env.guest_file("a/file", b"x");
    for path in ["/a/file", "/a/file/below"] {
        let error = guest_create_dir(&env, path).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Io, "{error}");
        assert!(error.message().contains("/a/file"), "{error}");
    }
}

#[skuld::test(requires = [scripted_server])]
fn a_guest_that_has_not_even_a_root_ends_the_walk_up_with_an_error(#[fixture(unicode_home)] home: &Path) {
    let env = Env::with(home, |options| options.hollow = true);
    for path in ["/a/b", "a/b", "."] {
        let error = guest_create_dir(&env, path).unwrap_err();
        assert!(
            error.message().contains("cannot create the guest directory"),
            "{path}: {error}"
        );
    }
}

#[skuld::test(requires = [scripted_server])]
fn the_guest_temporary_is_opened_so_that_it_cannot_be_a_file_that_is_already_there(
    #[fixture(unicode_home)] home: &Path,
) {
    let env = Env::new(home);
    let src = env.host_file("a.txt", b"hello");
    env.copy_in(&src, "/dest.txt").unwrap();
    let opened = env.rig.sshd.opened();
    let partials: Vec<_> = opened
        .iter()
        .filter(|(path, _)| path.contains(".devvm-partial-"))
        .collect();
    assert_eq!(partials.len(), 1, "{opened:?}");
    assert!(
        partials[0].1.contains(russh_sftp::protocol::OpenFlags::EXCLUDE),
        "the temporary is created with EXCLUDE: {:?}",
        partials[0].1
    );
}

#[skuld::test(requires = [scripted_server])]
fn an_sftp_channel_that_closes_on_a_live_connection_is_not_a_lost_connection(#[fixture(unicode_home)] home: &Path) {
    let env = Env::with(home, |options| options.close_sftp_after = Some(100 * 1024));
    let big = env.host_file("big.bin", &vec![7u8; 2 << 20]);
    let error = env.copy_in(&big, "/dest.bin").unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "the connection is up: {error}");
    assert!(
        error.message().contains("SFTP"),
        "the error names the SFTP failure: {error}"
    );
    assert!(!env.root().join("dest.bin").exists());
}

#[skuld::test(requires = [scripted_server])]
fn a_step_waiting_on_an_sftp_channel_that_has_ended_ends_with_the_channel_failure(
    #[fixture(unicode_home)] home: &Path,
) {
    // The SFTP library does not answer a request registered after the end of its stream was handled (measured: 2-3 % of
    // runs of the test above), so the end seen at the stream must end the step.
    let env = Env::new(home);
    env.sftp
        .remote
        .channel_end
        .end("the peer closed the SFTP channel".to_owned());
    let error = env
        .sftp
        .call("waiting for a reply that cannot come", |_| {
            std::future::pending::<Result<()>>()
        })
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Io, "the connection is up: {error}");
    assert!(error.message().contains("SFTP channel"), "{error}");
    assert!(
        error.message().contains("waiting for a reply that cannot come"),
        "{error}"
    );
}
