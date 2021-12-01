pub type BoxedError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug)]
pub enum Error {
    SenderError(BoxedError),
    WriteError(WriteError),
    ParseError,
    TimedOut,
    Canceled,
}

impl From<BoxedError> for Error {
    fn from(e: BoxedError) -> Self {
        Self::SenderError(e)
    }
}

impl From<WriteError> for Error {
    fn from(e: WriteError) -> Self {
        Self::WriteError(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::SenderError(err) => write!(f, "{}", err),
            Error::WriteError(err) => write!(f, "{}", err),
            Error::ParseError => write!(f, "failed to parse setting"),
            Error::TimedOut => write!(f, "timed out"),
            Error::Canceled => write!(f, "canceled"),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteError {
    ValueRejected,
    SettingRejected,
    ParseFailed,
    ReadOnly,
    ModifyDisabled,
    ServiceFailed,
    Timeout,
    Unknown,
}

impl From<u8> for WriteError {
    fn from(n: u8) -> Self {
        match n {
            1 => WriteError::ValueRejected,
            2 => WriteError::SettingRejected,
            3 => WriteError::ParseFailed,
            4 => WriteError::ReadOnly,
            5 => WriteError::ModifyDisabled,
            6 => WriteError::ServiceFailed,
            7 => WriteError::Timeout,
            _ => WriteError::Unknown,
        }
    }
}

impl std::fmt::Display for WriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteError::ValueRejected => {
                write!(f, "setting value invalid",)
            }
            WriteError::SettingRejected => {
                write!(f, "setting does not exist")
            }
            WriteError::ParseFailed => {
                write!(f, "could not parse setting value ")
            }
            WriteError::ReadOnly => {
                write!(f, "setting is read only")
            }
            WriteError::ModifyDisabled => {
                write!(f, "setting is not modifiable")
            }
            WriteError::ServiceFailed => write!(f, "system failure during setting"),
            WriteError::Timeout => write!(f, "request wasn't replied in time"),
            WriteError::Unknown => {
                write!(f, "unknown settings write response")
            }
        }
    }
}

impl std::error::Error for WriteError {}
