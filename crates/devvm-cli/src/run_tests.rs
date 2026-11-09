use std::io::Write;
use std::path::{Path, PathBuf};

use devvm::{Arch, ErrorKind, Network, RunAs};

use super::{Streaming, Transfer, build_options, copy_direction, guest_command, start_options};
use crate::args::{CpSide, VmFlags};

fn local(path: &str) -> CpSide {
    CpSide::Local(PathBuf::from(path))
}

fn guest(instance: &str, path: &str) -> CpSide {
    CpSide::Guest {
        instance: instance.to_owned(),
        path: path.to_owned(),
    }
}

#[skuld::test]
fn cp_goes_in_for_a_local_source_and_out_for_a_guest_source() {
    assert_eq!(
        copy_direction(&local("a.txt"), &guest("box", "~/a.txt")).unwrap(),
        Transfer::In {
            instance: "box",
            from: Path::new("a.txt"),
            to: "~/a.txt"
        }
    );
    assert_eq!(
        copy_direction(&guest("box", "/etc/hosts"), &local("out")).unwrap(),
        Transfer::Out {
            instance: "box",
            from: "/etc/hosts",
            to: Path::new("out")
        }
    );
}

#[skuld::test]
fn cp_refuses_two_local_sides_and_two_guest_sides_as_usage_errors() {
    for (src, dst) in [
        (local("a"), local("b")),
        (guest("one", "a"), guest("two", "b")),
        (guest("one", "a"), guest("one", "b")),
    ] {
        let error = copy_direction(&src, &dst).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Usage, "{src:?} {dst:?}");
    }
}

#[skuld::test]
fn the_flags_become_the_build_and_start_options() {
    let flags = VmFlags {
        arch: Some(Arch::Arm64),
        cpus: Some(3),
        memory: Some(1 << 30),
        network: Network::None,
    };
    assert_eq!(build_options(&flags).arch, Some(Arch::Arm64));
    let start = start_options(&flags);
    assert_eq!(
        (start.cpus, start.memory, start.network),
        (Some(3), Some(1 << 30), Network::None)
    );
}

#[skuld::test]
fn elevated_runs_the_command_as_root_and_nothing_else_changes() {
    let argv = vec!["id".to_owned(), "-u".to_owned()];
    let user = guest_command(&argv, false);
    let root = guest_command(&argv, true);
    assert_eq!((user.run_as, root.run_as), (RunAs::User, RunAs::Elevated));
    assert_eq!((&user.argv, &root.argv, &user.cwd), (&argv, &argv, &None));
}

/// A writer that keeps what was written and counts the flushes after it.
#[derive(Default)]
struct Recorder {
    written: Vec<u8>,
    flushed_at: Vec<usize>,
}

impl Write for Recorder {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.written.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.flushed_at.push(self.written.len());
        Ok(())
    }
}

#[skuld::test]
fn every_write_is_flushed_before_the_next_one_starts() {
    let mut out = Streaming(Recorder::default());
    out.write_all(b"prompt> ").unwrap();
    out.write_all(b"no line break").unwrap();
    assert_eq!(out.0.written, b"prompt> no line break");
    assert_eq!(out.0.flushed_at, [8, 21]);
}

#[skuld::test]
fn a_failed_write_is_the_error_and_flushes_nothing() {
    struct Closed;
    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            panic!("nothing was written, so nothing is flushed");
        }
    }
    let error = Streaming(Closed).write_all(b"x").unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
}
