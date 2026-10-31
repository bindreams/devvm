//! What the tests ask the OS about a process they did not write. The VM tests need WHPX, so they fail on this host before
//! they ask; these answers only keep the suite compiling.

pub struct Watch;

impl Watch {
    pub fn new(_pid: u32) -> Option<Watch> {
        None
    }

    pub fn exit_code(&self) -> Option<u32> {
        None
    }
}

pub fn windows_of(_pid: u32) -> usize {
    0
}

pub fn children_of(_pid: u32) -> usize {
    0
}
