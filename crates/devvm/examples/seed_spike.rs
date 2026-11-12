//! Spike only (never merged): writes the named files into a FAT image with devvm's `write_cidata_image`.
//! Usage: seed_spike <image> <file>...   (each file is stored under its own file name)

use fatfs::{FileSystem, FsOptions};

fn main() {
    let mut args = std::env::args().skip(1);
    let image = std::path::PathBuf::from(args.next().expect("image path"));
    let files: Vec<(String, Vec<u8>)> = args
        .map(|path| {
            let name = std::path::Path::new(&path)
                .file_name()
                .expect("file name")
                .to_string_lossy()
                .into_owned();
            (
                name,
                std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}")),
            )
        })
        .collect();
    let borrowed: Vec<(&str, Vec<u8>)> = files.iter().map(|(n, b)| (n.as_str(), b.clone())).collect();
    devvm::spike::write_seed(&borrowed, &image).expect("write_cidata_image");

    let fs = FileSystem::new(std::fs::File::open(&image).expect("open the image"), FsOptions::new()).expect("mount");
    println!(
        "image: {} ({} bytes)",
        image.display(),
        std::fs::metadata(&image).unwrap().len()
    );
    println!("fat type: {:?}", fs.fat_type());
    println!("volume label: {:?}", fs.volume_label());
    for entry in fs.root_dir().iter() {
        let entry = entry.expect("entry");
        println!("root entry: {:?} ({} bytes)", entry.file_name(), entry.len());
    }
}
