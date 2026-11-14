//! Sending and receiving files and images between the computers.
//!
//! Everything goes over the existing encrypted connection in small pieces (32 KiB), with a
//! window of about 256 KiB: the sender waits for the receiver's acknowledgements before it
//! sends more. That keeps memory use flat for huge files and stops a big transfer from
//! delaying the mouse and keyboard events that share the same connection.
//!
//! Received files are never allowed to choose where they are saved: names are cleaned up,
//! existing files are never overwritten, and sizes are limited.

use crate::proto::{Msg, XferInfo, XferKind};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU32, Ordering},
        mpsc::Sender,
        Arc, Condvar, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const CHUNK: usize = 32 * 1024;
/// How far the sender may run ahead of the receiver's acknowledgements.
pub const WINDOW: u64 = 256 * 1024;
/// Largest image (as PNG).
pub const MAX_IMAGE: u64 = 48 << 20;
/// Largest total size and count of files shared by copying them.
pub const MAX_CLIP_FILES_TOTAL: u64 = 256 << 20;
pub const MAX_CLIP_FILES: usize = 50;
/// Largest single file dropped onto the window.
pub const MAX_DROP_FILE: u64 = 16 << 30;
const MAX_ACTIVE_IN: usize = 8;
const ACK_TIMEOUT: Duration = Duration::from_secs(30);

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

pub fn next_id() -> u32 {
    NEXT_ID.fetch_add(1, Ordering::SeqCst)
}

// ------------------------------------------------------------ flow control

#[derive(Clone, Default)]
struct AckState {
    acked: u64,
    cancelled: Option<String>,
}

