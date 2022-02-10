use std::{borrow::Cow, fmt, fs, io, path::Path};

use log::warn;
use once_cell::sync::OnceCell;
use serde::{
    de::{self, Unexpected},
    Deserialize, Deserializer,
};

static SETTINGS: OnceCell<Vec<Setting>> = OnceCell::new();

fn bundled_settings() -> Vec<Setting> {
    let settings_yaml = include_str!(concat!(env!("OUT_DIR"), "/settings.yaml"));
    serde_yaml::from_str(settings_yaml).expect("could not parse settings.yaml")
}

pub fn load_from_path(path: impl AsRef<Path>) -> Result<(), LoadFromPathError> {
    let file = fs::File::open(path).map_err(LoadFromPathError::Io)?;
    let settings: Vec<serde_yaml::Mapping> =
        serde_yaml::from_reader(file).map_err(LoadFromPathError::Serde)?;
    let type_key = serde_yaml::Value::String("type".into());
    let settings = settings
        .into_iter()
        .map(|mut map| {
            if map.contains_key(&type_key) {
                serde_yaml::from_value(map.into())
            } else {
                map.insert(type_key.clone(), serde_yaml::Value::String("string".into()));
                let setting: Setting = serde_yaml::from_value(map.into())?;
                warn!(
                    "Missing `type` field for {} -> {}",
                    setting.group, setting.name
                );
                Ok(setting)
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(LoadFromPathError::Serde)?;
    load(settings).map_err(|_| LoadFromPathError::AlreadySet)
}

pub fn load(settings: Vec<Setting>) -> Result<(), Vec<Setting>> {
    SETTINGS.set(settings)
}

#[derive(Debug, Default, Clone, PartialEq, Deserialize)]
pub struct Setting {
    pub name: String,

    pub group: String,

    #[serde(rename = "type")]
    pub kind: SettingKind,

    #[serde(deserialize_with = "deserialize_bool", default)]
    pub expert: bool,

    #[serde(deserialize_with = "deserialize_bool", default)]
    pub readonly: bool,

    #[serde(rename = "Description")]
    pub description: Option<String>,

    #[serde(
        alias = "default value",
        deserialize_with = "deserialize_string",
        default
    )]
    pub default_value: Option<String>,

    #[serde(rename = "Notes")]
    pub notes: Option<String>,

    #[serde(deserialize_with = "deserialize_string", default)]
    pub units: Option<String>,

    #[serde(
        rename = "enumerated possible values",
        deserialize_with = "deserialize_string",
        default
    )]
    pub enumerated_possible_values: Option<String>,

    pub digits: Option<String>,
}

impl Setting {
    pub fn all() -> &'static [Setting] {
        let settings = SETTINGS.get_or_init(bundled_settings);
        settings.as_slice()
    }

    pub fn find(group: impl AsRef<str>, name: impl AsRef<str>) -> Option<&'static Setting> {
        let group = group.as_ref();
        let name = name.as_ref();
        Setting::all()
            .iter()
            .find(|s| s.group == group && s.name == name)
    }

    pub(crate) fn new(group: impl AsRef<str>, name: impl AsRef<str>) -> Cow<'static, Setting> {
        Setting::find(&group, &name).map_or_else(
            || {
                let group = group.as_ref().to_owned();
                let name = name.as_ref().to_owned();
                warn!("No documentation entry setting {} -> {}", group, name);
                Cow::Owned(Setting {
                    group,
                    name,
                    ..Default::default()
                })
            },
            Cow::Borrowed,
        )
    }

    pub(crate) fn with_fmt_type(
        group: impl AsRef<str>,
        name: impl AsRef<str>,
        fmt_type: impl AsRef<str>,
    ) -> Cow<'static, Setting> {
        let mut setting = Setting::new(group, name);
        if setting.kind == SettingKind::Enum {
            let mut parts = fmt_type.as_ref().splitn(2, ':');
            let possible_values = parts.nth(1);
            if let Some(p) = possible_values {
                setting.to_mut().enumerated_possible_values = Some(p.to_owned());
            }
        }
        setting
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize)]
pub enum SettingKind {
    #[serde(rename = "integer", alias = "int")]
    Integer,

    #[serde(rename = "boolean", alias = "bool")]
    Boolean,

    #[serde(rename = "float")]
    Float,

