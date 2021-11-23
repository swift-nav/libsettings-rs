use std::{
    borrow::Cow,
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
use log::warn;
use parking_lot::Mutex;
use sbp::{
    link::{Key, Link, LinkSource},
    messages::settings::{
        MsgSettingsReadByIndexDone, MsgSettingsReadByIndexReq, MsgSettingsReadByIndexResp,
        MsgSettingsReadReq, MsgSettingsReadResp, MsgSettingsWrite, MsgSettingsWriteResp,
    },
    Sbp, SbpIterExt,
};

use crate::{Setting, SettingKind, SettingValue};

const SENDER_ID: u16 = 0x42;
const NUM_WORKERS: usize = 10;

pub struct Client<'a> {
    link: Link<'a, ()>,
    sender: MsgSender,
    handle: Option<JoinHandle<()>>,
    done_rx: Receiver<()>,
    key: Key,
}

impl Drop for Client<'_> {
    fn drop(&mut self) {
        self.link.unregister(self.key);
    }
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
        let (done_tx, done_rx) = crossbeam_channel::bounded(NUM_WORKERS);
        let key = link.register(on_read_by_index_done(done_tx));
        Self {
            link,
            sender: MsgSender(Arc::new(Mutex::new(Box::new(sender)))),
            handle: None,
            done_rx,
            key,
        }
    }

    pub fn write_setting(
        &self,
        group: impl Into<String>,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), Error<WriteSettingError>> {
        let (ctx, _ctx_handle) = Context::new();
        self.write_setting_ctx(group, name, value, ctx)
    }

    pub fn write_setting_ctx(
        &self,
        group: impl Into<String>,
        name: impl Into<String>,
        value: impl Into<String>,
        ctx: Context,
    ) -> Result<(), Error<WriteSettingError>> {
        self.write_setting_inner(group.into(), name.into(), value.into(), ctx)
    }

    pub fn read_setting(
        &self,
        group: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<Option<ReadResp>, Error<ReadSettingError>> {
        let (ctx, _ctx_handle) = Context::new();
        self.read_setting_ctx(group, name, ctx)
    }

    pub fn read_setting_ctx(
        &self,
        group: impl Into<String>,
        name: impl Into<String>,
        ctx: Context,
    ) -> Result<Option<ReadResp>, Error<ReadSettingError>> {
        self.read_setting_inner(group.into(), name.into(), ctx)
    }

    pub fn read_all(&self) -> (Vec<ReadResp>, Vec<Error<ReadSettingError>>) {
        let (ctx, _ctx_handle) = Context::new();
        self.read_all_ctx(ctx)
    }

    pub fn read_all_ctx(&self, ctx: Context) -> (Vec<ReadResp>, Vec<Error<ReadSettingError>>) {
        self.read_all_inner(ctx)
    }

    fn read_all_inner(&self, ctx: Context) -> (Vec<ReadResp>, Vec<Error<ReadSettingError>>) {
        let (settings, errors) = (Mutex::new(Vec::new()), Mutex::new(Vec::new()));
        let idx = AtomicU16::new(0);
        thread::scope(|scope| {
            for _ in 0..NUM_WORKERS {
                let idx = &idx;
                let settings = &settings;
                let errors = &errors;
                let ctx = ctx.clone();
                scope.spawn(move |_| loop {
                    let idx = idx.fetch_add(1, Ordering::SeqCst);
                    match self.read_by_index(idx, &ctx) {
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
        settings.lock().sort_by_key(|(idx, _)| *idx);
        errors.lock().sort_by_key(|(idx, _)| *idx);
        (
            settings.into_inner().into_iter().map(|e| e.1).collect(),
            errors.into_inner().into_iter().map(|e| e.1).collect(),
        )
    }

    fn read_by_index(
        &self,
        idx: u16,
        ctx: &Context,
    ) -> Result<Option<ReadResp>, Error<ReadSettingError>> {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let key = self.link.register(on_read_by_index_resp(idx, tx));
        self.sender.send(MsgSettingsReadByIndexReq {
            sender_id: Some(SENDER_ID),
            index: idx,
        })?;
        let res = crossbeam_channel::select! {
            recv(rx) -> msg => Ok(Some(msg.unwrap())),
            recv(self.done_rx) -> _ => Ok(None),
            recv(ctx.timeout_rx) -> _ => Err(Error::TimedOut),
            recv(ctx.cancel_rx) -> _ => Err(Error::Canceled),
        };
        self.link.unregister(key);
        res
    }

    fn read_setting_inner(
        &self,
        group: String,
        name: String,
        ctx: Context,
    ) -> Result<Option<ReadResp>, Error<ReadSettingError>> {
        let entry = match Setting::find(&group, &name) {
            Some(setting) => setting,
            None => return Ok(None),
        };
        let read_req = MsgSettingsReadReq {
            sender_id: Some(SENDER_ID),
            setting: format!("{}\0{}\0", group, name).into(),
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let key = self.link.register(on_read_resp(group, name, tx, entry));
        self.sender.send(read_req)?;
        let res = crossbeam_channel::select! {
            recv(rx) -> msg => Ok(msg.unwrap()),
            recv(ctx.timeout_rx) -> _ => Err(Error::TimedOut),
            recv(ctx.cancel_rx) -> _ => Err(Error::Canceled),
        };
        self.link.unregister(key);
        res
    }

    fn write_setting_inner(
        &self,
        group: String,
        name: String,
        value: String,
        ctx: Context,
    ) -> Result<(), Error<WriteSettingError>> {
        let write_req = MsgSettingsWrite {
            sender_id: Some(SENDER_ID),
            setting: format!("{}\0{}\0{}\0", group, name, value).into(),
        };
        let (tx, rx) = crossbeam_channel::bounded(1);
        let key = self.link.register(on_write_resp(group, name, tx));
        self.sender.send(write_req)?;
        let res = crossbeam_channel::select! {
            recv(rx) -> msg => msg.unwrap(),
            recv(ctx.timeout_rx) -> _ => Err(Error::TimedOut),
            recv(ctx.cancel_rx) -> _ => Err(Error::Canceled),
        };
        self.link.unregister(key);
        res
    }
}

fn on_read_by_index_done(done_tx: Sender<()>) -> impl FnMut(MsgSettingsReadByIndexDone) {
    move |_: MsgSettingsReadByIndexDone| {
        for _ in 0..NUM_WORKERS {
            let _ = done_tx.try_send(());
        }
    }
}

fn on_write_resp(
    group: String,
    name: String,
    tx: Sender<Result<(), Error<WriteSettingError>>>,
) -> impl FnMut(MsgSettingsWriteResp) {
    move |msg: MsgSettingsWriteResp| {
        let setting = msg.setting.to_string();
        let mut fields = setting.split('\0');
        if fields.next() == Some(&group) && fields.next() == Some(&name) {
            if msg.status == 0 {
                let _ = tx.send(Ok(()));
            } else {
                let _ = tx.send(Err(Error::Err(WriteSettingError::from(msg.status))));
            }
        }
    }
}

fn on_read_resp(
    group: String,
    name: String,
    tx: Sender<Option<ReadResp>>,
    entry: &'static Setting,
) -> impl FnMut(MsgSettingsReadResp) {
    move |msg: MsgSettingsReadResp| {
        let setting = msg.setting.to_string();
        let mut fields = setting.split('\0');
        if fields.next() == Some(&group) && fields.next() == Some(&name) {
            match fields.next() {
                Some(value) => {
                    let _ = tx.try_send(Some(ReadResp {
                        entry: Cow::Borrowed(entry),
                        value: SettingValue::parse(value, entry.kind),
                    }));
                }
                None => {
                    let _ = tx.try_send(None);
                }
            }
        }
    }
}

fn on_read_by_index_resp(idx: u16, tx: Sender<ReadResp>) -> impl FnMut(MsgSettingsReadByIndexResp) {
    move |msg: MsgSettingsReadByIndexResp| {
        if idx != msg.index {
            return;
        }
        let setting = msg.setting.to_string();
        let mut fields = setting.split('\0');
        let group = fields.next().unwrap_or_default();
        let name = fields.next().unwrap_or_default();
        let entry = Setting::find(group, name);
        if let Some(entry) = entry {
            let value = fields
                .next()
                .and_then(|s| SettingValue::parse(s, entry.kind));
            let fmt_type = fields.next();
            if entry.kind == SettingKind::Enum {
                if let Some(fmt_type) = fmt_type {
                    let mut parts = fmt_type.splitn(2, ':');
                    let possible_values = parts.nth(1);
                    if let Some(p) = possible_values {
                        let mut entry = entry.clone();
                        entry.enumerated_possible_values = Some(p.to_owned());
                        let _ = tx.send(ReadResp {
                            entry: Cow::Owned(entry),
                            value,
                        });
                        return;
                    }
                }
            }
            let _ = tx.send(ReadResp {
                entry: Cow::Borrowed(entry),
                value,
            });
        } else {
            warn!(
                "No settings documentation entry or name: {} in group: {}",
                name, group
            );
            let entry: Cow<Setting> = Cow::Owned(Setting {
                group: group.to_owned(),
                name: name.to_owned(),
                ..Default::default()
            });
            let value = fields
                .next()
                .and_then(|s| SettingValue::parse(s, entry.kind));
            let _ = tx.send(ReadResp { entry, value });
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadResp {
    pub entry: Cow<'static, Setting>,
    pub value: Option<SettingValue>,
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
pub enum Error<E> {
    Err(E),
    SenderError(BoxedError),
    TimedOut,
    Canceled,
}

impl<E> From<BoxedError> for Error<E> {
    fn from(v: BoxedError) -> Self {
        Self::SenderError(v)
    }
}

impl<E> std::fmt::Display for Error<E>
where
    E: std::error::Error,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Err(err) => write!(f, "{}", err),
            Error::TimedOut => write!(f, "timed out"),
            Error::Canceled => write!(f, "canceled"),
            Error::SenderError(err) => write!(f, "{}", err),
        }
    }
}

impl<E> std::error::Error for Error<E> where E: std::error::Error {}

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

#[derive(Debug)]
pub struct ReadSettingError {}

impl std::fmt::Display for ReadSettingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "settings read failed")
    }
}

impl std::error::Error for ReadSettingError {}

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
        let client = Client::new(reader, writer);
        let response = client.read_setting(group, name).unwrap().unwrap();
        assert!(matches!(response.value, Some(SettingValue::Integer(10))));
    }

    #[test]
    fn read_setting_timeout() {
        let (group, name) = ("sbp", "obs_msg_max_size");
        let mock = Mock::new();
        let (reader, writer) = mock.into_io();
        let client = Client::new(reader, writer);
        let (ctx, _ctx_handle) = Context::with_timeout(Duration::from_millis(100));
        let now = Instant::now();
        let mut response1 = Ok(None);
        let mut response2 = Ok(None);
        scope(|scope| {
            scope.spawn({
                let ctx = ctx.clone();
                |_| {
                    response1 = client.read_setting_ctx(group, name, ctx);
                }
            });
            scope.spawn(|_| {
                response2 = client.read_setting_ctx(group, name, ctx);
            });
        })
        .unwrap();
        assert!(now.elapsed().as_millis() >= 100);
        assert!(matches!(response1, Err(Error::TimedOut)));
        assert!(matches!(response2, Err(Error::TimedOut)));
    }

    #[test]
    fn read_setting_cancel() {
        let (group, name) = ("sbp", "obs_msg_max_size");
        let mock = Mock::new();
        let (reader, writer) = mock.into_io();
        let client = Client::new(reader, writer);
        let (ctx, ctx_handle) = Context::new();
        let now = Instant::now();
        let mut response1 = Ok(None);
        let mut response2 = Ok(None);
        scope(|scope| {
            scope.spawn({
                let ctx = ctx.clone();
                |_| {
                    response1 = client.read_setting_ctx(group, name, ctx);
                }
            });
            scope.spawn(|_| {
                response2 = client.read_setting_ctx(group, name, ctx);
            });
            std::thread::sleep(Duration::from_millis(100));
            ctx_handle.cancel();
        })
        .unwrap();
        assert!(now.elapsed().as_millis() >= 100);
        assert!(matches!(response1, Err(Error::Canceled)));
        assert!(matches!(response2, Err(Error::Canceled)));
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
        let client = Client::new(reader, writer);
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
        let client = Client::new(reader, writer);
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
        let client = Client::new(reader, writer);
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
        let client = Client::new(reader, writer);
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
        let client = Client::new(reader, writer);
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