/// What the other computer has acknowledged so far, per transfer.
#[derive(Default)]
pub struct AckTable {
    state: Mutex<HashMap<u32, AckState>>,
    cv: Condvar,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SendError {
    /// the receiver stopped the transfer (too large, disk error, ...)
    Cancelled(String),
    Timeout,
    Disconnected,
    Io(String),
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::Cancelled(r) => write!(f, "stopped by the other computer: {r}"),
            SendError::Timeout => write!(f, "the other computer stopped answering"),
            SendError::Disconnected => write!(f, "the connection was lost"),
            SendError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl AckTable {
    pub fn ack(&self, id: u32, bytes: u64) {
        let mut g = self.state.lock().unwrap();
        let s = g.entry(id).or_default();
        s.acked = s.acked.max(bytes);
        self.cv.notify_all();
    }

    pub fn cancel(&self, id: u32, reason: String) {
        self.state.lock().unwrap().entry(id).or_default().cancelled = Some(reason);
        self.cv.notify_all();
    }

    fn forget(&self, id: u32) {
        self.state.lock().unwrap().remove(&id);
    }

    /// Waits until at least `need` bytes are acknowledged.
    fn wait(&self, id: u32, need: u64) -> Result<(), SendError> {
        let deadline = Instant::now() + ACK_TIMEOUT;
        let mut g = self.state.lock().unwrap();
        loop {
            let s = g.entry(id).or_default().clone();
            if let Some(r) = s.cancelled {
                return Err(SendError::Cancelled(r));
            }
            if s.acked >= need {
                return Ok(());
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(SendError::Timeout);
            }
            g = self.cv.wait_timeout(g, left).unwrap().0;
        }
    }
}

/// A way to reach one other computer: its outgoing message queue and its acknowledgements.
#[derive(Clone)]
pub struct Link {
    pub tx: Sender<Msg>,
    pub acks: Arc<AckTable>,
}

impl Link {
    pub fn new(tx: Sender<Msg>, acks: Arc<AckTable>) -> Self {
        Link { tx, acks }
    }

    fn send(&self, m: Msg) -> Result<(), SendError> {
        self.tx.send(m).map_err(|_| SendError::Disconnected)
    }
}

// ----------------------------------------------------------------- sending

pub struct Meta<'a> {
    pub kind: XferKind,
    pub name: &'a str,
    pub size: u64,
    pub batch: u32,
    pub index: u16,
    pub count: u16,
}

/// Sends `size` bytes read from `src`. `progress` is called with the number of bytes sent so far.
pub fn send_stream(link: &Link, meta: &Meta, mut src: impl Read, progress: &mut dyn FnMut(u64)) -> Result<(), SendError> {
    let id = next_id();
    let result = (|| {
        link.send(Msg::XferBegin {
            id,
            kind: meta.kind,
            name: meta.name.to_string(),
            size: meta.size,
            batch: meta.batch,
            index: meta.index,
            count: meta.count,
        })?;
        let mut buf = vec![0u8; CHUNK];
        let mut sent = 0u64;
        loop {
            let n = src.read(&mut buf).map_err(|e| SendError::Io(e.to_string()))?;
            if n == 0 {
                break;
            }
            if sent + n as u64 > meta.size {
                return Err(SendError::Io("the file grew while it was being sent".into()));
            }
            link.send(Msg::XferData { id, bytes: buf[..n].to_vec() })?;
            sent += n as u64;
            // "complete" is only reported once the other computer confirmed everything (see below)
            progress(if sent >= meta.size { meta.size.saturating_sub(1) } else { sent });
            // do not run further ahead than the window
            link.acks.wait(id, sent.saturating_sub(WINDOW))?;
        }
        if sent != meta.size {
            return Err(SendError::Io("the file changed while it was being sent".into()));
        }
        link.send(Msg::XferEnd { id })?;
        link.acks.wait(id, meta.size) // the final acknowledgement: everything is stored
    })();
    if let Err(e) = &result {
        let _ = link.tx.send(Msg::XferCancel { id, reason: e.to_string() }); // let the receiver drop a partial file
    }
    link.acks.forget(id);
    result
}

/// Sends an image (already PNG encoded).
pub fn send_image(link: &Link, png: &[u8]) -> Result<(), SendError> {
    let meta = Meta { kind: XferKind::Image, name: "image.png", size: png.len() as u64, batch: next_id(), index: 0, count: 1 };
    send_stream(link, &meta, png, &mut |_| {})
}

/// Sends files one after the other. `progress(name, size, done)` reports each of them.
/// Returns how many files were sent.
pub fn send_paths(link: &Link, paths: &[PathBuf], kind: XferKind, progress: &mut dyn FnMut(&str, u64, u64)) -> Result<usize, SendError> {
    let batch = next_id();
    let count = paths.len().min(u16::MAX as usize) as u16;
    let mut sent = 0;
    for (i, p) in paths.iter().enumerate().take(count as usize) {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "file".into());
        let file = File::open(p).map_err(|e| SendError::Io(format!("{name}: {e}")))?;
        let size = file.metadata().map_err(|e| SendError::Io(format!("{name}: {e}")))?.len();
        let meta = Meta { kind, name: &name, size, batch, index: i as u16, count };
        progress(&name, size, 0);
        send_stream(link, &meta, file, &mut |done| progress(&name, size, done))?;
        progress(&name, size, size);
        sent += 1;
    }
    Ok(sent)
}

/// Splits a selection into the files that can be sent and a note about what was left out.
pub fn usable_files(paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<String>) {
    let mut files = Vec::new();
    let mut notes = Vec::new();
    for p in paths {
        let name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match fs::metadata(p) {
            Ok(m) if m.is_file() => files.push(p.clone()),
            Ok(m) if m.is_dir() => notes.push(format!("{name}: folders cannot be sent yet")),
            _ => notes.push(format!("{name}: cannot be read")),
        }
    }
    (files, notes)
}

// --------------------------------------------------------------- file names

/// A name that is safe to create on this computer: no folders, no tricks, no reserved names.
pub fn safe_name(raw: &str) -> String {
    let last = raw.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = last.chars().filter(|c| !c.is_control() && !matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*')).collect();
    let cleaned = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    let mut name = if cleaned.is_empty() { "file".to_string() } else { cleaned };
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4 && (stem.starts_with("COM") || stem.starts_with("LPT")) && stem.ends_with(|c: char| ('1'..='9').contains(&c)));
    if reserved {
        name.insert(0, '_');
    }
    if name.chars().count() > 150 {
        name = name.chars().take(150).collect();
    }
    name
}

/// `dir/name`, or `dir/name (1).ext`, `(2)`, ... if that already exists. Never overwrites anything.
pub fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() && !PathBuf::from(format!("{}.part", first.display())).exists() {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1.. {
        let p = dir.join(format!("{stem} ({n}){ext}"));
        if !p.exists() && !PathBuf::from(format!("{}.part", p.display())).exists() {
            return p;
        }
    }
    unreachable!()
}

