use std::io::Read;
use std::process::{Child, Stdio};
use std::sync::mpsc;

use cosca::identity::ProcessIdRecord;

use crate::contain::VmProcess;
use crate::test_support::helper;

/// Helper process: blocks until stdin closes.
pub fn sleeper(_args: &[String]) {
    let mut sink = Vec::new();
    std::io::stdin().read_to_end(&mut sink).unwrap();
}

/// A running `sleeper`; closing its stdin lets it exit.
struct Sleeper {
    child: Child,
}

impl Sleeper {
    fn spawn() -> Sleeper {
        let child = helper::command("sleeper", &[])
            .stdin(Stdio::piped())
            .spawn()
            .expect("the sleeper should start");
        Sleeper { child }
    }

    fn record(&self) -> ProcessIdRecord {
        let process = cosca::Process::from_pid(self.child.id())
            .found()
            .expect("the sleeper is alive");
        process.id().to_record().expect("an identity record")
    }

    /// Lets the sleeper exit and reaps it.
    fn finish(mut self) {
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