    #[serde(rename = "double", alias = "Double")]
    Double,

    #[serde(rename = "string")]
    String,

    #[serde(rename = "enum")]
    Enum,

    #[serde(rename = "packed bitfield")]
    PackedBitfield,
}

impl Default for SettingKind {
    fn default() -> Self {
        SettingKind::String
    }
}

impl SettingKind {
    pub fn to_str(&self) -> &'static str {
        match self {
            SettingKind::Integer => "integer",
            SettingKind::Boolean => "boolean",
            SettingKind::Float => "float",
            SettingKind::Double => "double",
            SettingKind::String => "string",
            SettingKind::Enum => "enum",
            SettingKind::PackedBitfield => "packed bitfield",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SettingValue {
    Integer(i64),
    Boolean(bool),
    Float(f32),
    String(String),
}

impl SettingValue {
    pub fn parse(v: &str, kind: SettingKind) -> Option<Self> {
        if v.is_empty() {
            return None;
        }
        match kind {
            SettingKind::Integer => v.parse().ok().map(SettingValue::Integer),
            SettingKind::Boolean if v == "True" => Some(SettingValue::Boolean(true)),
            SettingKind::Boolean if v == "False" => Some(SettingValue::Boolean(false)),
            SettingKind::Float | SettingKind::Double => v.parse().ok().map(SettingValue::Float),
            SettingKind::String | SettingKind::Enum | SettingKind::PackedBitfield => {
                Some(SettingValue::String(v.to_owned()))
            }
            _ => None,
        }
    }
}

impl fmt::Display for SettingValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SettingValue::Integer(s) => s.fmt(f),
            // For consistency with the C version of libsettings, use the
            // same boolean literals
            SettingValue::Boolean(true) => write!(f, "True"),
            SettingValue::Boolean(false) => write!(f, "False"),
            SettingValue::Float(s) => s.fmt(f),
            SettingValue::String(s) => s.fmt(f),
        }
    }
}

fn deserialize_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    struct BoolVisitor;

    impl<'de> de::Visitor<'de> for BoolVisitor {
        type Value = bool;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a bool or a string containing a bool")
        }

        fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(v)
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            match v {
                "True" | "true" => Ok(true),
                "False" | "false" => Ok(false),
                other => Err(de::Error::invalid_value(
                    Unexpected::Str(other),
                    &"True or False",
                )),
            }
        }
    }

    deserializer.deserialize_any(BoolVisitor)
}

fn deserialize_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringVisitor;

    impl<'de> de::Visitor<'de> for StringVisitor {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("an optional string")
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            match v {
                "N/A" | "" => Ok(None),
                _ => Ok(Some(v.to_owned())),
            }
        }

        fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(v.to_string()))
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(v.to_string()))
        }

        fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(v.to_string()))
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }
    }

    deserializer.deserialize_any(StringVisitor)
}

#[derive(Debug)]
pub enum LoadFromPathError {
    AlreadySet,
    Io(io::Error),
    Serde(serde_yaml::Error),
}

impl fmt::Display for LoadFromPathError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LoadFromPathError::AlreadySet => write!(f, "settings have already been loaded"),
            LoadFromPathError::Io(e) => write!(f, "{}", e),
            LoadFromPathError::Serde(e) => write!(f, "{}", e),
        }
    }
}

impl std::error::Error for LoadFromPathError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_setting() {
        assert_eq!(
            Setting::find("solution", "soln_freq"),
            Some(&Setting {
                name: "soln_freq".into(),
                group: "solution".into(),
                kind: SettingKind::Integer,
                readonly: false,
                expert: false,
                units: Some("Hz".into()),
                default_value: Some("10".into()),
                description: Some("The frequency at which a position solution is computed.".into()),
                notes: None,
                enumerated_possible_values: None,
                digits: None,
            })
        );

        assert_eq!(Setting::find("solution", "froo_froo"), None);
    }

    #[test]
    fn test_na_is_none() {
        let setting = Setting::find("tcp_server0", "enabled_sbp_messages").unwrap();
        assert_eq!(setting.units, None);
    }

    #[test]
    fn test_bool_display() {
        assert_eq!(SettingValue::Boolean(true).to_string(), "True");
        assert_eq!(SettingValue::Boolean(false).to_string(), "False");
    }
}