// ---------------------------------------------------------------- folders

/// Where received files go.
#[derive(Clone, Debug)]
pub struct Dirs {
    /// files of a clipboard copy wait here until they are pasted
    pub inbox: PathBuf,
    /// dropped files are saved here
    pub downloads: PathBuf,
}

impl Dirs {
    /// The usual places: `<config>/inbox` and `<Downloads>/MKShare`.
    pub fn standard() -> Dirs {
        let inbox = crate::secret::config_dir().map(|d| d.join("inbox")).unwrap_or_else(|| std::env::temp_dir().join("mkshare-inbox"));
        Dirs { inbox, downloads: download_dir().join("MKShare") }
    }
}

/// The user's Downloads folder.
pub fn download_dir() -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    if cfg!(target_os = "linux") {
        if let Some(h) = &home {
            // ~/.config/user-dirs.dirs: XDG_DOWNLOAD_DIR="$HOME/Downloads"
            if let Ok(s) = fs::read_to_string(h.join(".config/user-dirs.dirs")) {
                for line in s.lines() {
                    if let Some(v) = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=") {
                        let v = v.trim().trim_matches('"').replace("$HOME", &h.to_string_lossy());
                        if !v.is_empty() {
                            return PathBuf::from(v);
                        }
                    }
                }
            }
        }
    }
    home.map(|h| h.join("Downloads")).unwrap_or_else(std::env::temp_dir)
}

