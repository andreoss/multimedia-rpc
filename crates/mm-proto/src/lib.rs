mod convert;

pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/mmrpc.v1.rs"));
}
