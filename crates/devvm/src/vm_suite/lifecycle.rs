//! An instance's life through the library, in real guests: reaching it with a plain `ssh`, its host key across a stop
//! and a reboot, concurrent `up` and `rm`, and `cp` of links and trees. The same life through the `devvm` executable is
//! `devvm-cli/tests/vm_cli.rs`'s.

use std::path::Path;
use std::process::Stdio;

use super::fixture::{SharedGuest, Up, VmEngine, guest_options, run_ok, shared_guest, small_options, vm_engine};
use crate::access::sftp::sftp_tests::tree;
use crate::host;
use crate::test_groups::{Group, symlinks};
use crate::test_support::fixtures::unicode_home;
use crate::test_support::symlink::symlink;
use crate::{Command, ErrorKind, Guest, ListFilter};

/// The guest's SSH host key, as `<type> <base64>`.
fn host_key(guest: &dyn Guest) -> String {
    let text = run_ok(guest, &["cat", "/etc/ssh/ssh_host_ed25519_key.pub"]);
    let fields: Vec<&str> = text.split_whitespace().take(2).collect();
    assert_eq!(fields.len(), 2, "no host key in {text:?}");
    fields.join(" ")
}

fn boot_id(guest: &dyn Guest) -> String {
    run_ok(guest, &["cat", "/proc/sys/kernel/random/boot_id"])
        .trim()
        .to_owned()
}

#[skuld::test]
fn vm_cp_round_trips_symlinks_and_a_tree_against_the_guests_openssh_sftp_server(
    #[fixture(shared_guest)] guest: &SharedGuest,
    #[fixture(unicode_home)] home: &Path,
    #[fixture(symlinks)] _group: &Group,
) {
    let pack = home.join("pack");
    std::fs::create_dir_all(pack.join("sub")).unwrap();
    std::fs::write(pack.join("real.txt"), b"real").unwrap();
    std::fs::write(pack.join("sub").join("with space \u{416}.txt"), b"nested").unwrap();
    for (name, target) in [("file-link", "real.txt"), ("dir-link", "sub"), ("dangling", "nowhere")] {
        symlink(Path::new(target), &pack.join(name));
    }
    let want = tree(&pack);
    assert!(want.contains(&"dangling -> nowhere".to_owned()), "{want:?}");

    guest.vm.copy_in(&pack, "~/pack").unwrap();
    let link = run_ok(&guest.vm, &["sh", "-c", "readlink ~/pack/dangling"]);
    assert_eq!(link, "nowhere\n");
    guest.vm.copy_out("~/pack", &home.join("back")).unwrap();

    assert_eq!(tree(&home.join("back")), want);
    assert_eq!(std::fs::read(home.join("back").join("real.txt")).unwrap(), b"real");
    run_ok(&guest.vm, &["sh", "-c", "rm -rf ~/pack"]);
}

#[skuld::test]
fn plain_ssh_with_the_generated_config_connects_without_a_prompt(#[fixture(vm_engine)] vm: &VmEngine) {
    let image = vm.base();
    let up = Up::new(&image, "plain-ssh", &guest_options()).unwrap();

    let config = up.ssh_config().unwrap();
    let mut command = std::process::Command::new("ssh");
    command
        .arg("-F")
        .arg(&config)
        .args(["-o", "BatchMode=yes"])
        .arg(up.name())
        .arg("true")
        .stdin(Stdio::null());
    host::current().quiet(&mut command);
    let output = command.output().unwrap_or_else(|e| panic!("cannot run ssh: {e}"));

    assert!(
        output.status.success(),
        "ssh -F {} {} true: {} {}",
        config.display(),
        up.name(),
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[skuld::test]
fn two_instances_of_one_image_have_different_host_keys(#[fixture(vm_engine)] vm: &VmEngine) {
    let image = vm.base();
    let first = Up::new(&image, "keys-one", &small_options()).unwrap();
    let second = Up::new(&image, "keys-two", &small_options()).unwrap();

    assert_ne!(host_key(&*first), host_key(&*second));
}

#[skuld::test]
fn concurrent_up_and_rm_leave_every_live_instance_resolvable(#[fixture(vm_engine)] vm: &VmEngine) {
    let image = vm.base();
    let opts = small_options();

    let (gone, kept_b, kept_c) = std::thread::scope(|scope| {
        let a = scope.spawn(|| Up::new(&image, "conc-a", &opts).and_then(Up::remove));
        let b = scope.spawn(|| Up::new(&image, "conc-b", &opts));
        let c = scope.spawn(|| Up::new(&image, "conc-c", &opts));
        (a.join().unwrap(), b.join().unwrap(), c.join().unwrap())
    });
    gone.unwrap();
    let (kept_b, kept_c) = (kept_b.unwrap(), kept_c.unwrap());

    let error = vm.engine.instance("conc-a").err().expect("conc-a was removed");
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    let listed: Vec<String> = vm
        .engine
        .list(ListFilter::All)
        .unwrap()
        .into_iter()
        .map(|info| info.name)
        .collect();
    for (name, up) in [("conc-b", &kept_b), ("conc-c", &kept_c)] {
        assert!(
            listed.iter().any(|listed| listed == name),
            "{name} is not in {listed:?}"
        );
        let found = vm.engine.instance(name).unwrap();
        assert_eq!(found.name(), name);
        run_ok(&**up, &["true"]);
    }
    assert!(!listed.iter().any(|name| name == "conc-a"), "{listed:?}");
}

#[skuld::test]
fn vm_reboot_changes_the_boot_id_keeps_the_instance_its_host_key_and_the_qemu_process_and_ssh_answers(
    #[fixture(vm_engine)] vm: &VmEngine,
) {
    let image = vm.base();
    let up = Up::new(&image, "reboot-up", &guest_options()).unwrap();
    let machine = || {
        let machine = up
            .dir()
            .record()
            .unwrap()
            .machine
            .expect("a running instance has a machine");
        let ports: Vec<_> = machine.ssh.iter().map(|endpoint| endpoint.addr).collect();
        (machine.process, ports)
    };
    let (process, ports) = machine();
    let (id, key) = (boot_id(&*up), host_key(&*up));

    up.reboot().unwrap();

    assert_ne!(boot_id(&*up), id, "the guest did not boot again");
    assert_eq!(host_key(&*up), key);
    assert_eq!(
        (process, ports),
        machine(),
        "the machine is not the one that was running"
    );
    assert_eq!(vm.engine.instance("reboot-up").unwrap().name(), "reboot-up");
}

#[skuld::test]
fn vm_stop_then_start_keeps_the_host_key(#[fixture(vm_engine)] vm: &VmEngine) {
    let image = vm.base();
    let up = Up::new(&image, "stop-start", &guest_options()).unwrap();
    let key = host_key(&*up);

    up.stop().unwrap();
    let error = up.run(&Command::new(["true"])).unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    up.start().unwrap();

    assert_eq!(host_key(&*up), key);
}