/// Removes clipboard-file folders older than `max_age` (they were only needed for pasting).
pub fn clean_inbox(inbox: &Path, max_age: Duration) {
    let Ok(rd) = fs::read_dir(inbox) else { return };
    for e in rd.flatten() {
        let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| SystemTime::now().duration_since(t).ok()).is_some_and(|age| age > max_age);
        if old {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

// --------------------------------------------------------------- receiving

/// Something that happened while receiving.
#[derive(Debug)]
pub enum Event {
    Progress { name: String, size: u64, done: u64, kind: XferKind },
    /// a complete PNG image
    Image(Vec<u8>),
    /// all files of a clipboard copy arrived (they are in the inbox)
    Files(Vec<PathBuf>),
    /// a dropped file was saved here
    Dropped(PathBuf),
    Failed { name: String, reason: String },
}

enum Sink {
    Mem(Vec<u8>),
    File { file: File, part: PathBuf, dest: PathBuf },
}

struct Incoming {
    kind: XferKind,
    name: String,
    size: u64,
    got: u64,
    acked: u64,
    batch: u32,
    sink: Sink,
    last_event: Instant,
}

struct Batch {
    dir: PathBuf,
    count: u16,
    paths: Vec<PathBuf>,
}

/// Receives the transfers of one connection.
pub struct Receiver {
    dirs: Dirs,
    send: Box<dyn Fn(Msg) + Send>,
    active: HashMap<u32, Incoming>,
    batches: HashMap<u32, Batch>,
}

impl Receiver {
    /// `send` delivers acknowledgements and cancellations back to the sender.
    pub fn new(dirs: Dirs, send: impl Fn(Msg) + Send + 'static) -> Self {
        Receiver { dirs, send: Box::new(send), active: HashMap::new(), batches: HashMap::new() }
    }

    fn cancel(&mut self, id: u32, name: &str, reason: &str) -> Vec<Event> {
        (self.send)(Msg::XferCancel { id, reason: reason.to_string() });
        self.drop_partial(id);
        vec![Event::Failed { name: name.to_string(), reason: reason.to_string() }]
    }

    fn drop_partial(&mut self, id: u32) {
        if let Some(inc) = self.active.remove(&id) {
            if let Sink::File { file, part, .. } = inc.sink {
                drop(file);
                let _ = fs::remove_file(part);
            }
        }
    }

    /// Throw away everything unfinished (the connection ended).
    pub fn abort_all(&mut self) {
        let ids: Vec<u32> = self.active.keys().copied().collect();
        for id in ids {
            self.drop_partial(id);
        }
    }

    fn batch_dir(&mut self, batch: u32, count: u16) -> std::io::Result<PathBuf> {
        if let Some(b) = self.batches.get(&batch) {
            return Ok(b.dir.clone());
        }
        let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let dir = self.dirs.inbox.join(format!("{ms}-{batch}"));
        fs::create_dir_all(&dir)?;
        self.batches.insert(batch, Batch { dir: dir.clone(), count, paths: Vec::new() });
        Ok(dir)
    }

    /// Handles one message of a transfer (other messages are ignored).
    pub fn handle(&mut self, msg: Msg) -> Vec<Event> {
        match msg {
            Msg::XferBegin { id, kind, name, size, batch, index: _, count } => self.begin(id, kind, &name, size, batch, count),
            Msg::XferData { id, bytes } => self.data(id, &bytes),
            Msg::XferEnd { id } => self.end(id),
            Msg::XferCancel { id, .. } => {
                self.drop_partial(id);
                vec![]
            }
            _ => vec![],
        }
    }

    fn begin(&mut self, id: u32, kind: XferKind, raw_name: &str, size: u64, batch: u32, count: u16) -> Vec<Event> {
        let name = safe_name(raw_name);
        if self.active.contains_key(&id) {
            return self.cancel(id, &name, "duplicate transfer");
        }
        if self.active.len() >= MAX_ACTIVE_IN {
            return self.cancel(id, &name, "too many transfers at once");
        }
        let limit = match kind {
            XferKind::Image => MAX_IMAGE,
            XferKind::File => MAX_CLIP_FILES_TOTAL,
            XferKind::Drop => MAX_DROP_FILE,
        };
        if size > limit {
            return self.cancel(id, &name, "too large");
        }
        if kind == XferKind::File && count as usize > MAX_CLIP_FILES {
            return self.cancel(id, &name, "too many files");
        }
        let sink = match kind {
            XferKind::Image => Sink::Mem(Vec::with_capacity(size.min(1 << 20) as usize)),
            XferKind::File | XferKind::Drop => {
                let dir = if kind == XferKind::File {
                    self.batch_dir(batch, count)
                } else {
                    fs::create_dir_all(&self.dirs.downloads).map(|_| self.dirs.downloads.clone())
                };
                let dir = match dir {
                    Ok(d) => d,
                    Err(e) => return self.cancel(id, &name, &format!("cannot create the folder: {e}")),
                };
                let dest = unique_path(&dir, &name);
                let part = PathBuf::from(format!("{}.part", dest.display()));
                match File::create(&part) {
                    Ok(file) => Sink::File { file, part, dest },
                    Err(e) => return self.cancel(id, &name, &format!("cannot write the file: {e}")),
                }
            }
        };
        self.active.insert(id, Incoming { kind, name: name.clone(), size, got: 0, acked: 0, batch, sink, last_event: Instant::now() });
        vec![Event::Progress { name, size, done: 0, kind }]
    }

    fn data(&mut self, id: u32, bytes: &[u8]) -> Vec<Event> {
        let Some(inc) = self.active.get_mut(&id) else { return vec![] };
        inc.got += bytes.len() as u64;
        if inc.got > inc.size {
            let name = inc.name.clone();
            return self.cancel(id, &name, "more data than announced");
        }
        let written = match &mut inc.sink {
            Sink::Mem(v) => {
                v.extend_from_slice(bytes);
                Ok(())
            }
            Sink::File { file, .. } => file.write_all(bytes),
        };
        if let Err(e) = written {
            let name = inc.name.clone();
            return self.cancel(id, &name, &format!("cannot write the file: {e}"));
        }
        let mut events = Vec::new();
        if inc.got - inc.acked >= WINDOW / 2 {
            inc.acked = inc.got;
            (self.send)(Msg::XferAck { id, bytes: inc.got });
        }
        if inc.last_event.elapsed() >= Duration::from_millis(250) {
            inc.last_event = Instant::now();
            events.push(Event::Progress { name: inc.name.clone(), size: inc.size, done: inc.got, kind: inc.kind });
        }
        events
    }

    fn end(&mut self, id: u32) -> Vec<Event> {
        let Some(inc) = self.active.remove(&id) else { return vec![] };
        if inc.got != inc.size {
            let _ = (self.send)(Msg::XferCancel { id, reason: "incomplete".into() });
            if let Sink::File { file, part, .. } = inc.sink {
                drop(file);
                let _ = fs::remove_file(part);
            }
            return vec![Event::Failed { name: inc.name, reason: "incomplete".into() }];
        }
        let mut events = vec![Event::Progress { name: inc.name.clone(), size: inc.size, done: inc.size, kind: inc.kind }];
        match inc.sink {
            Sink::Mem(png) => {
                (self.send)(Msg::XferAck { id, bytes: inc.size });
                events.push(Event::Image(png));
            }
            Sink::File { mut file, part, dest } => {
                let finished = file.flush().and_then(|_| {
                    drop(file);
                    fs::rename(&part, &dest)
                });
                if let Err(e) = finished {
                    let _ = fs::remove_file(&part);
                    (self.send)(Msg::XferCancel { id, reason: e.to_string() });
                    events.push(Event::Failed { name: inc.name, reason: format!("cannot save the file: {e}") });
                    return events;
                }
                (self.send)(Msg::XferAck { id, bytes: inc.size });
                if inc.kind == XferKind::Drop {
                    events.push(Event::Dropped(dest));
                } else if let Some(b) = self.batches.get_mut(&inc.batch) {
                    b.paths.push(dest);
                    if b.paths.len() >= b.count as usize {
                        let done = self.batches.remove(&inc.batch).map(|b| b.paths).unwrap_or_default();
                        events.push(Event::Files(done));
                    }
                }
            }
        }
        events
    }
}

// ------------------------------------------------- progress for the app

fn kind_name(kind: XferKind) -> &'static str {
    match kind {
        XferKind::Image => "image",
        XferKind::File => "file",
        XferKind::Drop => "drop",
    }
}

/// One machine-readable line (`Xfer: {...}`) that the app turns into its transfer list.
pub fn print_info(i: &XferInfo) {
    if let Ok(json) = serde_json::to_string(i) {
        println!("Xfer: {json}");
    }
}

#[allow(clippy::too_many_arguments)]
fn info(dir: &str, peer: &str, name: &str, size: u64, done: u64, state: &str, kind: XferKind, path: Option<&Path>, reason: Option<String>) -> XferInfo {
    XferInfo {
        dir: dir.into(),
        peer: peer.into(),
        name: name.into(),
        size,
        done,
        state: state.into(),
        path: path.map(|p| p.display().to_string()),
        reason,
        kind: kind_name(kind).into(),
    }
}

/// Sends files to one computer in the background and reports the progress.
pub fn spawn_send_files(link: Link, peer: String, paths: Vec<PathBuf>, kind: XferKind) {
    std::thread::spawn(move || {
        let mut last = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now);
        let mut current = (String::new(), 0u64);
        let result = send_paths(&link, &paths, kind, &mut |name, size, done| {
            current = (name.to_string(), size);
            if done == 0 || done >= size || last.elapsed() >= Duration::from_millis(250) {
                last = Instant::now();
                let state = if done >= size { "done" } else { "progress" };
                print_info(&info("out", &peer, name, size, done, state, kind, None, None));
            }
        });
        if let Err(e) = result {
            print_info(&info("out", &peer, &current.0, current.1, 0, "failed", kind, None, Some(e.to_string())));
        }
    });
}

