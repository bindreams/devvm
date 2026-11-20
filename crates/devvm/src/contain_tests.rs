use std::io::Read;
use std::process::{Child, Stdio};
use std::sync::mpsc;

use cosca::error::RecordErrorKind;
use cosca::identity::{Liveness, ProcessIdRecord};

use crate::contain::{Lifetime, VmCommand, VmProcess, frozen_spawn_error, refused_record, unknown_liveness};
use crate::test_support::helper;
use crate::{ErrorKind, Result};

impl VmProcess {
    /// Whether the process is still running; an error when the OS refuses to say (never "dead").
    pub(crate) fn is_alive(&self) -> Result<bool> {
        let id = self.id();
        match id.is_alive() {
            Liveness::Alive => Ok(true),
            Liveness::Dead => Ok(false),
            Liveness::Unknown => Err(unknown_liveness(id)),
        }
    }
}

/// Helper process: blocks until stdin closes.
pub fn sleeper(_args: &[String]) {
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
}

/// A running `sleeper`; closing its stdin lets it exit.
pub(crate) struct Sleeper {
    pub(crate) child: Child,
}

impl Sleeper {
    pub(crate) fn spawn() -> Sleeper {
        let child = helper::command("sleeper", &[])
            .stdin(Stdio::piped())
            .spawn()
            .expect("the sleeper should start");
        Sleeper { child }
    }

    pub(crate) fn record(&self) -> ProcessIdRecord {
        let id = cosca::identity::ProcessId::of(self.child.id())
            .found()
            .expect("the sleeper is alive");
        id.to_record().expect("an identity record")
    }

    /// Lets the sleeper exit and reaps it.
    pub(crate) fn finish(mut self) {
        drop(self.child.stdin.take());
        self.child.wait().unwrap();
    }
}

impl Drop for Sleeper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[skuld::test]
fn a_relative_program_is_refused_before_anything_is_created() {
    let dir = skuld::TempDir::new().unwrap();
    let log = dir.path().join("vm.log");
    // A bare name, and on Windows a root-relative and a drive-relative path: none is absolute.
    for program in ["qemu", r"\qemu.exe", "C:qemu.exe"] {
        let command = VmCommand {
            args: Vec::new(),
            clear_env: true,
            env: Vec::new(),
            cwd: None,
        };
        let error = VmProcess::spawn_frozen(
            crate::host::current(),
            program.as_ref(),
            command,
            Lifetime::DiesWithOwner,
            &log,
        )
        .err()
        .unwrap_or_else(|| panic!("{program} is refused"));
        assert_eq!(error.kind(), ErrorKind::Internal, "{program}: {error}");
        assert!(error.to_string().contains(program), "{error}");
        assert!(!log.exists(), "{program}");
    }
}

#[skuld::test]
fn kill_returns_only_after_the_process_has_exited() {
    let sleeper = Sleeper::spawn();
    let mut vm = VmProcess::attach(&sleeper.record())
        .unwrap()
        .expect("a live process attaches");
    assert!(vm.is_alive().unwrap());
    vm.kill().unwrap();
    assert!(
        !vm.is_alive().unwrap(),
        "kill must not return before the process has exited"
    );
    vm.kill().expect("killing a dead process is Ok");
}

#[skuld::test]
fn attach_refuses_a_record_whose_start_token_differs() {
    let sleeper = Sleeper::spawn();
    let mut record = sleeper.record();
    record.token ^= 1;
    assert!(VmProcess::attach(&record).unwrap().is_none());
    // The process the forged record pointed at is untouched.
    let mut vm = VmProcess::attach(&sleeper.record()).unwrap().unwrap();
    assert!(vm.is_alive().unwrap());
    vm.kill().unwrap();
}

#[skuld::test]
fn attach_returns_none_for_an_exited_process() {
    let sleeper = Sleeper::spawn();
    let record = sleeper.record();
    sleeper.finish();
    assert!(VmProcess::attach(&record).unwrap().is_none());
}

#[skuld::test]
fn attach_returns_none_for_a_record_from_another_platform() {
    let sleeper = Sleeper::spawn();
    let mut record = sleeper.record();
    record.platform = cosca::identity::Platform::Other("plan9".to_owned());
    assert!(VmProcess::attach(&record).unwrap().is_none());
}

#[skuld::test]
fn wait_on_an_attached_foreign_process_returns_when_it_exits() {
    let mut sleeper = Sleeper::spawn();
    let vm = VmProcess::attach(&sleeper.record()).unwrap().unwrap();
    let (done, waited) = mpsc::channel();
    let waiter = std::thread::spawn(move || {
        vm.wait().unwrap();
        done.send(()).unwrap();
    });
    drop(sleeper.child.stdin.take());
    waited.recv().expect("the wait should return once the sleeper exits");
    waiter.join().unwrap();
}

#[skuld::test]
fn record_names_the_attached_process() {
    let sleeper = Sleeper::spawn();
    let record = sleeper.record();
    let vm = VmProcess::attach(&record).unwrap().unwrap();
    assert_eq!(vm.record().unwrap(), record);
}

#[skuld::test]
fn a_host_that_cannot_freeze_a_process_is_not_supported_naming_the_host() {
    let refusal = cosca::error::Error::Unsupported {
        op: "spawn_suspended".into(),
        platform: "plan9",
        detail: "no gate before exec".into(),
        source: Some(std::sync::Arc::new(cosca::error::Error::Io(std::io::Error::other(
            "no such call",
        )))),
    };
    let error = frozen_spawn_error(refusal);
    assert_eq!(error.kind(), ErrorKind::NotSupported);
    assert!(
        error.message().contains("frozen spawn needs Windows in 0.1.0")
            && error.message().contains("plan9")
            && error.message().contains("no such call"),
        "{}",
        error.message()
    );
}

