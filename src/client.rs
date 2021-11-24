use std::{
    borrow::Cow,
    convert::TryFrom,
    io,
    sync::{
        atomic::{AtomicU16, AtomicUsize, Ordering},
        Arc,
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use crossbeam_channel::{Receiver, Sender};
use crossbeam_utils::thread;
use log::trace;
use parking_lot::Mutex;
use sbp::{
    link::{Link, LinkSource},
    messages::settings::{
        MsgSettingsReadByIndexDone, MsgSettingsReadByIndexReq, MsgSettingsReadByIndexResp,
        MsgSettingsReadReq, MsgSettingsReadResp, MsgSettingsWrite, MsgSettingsWriteResp,
    },
    sbp_string::Multipart,
    Sbp, SbpIterExt, SbpString,
};

use crate::{Setting, SettingValue};

const SENDER_ID: u16 = 0x42;
const NUM_WORKERS: usize = 10;

pub struct Client<'a> {
    link: Link<'a, ()>,
    sender: MsgSender,
    handle: Option<JoinHandle<()>>,
}

impl<'a> Client<'a> {
    pub fn new<R, W>(reader: R, mut writer: W) -> Client<'static>
    where
        R: io::Read + Send + 'static,
        W: io::Write + Send + 'static,
    {
        let source = LinkSource::new();
        let mut client = Client::<'static>::with_link(source.link(), move |msg| {
            sbp::to_writer(&mut writer, &msg).map_err(Into::into)
        });
        client.handle = Some(std::thread::spawn(move || {
            let messages = sbp::iter_messages(reader).log_errors(log::Level::Warn);
            for msg in messages {
                source.send(msg);
            }
        }));
        client
    }

    pub fn with_link<F>(link: Link<'a, ()>, sender: F) -> Client<'a>
    where
        F: FnMut(Sbp) -> Result<(), BoxedError> + Send + 'static,
    {
        Self {
            link,
            sender: MsgSender(Arc::new(Mutex::new(Box::new(sender)))),
            handle: None,
        }
    }

    pub fn write_setting(
        &mut self,
        group: impl Into<String>,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<Entry, Error> {
        let (ctx, _ctx_handle) = Context::new();
        self.write_setting_ctx(group, name, value, ctx)
    }

    pub fn write_setting_ctx(
        &mut self,
        group: impl Into<String>,
        name: impl Into<String>,
        value: impl Into<String>,
        ctx: Context,
    ) -> Result<Entry, Error> {
        self.write_setting_inner(group.into(), name.into(), value.into(), ctx)
    }

    pub fn read_setting(
        &mut self,
        group: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<Option<Entry>, Error> {
        let (ctx, _ctx_handle) = Context::new();
        self.read_setting_ctx(group, name, ctx)
    }

    pub fn read_setting_ctx(
        &mut self,
        group: impl Into<String>,
        name: impl Into<String>,
        ctx: Context,
    ) -> Result<Option<Entry>, Error> {
        self.read_setting_inner(group.into(), name.into(), ctx)
    }

    pub fn read_all(&mut self) -> (Vec<Entry>, Vec<Error>) {
        let (ctx, _ctx_handle) = Context::new();
        self.read_all_ctx(ctx)
    }

    pub fn read_all_ctx(&mut self, ctx: Context) -> (Vec<Entry>, Vec<Error>) {
        self.read_all_inner(ctx)
    }

    fn read_all_inner(&mut self, ctx: Context) -> (Vec<Entry>, Vec<Error>) {
        let (done_tx, done_rx) = crossbeam_channel::bounded(NUM_WORKERS);
        let done_key = self.link.register(move |_: MsgSettingsReadByIndexDone| {
            for _ in 0..NUM_WORKERS {
                let _ = done_tx.try_send(());
            }
        });
        let (settings, errors) = (Mutex::new(Vec::new()), Mutex::new(Vec::new()));
        let idx = AtomicU16::new(0);
        thread::scope(|scope| {
            for _ in 0..NUM_WORKERS {
                let this = &self;
                let idx = &idx;
                let settings = &settings;
                let errors = &errors;
                let done_rx = &done_rx;
                let ctx = ctx.clone();
                scope.spawn(move |_| loop {
                    let idx = idx.fetch_add(1, Ordering::SeqCst);
                    match this.read_by_index(idx, done_rx, &ctx) {
                        Ok(Some(setting)) => {
                            settings.lock().push((idx, setting));
                        }
                        Ok(None) => break,
                        Err(err) => {
                            let exit = matches!(err, Error::TimedOut | Error::Canceled);
                            errors.lock().push((idx, err));
                            if exit {
                                break;
                            }
                        }
                    }
                });
            }
        })
        .expect("read_all worker thread panicked");
        self.link.unregister(done_key);
        settings.lock().sort_by_key(|(idx, _)| *idx);
        errors.lock().sort_by_key(|(idx, _)| *idx);
        (
            settings.into_inner().into_iter().map(|e| e.1).collect(),
            errors.into_inner().into_iter().map(|e| e.1).collect(),
        )
    }

    fn read_by_index(
        &self,
        index: u16,
        done_rx: &Receiver<()>,
        ctx: &Context,
    ) -> Result<Option<Entry>, Error> {
        trace!("read_by_idx: {}", index);
        let (tx, rx) = crossbeam_channel::bounded(1);
        let key = self.link.register(move |msg: MsgSettingsReadByIndexResp| {
            if index == msg.index {
                let _ = tx.try_send(Entry::try_from(msg));
            }
        });
        self.sender.send(MsgSettingsReadByIndexReq {
            sender_id: Some(SENDER_ID),
            index,
        })?;
        let res = crossbeam_channel::select! {
            recv(rx) -> msg => msg.expect("read_by_index channel disconnected").map(Some),
            recv(done_rx) -> _ => Ok(None),
            recv(ctx.timeout_rx) -> _ => Err(Error::TimedOut),
            recv(ctx.cancel_rx) -> _ => Err(Error::Canceled),
        };
        self.link.unregister(key);
        res
    }

    fn read_setting_inner(
        &mut self,
        group: String,
        name: String,
        ctx: Context,
    ) -> Result<Option<Entry>, Error> {
        trace!("read_setting: {} {}", group, name);
        let req = MsgSettingsReadReq {
            sender_id: Some(SENDER_ID),
            setting: format!("{}\0{}\0", group, name).into(),
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let key = self.link.register(move |msg: MsgSettingsReadResp| {
            if request_matches(&group, &name, &msg.setting) {
                let _ = tx.try_send(Entry::try_from(msg).map(|e| {
                    if e.value.is_some() {
                        Some(e)
                    } else {
                        None
                    }
                }));
            }
        });
        self.sender.send(req)?;
        let res = crossbeam_channel::select! {
            recv(rx) -> msg => msg.expect("read_setting_inner channel disconnected"),
            recv(ctx.timeout_rx) -> _ => Err(Error::TimedOut),
            recv(ctx.cancel_rx) -> _ => Err(Error::Canceled),
        };
        self.link.unregister(key);
        res
    }

    fn write_setting_inner(
        &mut self,
        group: String,
        name: String,
        value: String,
        ctx: Context,
    ) -> Result<Entry, Error> {
        trace!("write_setting: {} {} {}", group, name, value);
        let req = MsgSettingsWrite {
            sender_id: Some(SENDER_ID),
            setting: format!("{}\0{}\0{}\0", group, name, value).into(),
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let key = self.link.register(move |msg: MsgSettingsWriteResp| {
            if request_matches(&group, &name, &msg.setting) {
                let _ = tx.try_send(Entry::try_from(msg));
            }
        });
        self.sender.send(req)?;
        let res = crossbeam_channel::select! {
            recv(rx) -> msg => msg.expect("write_setting_inner channel disconnected"),
            recv(ctx.timeout_rx) -> _ => Err(Error::TimedOut),
            recv(ctx.cancel_rx) -> _ => Err(Error::Canceled),
        };
        self.link.unregister(key);
        res
    }
}

fn request_matches(group: &str, name: &str, setting: &SbpString<Vec<u8>, Multipart>) -> bool {
    let fields = split_multipart(setting);
    matches!(fields.as_slice(), [g, n, ..] if g == group && n == name)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub setting: Cow<'static, Setting>,
    pub value: Option<SettingValue>,
}

impl TryFrom<MsgSettingsWriteResp> for Entry {
    type Error = Error;

    fn try_from(msg: MsgSettingsWriteResp) -> Result<Self, Self::Error> {
        if msg.status != 0 {
            return Err(Error::WriteError(msg.status.into()));
        }
        let fields = split_multipart(&msg.setting);
        if let [group, name, value] = fields.as_slice() {
            let setting = Setting::new(group, name);
            let value = SettingValue::parse(value, setting.kind);
            Ok(Entry { setting, value })
        } else {
            Err(Error::ParseError)
        }
    }
}

impl TryFrom<MsgSettingsReadResp> for Entry {
    type Error = Error;

    fn try_from(msg: MsgSettingsReadResp) -> Result<Self, Self::Error> {
        let fields = split_multipart(&msg.setting);
        match fields.as_slice() {
            [group, name] => {
                let setting = Setting::new(&group, &name);
                Ok(Entry {
                    setting,
                    value: None,
                })
            }
            [group, name, value] | [group, name, value, ..] => {
                let setting = Setting::new(&group, &name);
                let value = SettingValue::parse(value, setting.kind);
                Ok(Entry { setting, value })
            }
            _ => Err(Error::ParseError),
        }
    }
}

impl TryFrom<MsgSettingsReadByIndexResp> for Entry {
    type Error = Error;

    fn try_from(msg: MsgSettingsReadByIndexResp) -> Result<Self, Self::Error> {
        let fields = split_multipart(&msg.setting);
        match fields.as_slice() {
            [group, name, value, fmt_type] => {
                let setting = if fmt_type.is_empty() {
                    Setting::new(group, name)
                } else {
                    Setting::with_fmt_type(group, name, fmt_type)
                };
                let value = if !value.is_empty() {
                    SettingValue::parse(value, setting.kind)
                } else {
                    None
                };
                Ok(Entry { setting, value })
            }
            _ => Err(Error::ParseError),
        }
    }
}

fn split_multipart(s: &SbpString<Vec<u8>, Multipart>) -> Vec<Cow<'_, str>> {
    let mut parts: Vec<_> = s
        .as_bytes()
        .split(|b| *b == 0)
        .map(String::from_utf8_lossy)
        .collect();
    parts.pop();
    parts
}

pub struct Context {
    cancel_rx: Receiver<()>,
    timeout_rx: Receiver<Instant>,
    timeout_at: Instant,
    shared: Arc<CtxShared>,
}

impl Context {
    pub fn new() -> (Context, CtxHandle) {
        let (cancel_tx, cancel_rx) = crossbeam_channel::unbounded();
        let timeout_rx = crossbeam_channel::never();
        let timeout_at = Instant::now() + Duration::from_secs(60 * 60 * 24);
        let shared = Arc::new(CtxShared::new(cancel_tx));
        (
            Context {
                cancel_rx,
                timeout_rx,
                timeout_at,
                shared: Arc::clone(&shared),
            },
            CtxHandle { shared },
        )
    }

    pub fn with_timeout(timeout: Duration) -> (Context, CtxHandle) {
        let (mut ctx, h) = Self::new();
        ctx.set_timeout(timeout);
        (ctx, h)
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout_at = Instant::now() + timeout;
        self.timeout_rx = crossbeam_channel::at(self.timeout_at);
    }

    pub fn cancel(&self) {
        self.shared.cancel();
    }
}

impl Clone for Context {
    fn clone(&self) -> Self {
        self.shared.num_chans.fetch_add(1, Ordering::SeqCst);
        Self {
            cancel_rx: self.cancel_rx.clone(),
            timeout_rx: crossbeam_channel::at(self.timeout_at),
            timeout_at: self.timeout_at,
            shared: Arc::clone(&self.shared),
        }
    }
}

pub struct CtxHandle {
    shared: Arc<CtxShared>,
}

impl CtxHandle {
    pub fn cancel(&self) {
        self.shared.cancel();
    }
}

struct CtxShared {
    cancel_tx: Sender<()>,
    num_chans: AtomicUsize,
}

impl CtxShared {
    fn new(cancel_tx: Sender<()>) -> Self {
        Self {
            cancel_tx,
            num_chans: AtomicUsize::new(1),
        }
    }

    fn cancel(&self) {
        for _ in 0..self.num_chans.load(Ordering::SeqCst) {
            let _ = self.cancel_tx.try_send(());
        }
    }
}

type BoxedError = Box<dyn std::error::Error + Send + Sync>;

type SenderFunc = Box<dyn FnMut(Sbp) -> Result<(), BoxedError> + Send>;

struct MsgSender(Arc<Mutex<SenderFunc>>);

impl MsgSender {
    const RETRIES: usize = 5;
    const TIMEOUT: Duration = Duration::from_millis(100);

    fn send(&self, msg: impl Into<Sbp>) -> Result<(), BoxedError> {
        self.send_inner(msg.into(), 0)
    }

    fn send_inner(&self, msg: Sbp, tries: usize) -> Result<(), BoxedError> {
        let res = (self.0.lock())(msg.clone());
        if res.is_err() && tries < Self::RETRIES {
            std::thread::sleep(Self::TIMEOUT);
            self.send_inner(msg, tries + 1)
        } else {
            res
        }
    }
}

#[derive(Debug)]
pub enum Error {
    SenderError(BoxedError),
    WriteError(WriteSettingError),
    ParseError,
    TimedOut,
    Canceled,
}

impl From<BoxedError> for Error {
    fn from(v: BoxedError) -> Self {
        Self::SenderError(v)
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
pub enum WriteSettingError {
    ValueRejected,
    SettingRejected,
    ParseFailed,
    ReadOnly,
    ModifyDisabled,
    ServiceFailed,
    Timeout,
    Unknown,
}

impl From<u8> for WriteSettingError {
    fn from(n: u8) -> Self {
        match n {
            1 => WriteSettingError::ValueRejected,
            2 => WriteSettingError::SettingRejected,
            3 => WriteSettingError::ParseFailed,
            4 => WriteSettingError::ReadOnly,
            5 => WriteSettingError::ModifyDisabled,
            6 => WriteSettingError::ServiceFailed,
            7 => WriteSettingError::Timeout,
            _ => WriteSettingError::Unknown,
        }
    }
}

impl std::fmt::Display for WriteSettingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteSettingError::ValueRejected => {
                write!(f, "setting value invalid",)
            }
            WriteSettingError::SettingRejected => {
                write!(f, "setting does not exist")
            }
            WriteSettingError::ParseFailed => {
                write!(f, "could not parse setting value ")
            }
            WriteSettingError::ReadOnly => {
                write!(f, "setting is read only")
            }
            WriteSettingError::ModifyDisabled => {
                write!(f, "setting is not modifiable")
            }
            WriteSettingError::ServiceFailed => write!(f, "system failure during setting"),
            WriteSettingError::Timeout => write!(f, "request wasn't replied in time"),
            WriteSettingError::Unknown => {
                write!(f, "unknown settings write response")
            }
        }
    }
}

impl std::error::Error for WriteSettingError {}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::time::Instant;

    use super::*;

    use crossbeam_utils::thread::scope;
    use sbp::messages::settings::{MsgSettingsReadReq, MsgSettingsReadResp};
    use sbp::{SbpMessage, SbpString};

    #[test]
    fn test_should_retry() {
        let (group, name) = ("sbp", "obs_msg_max_size");
        let mut mock = Mock::with_errors(5);
        mock.req_reply(
            MsgSettingsReadReq {
                sender_id: Some(SENDER_ID),
                setting: format!("{}\0{}\0", group, name).into(),
            },
            MsgSettingsReadResp {
                sender_id: Some(0x42),
                setting: format!("{}\0{}\010\0", group, name).into(),
            },
        );
        let (reader, writer) = mock.into_io();
        let mut client = Client::new(reader, writer);
        let response = client.read_setting(group, name).unwrap().unwrap();
        assert!(matches!(response.value, Some(SettingValue::Integer(10))));
    }

    #[test]
    fn read_setting_timeout() {
        let (group, name) = ("sbp", "obs_msg_max_size");
        let mock = Mock::new();
        let (reader, writer) = mock.into_io();
        let mut client = Client::new(reader, writer);
        let (ctx, _ctx_handle) = Context::with_timeout(Duration::from_millis(100));
        let now = Instant::now();
        let mut response = Ok(None);
        scope(|scope| {
            scope.spawn(|_| {
                response = client.read_setting_ctx(group, name, ctx);
            });
        })
        .unwrap();
        assert!(now.elapsed().as_millis() >= 100);
        assert!(matches!(response, Err(Error::TimedOut)));
    }

    #[test]
    fn read_setting_cancel() {
        let (group, name) = ("sbp", "obs_msg_max_size");
        let mock = Mock::new();
        let (reader, writer) = mock.into_io();
        let mut client = Client::new(reader, writer);
        let (ctx, ctx_handle) = Context::new();
        let now = Instant::now();
        let mut response = Ok(None);
        scope(|scope| {
            scope.spawn(|_| {
                response = client.read_setting_ctx(group, name, ctx);
            });
            std::thread::sleep(Duration::from_millis(100));
            ctx_handle.cancel();
        })
        .unwrap();
        assert!(now.elapsed().as_millis() >= 100);
        assert!(matches!(response, Err(Error::Canceled)));
    }

    #[test]
    fn mock_read_setting_int() {
        let (group, name) = ("sbp", "obs_msg_max_size");
        let mut mock = Mock::new();
        mock.req_reply(
            MsgSettingsReadReq {
                sender_id: Some(SENDER_ID),
                setting: format!("{}\0{}\0", group, name).into(),
            },
            MsgSettingsReadResp {
                sender_id: Some(0x42),
                setting: format!("{}\0{}\010\0", group, name).into(),
            },
        );
        let (reader, writer) = mock.into_io();
        let mut client = Client::new(reader, writer);
        let response = client.read_setting(group, name).unwrap().unwrap();
        assert!(matches!(response.value, Some(SettingValue::Integer(10))));
    }

    #[test]
    fn mock_read_setting_bool() {
        let (group, name) = ("surveyed_position", "broadcast");
        let mut mock = Mock::new();
        mock.req_reply(
            MsgSettingsReadReq {
                sender_id: Some(SENDER_ID),
                setting: format!("{}\0{}\0", group, name).into(),
            },
            MsgSettingsReadResp {
                sender_id: Some(0x42),
                setting: SbpString::from(format!("{}\0{}\0True\0", group, name)),
            },
        );
        let (reader, writer) = mock.into_io();
        let mut client = Client::new(reader, writer);
        let response = client.read_setting(group, name).unwrap().unwrap();
        assert!(matches!(response.value, Some(SettingValue::Boolean(true))));
    }

    #[test]
    fn mock_read_setting_double() {
        let (group, name) = ("surveyed_position", "surveyed_lat");
        let mut mock = Mock::new();
        mock.req_reply(
            MsgSettingsReadReq {
                sender_id: Some(SENDER_ID),
                setting: SbpString::from(format!("{}\0{}\0", group, name)),
            },
            MsgSettingsReadResp {
                sender_id: Some(SENDER_ID),
                setting: SbpString::from(format!("{}\0{}\00.1\0", group, name)),
            },
        );
        let (reader, writer) = mock.into_io();
        let mut client = Client::new(reader, writer);
        let response = client.read_setting(group, name).unwrap().unwrap();
        assert_eq!(response.value, Some(SettingValue::Float(0.1)));
    }

    #[test]
    fn mock_read_setting_string() {
        let (group, name) = ("rtcm_out", "ant_descriptor");
        let mut mock = Mock::new();
        mock.req_reply(
            MsgSettingsReadReq {
                sender_id: Some(0x42),
                setting: SbpString::from(format!("{}\0{}\0", group, name)),
            },
            MsgSettingsReadResp {
                sender_id: Some(SENDER_ID),
                setting: SbpString::from(format!("{}\0{}\0foo\0", group, name)),
            },
        );
        let (reader, writer) = mock.into_io();
        let mut client = Client::new(reader, writer);
        let response = client.read_setting(group, name).unwrap().unwrap();
        assert_eq!(response.value, Some(SettingValue::String("foo".into())));
    }

    #[test]
    fn mock_read_setting_enum() {
        let (group, name) = ("frontend", "antenna_selection");
        let mut mock = Mock::new();
        mock.req_reply(
            MsgSettingsReadReq {
                sender_id: Some(SENDER_ID),
                setting: SbpString::from(format!("{}\0{}\0", group, name)),
            },
            MsgSettingsReadResp {
                sender_id: Some(0x42),
                setting: SbpString::from(format!("{}\0{}\0Secondary\0", group, name)),
            },
        );
        let (reader, writer) = mock.into_io();
        let mut client = Client::new(reader, writer);
        let response = client.read_setting(group, name).unwrap().unwrap();
        assert_eq!(
            response.value,
            Some(SettingValue::String("Secondary".into()))
        );
    }

    #[derive(Clone)]
    struct Mock {
        stream: mockstream::SyncMockStream,
        write_errors: u16,
    }

    impl Mock {
        fn new() -> Self {
            Self {
                stream: mockstream::SyncMockStream::new(),
                write_errors: 0,
            }
        }

        fn with_errors(write_errors: u16) -> Self {
            Self {
                stream: mockstream::SyncMockStream::new(),
                write_errors,
            }
        }

        fn req_reply(&mut self, req: impl SbpMessage, res: impl SbpMessage) {
            self.reqs_reply(&[req], res)
        }

        fn reqs_reply(&mut self, reqs: &[impl SbpMessage], res: impl SbpMessage) {
            let bytes: Vec<_> = reqs
                .iter()
                .flat_map(|req| sbp::to_vec(req).unwrap())
                .collect();
            self.stream.wait_for(bytes.as_ref());
            let bytes = sbp::to_vec(&res).unwrap();
            self.stream.push_bytes_to_read(bytes.as_ref());
        }

        fn into_io(self) -> (impl io::Read, impl io::Write) {
            (self.clone(), self)
        }
    }

    impl Read for Mock {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.stream.read(buf)
        }
    }

    impl Write for Mock {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.write_errors > 0 {
                self.write_errors -= 1;
                Err(io::Error::new(io::ErrorKind::Other, "error"))
            } else {
                self.stream.write(buf)
            }
        }

        fn flush(&mut self) -> io::Result<()> {
            self.stream.flush()
        }
    }
}
