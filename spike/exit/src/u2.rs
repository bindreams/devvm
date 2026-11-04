//! U2: `std::fs::remove_file` on a file with a second handle open; is the name gone at once, or delete-pending?
use std::collections::BTreeMap;
use std::fs::File;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;

use windows_sys::Win32::Storage::FileSystem::{
    FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, GetVolumeInformationByHandleW,
};

const ITERATIONS: u64 = 1000;

#[derive(Clone, Copy)]
enum Variant {
    /// The creating handle is closed; the second handle shares FILE_SHARE_DELETE only.
    CreatorClosedShareDelete,
    /// The creating handle is closed; the second handle shares read, write and delete.
    CreatorClosedShareAll,
    /// The creating handle stays open next to the second one (shares read, write and delete).
    CreatorOpenShareAll,
}

pub fn run(dirs: &[String]) {
    println!("RESULT u2 rustc={}", env!("RUSTC_VERSION"));
    for dir in dirs {
        let dir = Path::new(dir);
        std::fs::create_dir_all(dir).unwrap();
        for variant in [
            Variant::CreatorClosedShareDelete,
            Variant::CreatorClosedShareAll,
            Variant::CreatorOpenShareAll,
        ] {
            measure(dir, variant);
        }
    }
}

fn measure(dir: &Path, variant: Variant) {
    let mut outcomes: BTreeMap<String, u64> = BTreeMap::new();
    let mut filesystem = String::new();
    for i in 0..ITERATIONS {
        let path = dir.join(format!("u2-{i}.tmp"));
        let creator = File::create_new(&path).unwrap();
        filesystem = filesystem_name(&creator);
        let share = match variant {
            Variant::CreatorClosedShareDelete => FILE_SHARE_DELETE,
            _ => FILE_SHARE_DELETE | FILE_SHARE_READ | FILE_SHARE_WRITE,
        };
        let creator = match variant {
            Variant::CreatorOpenShareAll => Some(creator),
            _ => {
                drop(creator);
                None
            }
        };
        let second = File::options().read(true).share_mode(share).open(&path).unwrap();

        let removed = match std::fs::remove_file(&path) {
            Ok(()) => "remove_ok".to_string(),
            Err(e) => format!("remove_err_{}", e.raw_os_error().unwrap_or(-1)),
        };
        let listed = std::fs::read_dir(dir)
            .unwrap()
            .any(|entry| entry.unwrap().file_name() == path.file_name().unwrap());
        let created = match File::options().write(true).create_new(true).open(&path) {
            Ok(file) => {
                drop(file);
                "create_ok".to_string()
            }
            Err(e) => format!("create_err_{}", e.raw_os_error().unwrap_or(-1)),
        };
        let outcome = format!("{removed} listed={listed} {created}");
        *outcomes.entry(outcome).or_default() += 1;

        drop(second);
        drop(creator);
        let _ = std::fs::remove_file(&path);
    }
    let variant = match variant {
        Variant::CreatorClosedShareDelete => "creator_closed_share_delete",
        Variant::CreatorClosedShareAll => "creator_closed_share_all",
        Variant::CreatorOpenShareAll => "creator_open_share_all",
    };
    println!(
        "RESULT u2 dir={} filesystem={filesystem} variant={variant} iterations={ITERATIONS}",
        dir.display()
    );
    for (outcome, count) in outcomes {
        println!("RESULT u2   {count}x {outcome}");
    }
}

fn filesystem_name(file: &File) -> String {
    let mut name = [0u16; 64];
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle(),
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    };
    assert_ne!(ok, 0);
    let len = name.iter().position(|&c| c == 0).unwrap();
    String::from_utf16_lossy(&name[..len])
}