/// Sends an image (PNG) to one computer in the background.
pub fn spawn_send_image(link: Link, peer: String, png: Arc<Vec<u8>>) {
    std::thread::spawn(move || {
        let size = png.len() as u64;
        match send_image(&link, &png) {
            Ok(()) => print_info(&info("out", &peer, "image.png", size, size, "done", XferKind::Image, None, None)),
            Err(e) => print_info(&info("out", &peer, "image.png", size, 0, "failed", XferKind::Image, None, Some(e.to_string()))),
        }
    });
}

/// Something a receiving computer has to act on (the rest is only reported).
pub enum Done {
    /// a complete PNG image: put it on the clipboard
    Image(Vec<u8>),
    /// the files of a clipboard copy: put them on the clipboard
    Files(Vec<PathBuf>),
}

/// Feeds a transfer message to the receiver and reports what happened.
pub fn receive(rx: &mut Receiver, msg: Msg, peer: &str) -> Vec<Done> {
    let mut done = Vec::new();
    for ev in rx.handle(msg) {
        match ev {
            Event::Progress { name, size, done: got, kind } => {
                // a finished drop is reported once more below, with the place it was saved
                if !(kind == XferKind::Drop && got >= size) {
                    let state = if got >= size { "done" } else { "progress" };
                    print_info(&info("in", peer, &name, size, got, state, kind, None, None));
                }
            }
            Event::Image(png) => done.push(Done::Image(png)),
            Event::Files(paths) => done.push(Done::Files(paths)),
            Event::Dropped(path) => {
                let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                print_info(&info("in", peer, &name, size, size, "done", XferKind::Drop, Some(&path), None));
            }
            Event::Failed { name, reason } => print_info(&info("in", peer, &name, 0, 0, "failed", XferKind::Drop, None, Some(reason))),
        }
    }
    done
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::VecDeque,
        sync::{mpsc::channel, Mutex},
        thread,
    };

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("mkshare-xfer-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    /// A sender and a receiver connected by a channel. `delay` slows the receiver down.
    /// Returns the link, the receiver's events and the largest backlog of messages seen.
    struct Pair {
        link: Link,
        events: Arc<Mutex<Vec<String>>>,
        max_backlog: Arc<Mutex<usize>>,
        done: thread::JoinHandle<Receiver>,
        collected: Arc<Mutex<Vec<Event>>>,
    }

    fn pair(dirs: Dirs, delay: Duration) -> Pair {
        let (tx, rx) = channel::<Msg>();
        let acks = Arc::new(AckTable::default());
        let link = Link::new(tx, acks.clone());
        let a = acks.clone();
        let mut receiver = Receiver::new(dirs, move |m| match m {
            Msg::XferAck { id, bytes } => a.ack(id, bytes),
            Msg::XferCancel { id, reason } => a.cancel(id, reason),
            _ => {}
        });
        let events = Arc::new(Mutex::new(Vec::new()));
        let max_backlog = Arc::new(Mutex::new(0usize));
        let collected = Arc::new(Mutex::new(Vec::new()));
        let (mb, col) = (max_backlog.clone(), collected.clone());
        let done = thread::spawn(move || {
            let mut queue: VecDeque<Msg> = VecDeque::new();
            loop {
                while let Ok(m) = rx.try_recv() {
                    queue.push_back(m);
                }
                {
                    let mut g = mb.lock().unwrap();
                    *g = (*g).max(queue.len());
                }
                match queue.pop_front() {
                    Some(m) => {
                        if !delay.is_zero() {
                            thread::sleep(delay);
                        }
                        col.lock().unwrap().extend(receiver.handle(m));
                    }
                    None => match rx.recv_timeout(Duration::from_millis(300)) {
                        Ok(m) => queue.push_back(m),
                        Err(_) => return receiver, // the sender is done
                    },
                }
            }
        });
        Pair { link, events, max_backlog, done, collected }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i.wrapping_mul(31) ^ (i >> 7)) as u8).collect()
    }

    #[test]
    fn a_file_arrives_intact_and_is_not_overwritten() {
        let root = temp("file");
        let dirs = Dirs { inbox: root.join("inbox"), downloads: root.join("dl") };
        let src = root.join("report.bin");
        let bytes = pattern(3 * 1024 * 1024 + 123);
        fs::write(&src, &bytes).unwrap();
        for round in 0..2 {
            let p = pair(dirs.clone(), Duration::ZERO);
            let n = send_paths(&p.link, &[src.clone()], XferKind::Drop, &mut |_, _, _| {}).unwrap();
            assert_eq!(n, 1);
            drop(p.link);
            let _ = p.done.join().unwrap();
            let events = p.collected.lock().unwrap();
            let saved = events.iter().find_map(|e| if let Event::Dropped(p) = e { Some(p.clone()) } else { None }).expect("Dropped event");
            assert_eq!(fs::read(&saved).unwrap(), bytes);
            let expected = if round == 0 { "report.bin" } else { "report (1).bin" };
            assert_eq!(saved.file_name().unwrap().to_str().unwrap(), expected);
            let _ = &p.events;
        }
        // no unfinished files left behind
        assert!(fs::read_dir(root.join("dl")).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".part")));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn the_sender_waits_for_a_slow_receiver() {
        let root = temp("slow");
        let dirs = Dirs { inbox: root.join("inbox"), downloads: root.join("dl") };
        let src = root.join("big.bin");
        fs::write(&src, pattern(4 * 1024 * 1024)).unwrap(); // 128 chunks
        let p = pair(dirs, Duration::from_millis(1));
        send_paths(&p.link, &[src], XferKind::Drop, &mut |_, _, _| {}).unwrap();
        let backlog = *p.max_backlog.lock().unwrap();
        drop(p.link);
        let _ = p.done.join();
        // the window is 8 chunks: without flow control all 128 would pile up
        assert!(backlog <= (WINDOW as usize / CHUNK) + 4, "backlog was {backlog} messages");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn several_files_of_one_copy_arrive_as_one_batch() {
        let root = temp("batch");
        let dirs = Dirs { inbox: root.join("inbox"), downloads: root.join("dl") };
        let mut files = Vec::new();
        for (i, len) in [10usize, 0, 70_000].into_iter().enumerate() {
            let f = root.join(format!("f{i}.txt"));
            fs::write(&f, pattern(len)).unwrap();
            files.push(f);
        }
        let p = pair(dirs, Duration::ZERO);
        send_paths(&p.link, &files, XferKind::File, &mut |_, _, _| {}).unwrap();
        drop(p.link);
        let _ = p.done.join();
        let events = p.collected.lock().unwrap();
        let batches: Vec<&Vec<PathBuf>> = events.iter().filter_map(|e| if let Event::Files(p) = e { Some(p) } else { None }).collect();
        assert_eq!(batches.len(), 1, "exactly one Files event");
        assert_eq!(batches[0].len(), 3);
        assert_eq!(fs::metadata(&batches[0][1]).unwrap().len(), 0);
        assert_eq!(fs::read(&batches[0][2]).unwrap(), pattern(70_000));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn an_image_arrives_in_memory() {
        let root = temp("image");
        let p = pair(Dirs { inbox: root.join("i"), downloads: root.join("d") }, Duration::ZERO);
        let png = pattern(100_000);
        send_image(&p.link, &png).unwrap();
        drop(p.link);
        let _ = p.done.join();
        let events = p.collected.lock().unwrap();
        assert!(events.iter().any(|e| matches!(e, Event::Image(b) if *b == png)));
        assert!(!root.join("i").exists() && !root.join("d").exists(), "images never touch the disk");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn too_large_or_malicious_transfers_are_refused() {
        let root = temp("refuse");
        let dirs = Dirs { inbox: root.join("inbox"), downloads: root.join("dl") };
        let cancelled = Arc::new(Mutex::new(Vec::new()));
        let c = cancelled.clone();
        let mut r = Receiver::new(dirs, move |m| {
            if let Msg::XferCancel { id, reason } = m {
                c.lock().unwrap().push((id, reason));
            }
        });
        let begin = |id, kind, name: &str, size| Msg::XferBegin { id, kind, name: name.into(), size, batch: id, index: 0, count: 1 };
        // an image above the limit
        assert!(matches!(r.handle(begin(1, XferKind::Image, "x.png", MAX_IMAGE + 1)).as_slice(), [Event::Failed { .. }]));
        // more files than allowed in one clipboard copy
        let many = Msg::XferBegin { id: 2, kind: XferKind::File, name: "a".into(), size: 1, batch: 2, index: 0, count: 51 };
        assert!(matches!(r.handle(many).as_slice(), [Event::Failed { .. }]));
        // a path in the name never escapes the folder
        let ev = r.handle(begin(3, XferKind::Drop, "..\\..\\evil.txt", 3));
        assert!(matches!(ev.as_slice(), [Event::Progress { name, .. }] if name == "evil.txt"));
        // more data than announced
        r.handle(Msg::XferData { id: 3, bytes: vec![1, 2, 3] });
        let ev = r.handle(Msg::XferData { id: 3, bytes: vec![4] });
        assert!(matches!(ev.as_slice(), [Event::Failed { reason, .. }] if reason.contains("more data")));
        assert_eq!(cancelled.lock().unwrap().len(), 3);
        // nothing was left behind outside the folders, and no partial file remains
        assert!(!root.join("evil.txt").exists());
        let leftovers: Vec<_> = fs::read_dir(root.join("dl")).map(|d| d.flatten().collect()).unwrap_or_default();
        assert!(leftovers.is_empty(), "partial files left: {leftovers:?}");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_cancelled_transfer_removes_its_partial_file() {
        let root = temp("cancel");
        let mut r = Receiver::new(Dirs { inbox: root.join("i"), downloads: root.join("d") }, |_| {});
        r.handle(Msg::XferBegin { id: 5, kind: XferKind::Drop, name: "a.bin".into(), size: 100, batch: 5, index: 0, count: 1 });
        r.handle(Msg::XferData { id: 5, bytes: vec![0; 10] });
        assert!(root.join("d").join("a.bin.part").exists());
        r.handle(Msg::XferCancel { id: 5, reason: "user".into() });
        assert!(!root.join("d").join("a.bin.part").exists());
        // and a connection that ends in the middle does the same
        r.handle(Msg::XferBegin { id: 6, kind: XferKind::Drop, name: "b.bin".into(), size: 100, batch: 6, index: 0, count: 1 });
        assert!(root.join("d").join("b.bin.part").exists());
        r.abort_all();
        assert!(!root.join("d").join("b.bin.part").exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn file_names_are_made_safe() {
        assert_eq!(safe_name("report.pdf"), "report.pdf");
        assert_eq!(safe_name("..\\..\\Windows\\evil.exe"), "evil.exe");
        assert_eq!(safe_name("/etc/passwd"), "passwd");
        assert_eq!(safe_name("a<b>c:d|e?.txt"), "abcde.txt");
        assert_eq!(safe_name("trailing. . "), "trailing");
        assert_eq!(safe_name(""), "file");
        assert_eq!(safe_name(".."), "file");
        assert_eq!(safe_name("CON"), "_CON");
        assert_eq!(safe_name("com1.txt"), "_com1.txt");
        assert_eq!(safe_name("nul.tar.gz"), "_nul.tar.gz");
        assert_eq!(safe_name("a\u{0}b\nc.txt"), "abc.txt");
        assert_eq!(safe_name(&"x".repeat(400)).chars().count(), 150);
        assert_eq!(safe_name("Büro-Übersicht.pdf"), "Büro-Übersicht.pdf");
    }

    #[test]
    fn unique_paths_never_overwrite() {
        let d = temp("unique");
        assert_eq!(unique_path(&d, "a.txt"), d.join("a.txt"));
        fs::write(d.join("a.txt"), "1").unwrap();
        assert_eq!(unique_path(&d, "a.txt"), d.join("a (1).txt"));
        fs::write(d.join("a (1).txt"), "2").unwrap();
        assert_eq!(unique_path(&d, "a.txt"), d.join("a (2).txt"));
        fs::write(d.join("noext"), "3").unwrap();
        assert_eq!(unique_path(&d, "noext"), d.join("noext (1)"));
        fs::write(d.join(".hidden"), "4").unwrap();
        assert_eq!(unique_path(&d, ".hidden"), d.join(".hidden (1)"));
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn folders_and_missing_files_are_left_out() {
        let d = temp("usable");
        fs::write(d.join("f.txt"), "x").unwrap();
        fs::create_dir_all(d.join("sub")).unwrap();
        let (files, notes) = usable_files(&[d.join("f.txt"), d.join("sub"), d.join("gone.txt")]);
        assert_eq!(files, vec![d.join("f.txt")]);
        assert_eq!(notes.len(), 2);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn old_clipboard_folders_are_cleaned_up() {
        let inbox = temp("clean");
        fs::create_dir_all(inbox.join("old")).unwrap();
        fs::write(inbox.join("old").join("x"), "1").unwrap();
        clean_inbox(&inbox, Duration::from_secs(3600));
        assert!(inbox.join("old").exists(), "recent folders stay");
        clean_inbox(&inbox, Duration::ZERO);
        thread::sleep(Duration::from_millis(20));
        clean_inbox(&inbox, Duration::from_millis(1));
        assert!(!inbox.join("old").exists());
        let _ = fs::remove_dir_all(inbox);
    }
}
