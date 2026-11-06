use std::path::{Path, PathBuf};

use crate::ErrorKind;
use crate::host;
use crate::instance::locks::Locks;
use crate::store::lock::FileLock;
use crate::test_groups::{Group, exfat_volume};

/// The drive letter of the volume. The facts spike used the same one on the runner.
const LETTER: &str = "R";

/// A virtual disk formatted exFAT and mounted at `R:`, detached when dropped.
struct ExfatVolume {
    script_dir: tempfile::TempDir,
    file: PathBuf,
}

/// Runs the diskpart script `lines`, which fails the test unless diskpart succeeds.
fn diskpart(script_dir: &Path, lines: &[String]) {
    let script = script_dir.join("script.txt");
    std::fs::write(&script, lines.join("\r\n")).unwrap();
    let mut cmd = std::process::Command::new("diskpart");
    cmd.arg("/s").arg(&script);
    host::current().quiet(&mut cmd);
    let output = cmd.output().unwrap();
    assert!(
        output.status.success(),
        "diskpart {lines:?}: {}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

impl ExfatVolume {
    fn attach() -> ExfatVolume {
        let script_dir = tempfile::tempdir().unwrap();
        let file = script_dir.path().join("exfat.vhd");
        diskpart(
            script_dir.path(),
            &[
                format!("create vdisk file=\"{}\" maximum=256 type=expandable", file.display()),
                "attach vdisk".to_owned(),
                "create partition primary".to_owned(),
                "format fs=exfat quick label=devvm".to_owned(),
                format!("assign letter={LETTER}"),
            ],
        );
        ExfatVolume { script_dir, file }
    }

    fn root(&self) -> PathBuf {
        PathBuf::from(format!("{LETTER}:\\"))
    }
}

impl Drop for ExfatVolume {
    fn drop(&mut self) {
        let lines = [
            format!("select vdisk file=\"{}\"", self.file.display()),
            "detach vdisk".to_owned(),
        ];
        // Not a panic: the test's own failure is the one to report, and the runner is thrown away.
        let script = self.script_dir.path().join("detach.txt");
        let _ = std::fs::write(&script, lines.join("\r\n"));
        let mut cmd = std::process::Command::new("diskpart");
        cmd.arg("/s").arg(&script);
        host::current().quiet(&mut cmd);
        let _ = cmd.output();
    }
}

/// CI only: it attaches a disk to the machine it runs on.
#[skuld::test]
fn an_exfat_volume_has_no_posix_unlink_rename_and_a_lock_take_there_is_not_supported(
    #[fixture(exfat_volume)] _group: &Group,
) {
    let volume = ExfatVolume::attach();
    let state = volume.root().join("state");
    std::fs::create_dir(&state).unwrap();

    let lock = FileLock::open(&state.join("probe.lock")).unwrap();
    assert!(!lock.volume_has_posix_unlink_rename().unwrap());

    let locks = Locks::new(state.join("locks"), host::current());
    let error = locks.excluding_readers(&mut || {}, || ()).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotSupported, "{error}");
    assert!(error.message().contains("NTFS or ReFS"), "{}", error.message());
}
