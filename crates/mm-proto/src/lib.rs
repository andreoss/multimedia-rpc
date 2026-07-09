pub mod convert;
pub mod merge;

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/mmrpc.v1.rs"));
}

pub mod vm {
    pub mod v1 {
        include!(concat!(env!("OUT_DIR"), "/mmrpc.videomerge.v1.rs"));
    }
}

pub use merge::*;