#[skuld::test]
fn any_other_frozen_spawn_failure_is_an_io_error_carrying_coscas_words() {
    let error = frozen_spawn_error(cosca::error::Error::Containment {
        detail: "no job".into(),
        source: None,
    });
    assert_eq!(error.kind(), ErrorKind::Io);
    assert!(error.message().contains("no job"), "{}", error.message());
}

#[skuld::test]
fn a_cosca_error_names_the_cause_cosca_keeps_as_its_source() {
    let cosca_error = || {
        let cause = cosca::error::Error::Containment {
            detail: "could not resume the frozen child".into(),
            source: Some(std::sync::Arc::new(cosca::error::Error::Io(std::io::Error::other(
                "access denied",
            )))),
        };
        cosca::error::Error::Containment {
            detail: "its child was reaped".into(),
            source: Some(std::sync::Arc::new(cause)),
        }
    };
    let report = cosca::error::Report::new(&cosca_error()).to_string();
    assert!(
        report.contains("access denied"),
        "the report reaches the deepest cause: {report}"
    );

    let error = crate::contain::cosca_error("cannot resume the VM process 7", cosca_error());

    assert_eq!(error.message(), format!("cannot resume the VM process 7: {report}"));
    let at = |layer: &str| {
        error
            .message()
            .find(layer)
            .unwrap_or_else(|| panic!("{layer:?} not in: {error}"))
    };
    assert!(
        at("its child was reaped") < at("could not resume the frozen child")
            && at("could not resume") < at("access denied"),
        "every cause, outermost first: {error}"
    );
}

fn refusal(kind: RecordErrorKind) -> cosca::error::Error {
    cosca::error::Error::IdentityRecord {
        kind,
        detail: "record v3 from windows for pid 7 (boot_id None vs live None)".into(),
        source: None,
    }
}

/// What a host that cannot read its own boot session refuses with.
pub(crate) fn scope_refusal() -> cosca::error::Error {
    cosca::error::Error::IdentityRecord {
        kind: RecordErrorKind::ScopeUnreadable,
        detail: "cannot read /proc/sys/kernel/random/boot_id".into(),
        source: Some(std::sync::Arc::new(cosca::error::Error::Io(std::io::Error::other(
            "access denied",
        )))),
    }
}

#[skuld::test]
fn a_record_from_another_boot_or_platform_names_nothing_that_runs() {
    for kind in [RecordErrorKind::ForeignBootSession, RecordErrorKind::ForeignPlatform] {
        assert!(refused_record(7, refusal(kind)).unwrap().is_none(), "{kind:?}");
    }
}

#[skuld::test]
fn a_record_that_cannot_be_checked_is_a_process_record_error_that_says_why() {
    for (kind, why) in [
        (RecordErrorKind::UnknownVersion, "a format this devvm does not read"),
        (RecordErrorKind::InvalidPid, "a pid that cannot name a process"),
        (RecordErrorKind::MissingBootSession, "lacks"),
        (RecordErrorKind::MissingPidNamespace, "lacks"),
        (RecordErrorKind::MissingTokenKind, "lacks"),
        (RecordErrorKind::ForeignTokenKind, "another kind of process token"),
        (RecordErrorKind::ForeignPidNamespace, "another pid namespace"),
    ] {
        let error = refused_record(7, refusal(kind)).err().expect("not read as not running");

        assert_eq!(error.kind(), ErrorKind::ProcessRecord, "{kind:?}");
        assert!(
            error.message().starts_with("the VM's process record"),
            "{kind:?}: reads on its own, with no lead: {error}"
        );
        assert!(error.message().contains(why), "{kind:?}: {error}");
        let report = cosca::error::Report::new(&refusal(kind)).to_string();
        assert!(
            error.message().ends_with(&format!(" ({report})")),
            "{kind:?}: cosca's words once, last: {error}"
        );
        assert_eq!(error.message().matches(&report).count(), 1, "{kind:?}: {error}");
        assert!(std::error::Error::source(&error).is_some(), "{kind:?}");
    }
}

#[skuld::test]
fn a_host_that_cannot_read_its_own_boot_session_is_an_io_error_not_a_bad_record() {
    let error = refused_record(7, scope_refusal())
        .err()
        .expect("liveness cannot be established");

    let cosca::error::Error::IdentityRecord {
        kind,
        detail,
        source: Some(cause),
    } = scope_refusal()
    else {
        unreachable!();
    };
    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        error.message(),
        format!(
            "cannot check the VM process 7: {kind}: {detail}: {}",
            cosca::error::Report::new(&*cause)
        ),
        "names the process it was checking, since the host's failure says nothing of the record"
    );
    for part in ["cannot check the VM process 7", "boot_id", "access denied"] {
        assert!(error.message().contains(part), "{part:?} not in: {error}");
    }
    assert!(std::error::Error::source(&error).is_some());
}

#[skuld::test]
fn a_cosca_error_that_is_no_record_fault_is_an_io_error_that_names_the_process_and_quotes_cosca() {
    let cosca = || cosca::error::Error::Containment {
        detail: "its child was reaped".into(),
        source: None,
    };

    let error = refused_record(7, cosca())
        .err()
        .expect("the process cannot be restored");

    assert_eq!(error.kind(), ErrorKind::Io);
    assert_eq!(
        error.message(),
        format!(
            "cannot restore the VM process 7: {}",
            cosca::error::Report::new(&cosca())
        )
    );
    assert!(std::error::Error::source(&error).is_some());
}
