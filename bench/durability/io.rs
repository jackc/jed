//! Dependency-free byte-device seam shared by the durability experiments.
use std::io;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Write {
    pub offset: u64,
    pub bytes: Vec<u8>,
}

pub trait Device {
    fn read(&mut self, offset: u64, len: usize) -> io::Result<Vec<u8>>;
    fn write(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()>;
    fn sync(&mut self) -> io::Result<()>;
}
