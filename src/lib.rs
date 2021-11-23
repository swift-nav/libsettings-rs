mod client;
pub mod settings;

pub use client::{Client, Context, Entry, Error, WriteSettingError};
pub use settings::{Setting, SettingKind, SettingValue};
