//! Generated from proto/ by build.rs. The module tree mirrors the protobuf packages,
//! which the generated code refers to through `super::` paths.
#![allow(clippy::all, dead_code)]

pub mod securegcm {
    include!(concat!(env!("OUT_DIR"), "/securegcm.rs"));
}
pub mod securemessage {
    include!(concat!(env!("OUT_DIR"), "/securemessage.rs"));
}
pub mod location {
    pub mod nearby {
        pub mod connections {
            include!(concat!(env!("OUT_DIR"), "/location.nearby.connections.rs"));
        }
        pub mod proto {
            pub mod sharing {
                include!(concat!(env!("OUT_DIR"), "/location.nearby.proto.sharing.rs"));
            }
        }
    }
}
pub mod sharing {
    pub mod nearby {
        include!(concat!(env!("OUT_DIR"), "/sharing.nearby.rs"));
    }
}

pub use location::nearby::connections;
pub use sharing::nearby as share;
