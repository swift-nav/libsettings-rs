mod client;
mod settings;

pub use client::{Client, Error, ReadSettingError, WriteSettingError};
pub use settings::{lookup_setting, settings, Setting, SettingKind, SettingValue};

pub(crate) mod bindings {
    #![allow(non_upper_case_globals)]
    #![allow(non_camel_case_types)]
    #![allow(non_snake_case)]
    #![allow(deref_nullptr)]
    #![allow(dead_code)]
    include!(concat!(env!("OUT_DIR"), "/bindings.rs"));
}
