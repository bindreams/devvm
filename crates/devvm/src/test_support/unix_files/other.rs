use std::path::Path;

pub fn make_fifo(_path: &Path) {
    unimplemented!("a FIFO needs Unix");
}

pub fn make_file_with_name_bytes(_dir: &Path, _name: &[u8]) {
    unimplemented!("a name that is not UTF-8 needs Unix");
}

pub fn make_symlink_with_target_bytes(_target: &[u8], _link: &Path) {
    unimplemented!("a link target that is not UTF-8 needs Unix");
}
