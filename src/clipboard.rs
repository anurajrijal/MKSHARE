//! Clipboard sharing: text, images and copied files.
//!
//! One background thread owns the system clipboard. It polls for changes made on this
//! computer (and reports them through the `send` callback) and applies what the other
//! computer sent. Whatever we set ourselves is remembered, so it is never echoed back
//! (which would make the two computers ping-pong).
//!
//! Order of precedence when several things are on the clipboard at once: copied files, then
//! text, then an image (a spreadsheet cell, for example, is text *and* a picture: the text wins).

use crate::xfer;
use arboard::ImageData;
use std::{
    borrow::Cow,
    io::Cursor,
    path::PathBuf,
    sync::mpsc::{self, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant},
};

/// Largest text that is shared (bytes). Must fit in one encrypted frame (see net.rs).
pub const MAX_TEXT: usize = 32 * 1024;

const POLL: Duration = Duration::from_millis(400);
/// An image that stays on the clipboard is looked at again only this often (decoding it is not free).
const IMAGE_RECHECK: Duration = Duration::from_millis(1500);
const MAX_PIXELS: u64 = 120_000_000;

/// Something the user copied on this computer.
#[derive(Debug)]
pub enum ClipEvent {
    Text(String),
    /// a PNG image
    Image(Vec<u8>),
    /// files that can be sent (checked: regular files, within the limits)
    Files(Vec<PathBuf>),
    /// something that was copied but cannot be shared, and why
    Notice(String),
}

/// What to share besides text.
#[derive(Clone, Copy)]
pub struct Options {
    pub images: bool,
    pub files: bool,
}

enum Incoming {
    Text(String),
    Image(Vec<u8>),
    Files(Vec<PathBuf>),
}

/// Handle used to put what the other computer sent on this clipboard.
#[derive(Clone)]
pub struct Clipboard {
    tx: Sender<Incoming>,
}

impl Clipboard {
    pub fn set(&self, text: String) {
        if text.len() <= MAX_TEXT {
            let _ = self.tx.send(Incoming::Text(text));
        }
    }

    pub fn set_image(&self, png: Vec<u8>) {
        let _ = self.tx.send(Incoming::Image(png));
    }

    pub fn set_files(&self, paths: Vec<PathBuf>) {
        let _ = self.tx.send(Incoming::Files(paths));
    }
}

// ----------------------------------------------------------------- images

fn hash(bytes: &[u8]) -> u64 {
    // FNV-1a over 8 bytes at a time: fast enough for screenshots, and good enough to notice a change
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut chunks = bytes.chunks_exact(8);
    for c in &mut chunks {
        h = (h ^ u64::from_le_bytes(c.try_into().unwrap())).wrapping_mul(0x0000_0100_0000_01b3);
    }
    for b in chunks.remainder() {
        h = (h ^ *b as u64).wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^ bytes.len() as u64
}

pub fn encode_png(img: &ImageData) -> Option<Vec<u8>> {
    let (w, h) = (img.width as u32, img.height as u32);
    if w == 0 || h == 0 || img.bytes.len() != (w as usize) * (h as usize) * 4 {
        return None;
    }
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::Fast);
    let mut writer = enc.write_header().ok()?;
    writer.write_image_data(&img.bytes).ok()?;
    writer.finish().ok()?;
    Some(out)
}

pub fn decode_png(data: &[u8]) -> Option<ImageData<'static>> {
    let mut dec = png::Decoder::new(Cursor::new(data));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().ok()?;
    let info = reader.info();
    if info.width == 0 || info.height == 0 || (info.width as u64) * (info.height as u64) > MAX_PIXELS {
        return None;
    }
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).ok()?;
    let pixels = &buf[..frame.buffer_size()];
    let rgba: Vec<u8> = match frame.color_type {
        png::ColorType::Rgba => pixels.to_vec(),
        png::ColorType::Rgb => pixels.chunks_exact(3).flat_map(|p| [p[0], p[1], p[2], 255]).collect(),
        png::ColorType::GrayscaleAlpha => pixels.chunks_exact(2).flat_map(|p| [p[0], p[0], p[0], p[1]]).collect(),
        png::ColorType::Grayscale => pixels.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => return None, // EXPAND turns these into RGB(A)
    };
    Some(ImageData { width: frame.width as usize, height: frame.height as usize, bytes: Cow::Owned(rgba) })
}

// ------------------------------------------------------------------ files

/// Checks a copied selection against the limits. Err explains why it is not shared.
fn files_to_share(list: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
    let (files, notes) = xfer::usable_files(list);
    if files.is_empty() {
        return Err(notes.first().cloned().unwrap_or_else(|| "nothing to share".into()));
    }
    if files.len() > xfer::MAX_CLIP_FILES {
        return Err(format!("{} files copied: at most {} can be shared by copying (drop them onto the MKShare window instead)", files.len(), xfer::MAX_CLIP_FILES));
    }
    let total: u64 = files.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
    if total > xfer::MAX_CLIP_FILES_TOTAL {
        return Err(format!(
            "the copied files are {} MB: at most {} MB can be shared by copying (drop them onto the MKShare window instead)",
            total >> 20,
            xfer::MAX_CLIP_FILES_TOTAL >> 20
        ));
    }
    Ok(files)
}

// ---------------------------------------------------------------- watching

/// A number that changes whenever the clipboard changes (Windows only: lets us skip reading it).
#[cfg(windows)]
fn clipboard_seq() -> Option<u32> {
    Some(unsafe { windows_sys::Win32::System::DataExchange::GetClipboardSequenceNumber() })
}

#[cfg(not(windows))]
fn clipboard_seq() -> Option<u32> {
    None
}

struct Watcher {
    opts: Options,
    last_text: String,
    last_files: Vec<PathBuf>,
    last_image: u64,
    last_seq: Option<u32>,
    next_image_check: Instant,
}

impl Watcher {
    /// Remember what is on the clipboard right now, so it counts as "already seen".
    fn absorb_text_and_image(&mut self, cb: &mut arboard::Clipboard) {
        if let Ok(t) = cb.get_text() {
            self.last_text = t;
        }
        if self.opts.images {
            if let Ok(i) = cb.get_image() {
                self.last_image = hash(&i.bytes);
            }
        }
    }

    fn poll(&mut self, cb: &mut arboard::Clipboard, send: &dyn Fn(ClipEvent)) {
        let seq = clipboard_seq();
        if seq.is_some() && seq == self.last_seq {
            return; // nothing changed since the last look
        }
        let mut busy = false; // another program had the clipboard open: look again next time

        // 1. copied files
        if self.opts.files {
            match cb.get().file_list() {
                Ok(list) if !list.is_empty() => {
                    if list != self.last_files {
                        self.last_files = list.clone();
                        match files_to_share(&list) {
                            Ok(files) => send(ClipEvent::Files(files)),
                            Err(why) => send(ClipEvent::Notice(why)),
                        }
                        self.absorb_text_and_image(cb); // e.g. the file names as text: not shared
                    }
                    self.last_seq = seq;
                    return;
                }
                Err(arboard::Error::ClipboardOccupied) => busy = true,
                _ => self.last_files.clear(),
            }
        }

        // 2. text
        match cb.get_text() {
            Ok(text) => {
                if text != self.last_text {
                    self.last_text = text.clone();
                    if !text.is_empty() && text.len() <= MAX_TEXT {
                        send(ClipEvent::Text(text));
                    }
                    if self.opts.images {
                        if let Ok(i) = cb.get_image() {
                            self.last_image = hash(&i.bytes); // the picture that comes with the text: not shared
                        }
                    }
                    if !busy {
                        self.last_seq = seq;
                    }
                    return;
                }
            }
            Err(arboard::Error::ClipboardOccupied) => busy = true,
            Err(_) => self.last_text.clear(), // not text any more: copying the same text again counts as new
        }

        // 3. an image
        if self.opts.images && Instant::now() >= self.next_image_check {
            match cb.get_image() {
                Ok(img) => {
                    let h = hash(&img.bytes);
                    if h != self.last_image {
                        self.last_image = h;
                        match encode_png(&img) {
                            Some(png) if png.len() as u64 <= xfer::MAX_IMAGE => send(ClipEvent::Image(png)),
                            Some(png) => send(ClipEvent::Notice(format!("the copied image is {} MB: too large to share", png.len() >> 20))),
                            None => {}
                        }
                    } else {
                        self.next_image_check = Instant::now() + IMAGE_RECHECK; // same picture still there
                    }
                }
                Err(arboard::Error::ClipboardOccupied) => busy = true,
                Err(_) => self.last_image = 0,
            }
        }
        if !busy {
            self.last_seq = seq;
        }
    }

    fn apply(&mut self, cb: &mut arboard::Clipboard, incoming: Incoming) {
        match incoming {
            Incoming::Text(text) => {
                if text != self.last_text && cb.set_text(text.clone()).is_ok() {
                    self.last_text = text;
                }
            }
            Incoming::Image(png) => {
                let Some(img) = decode_png(&png) else { return };
                let set_hash = hash(&img.bytes);
                if cb.set_image(img).is_ok() {
                    // what the system hands back may differ slightly from what we set: remember that version
                    self.last_image = cb.get_image().map(|i| hash(&i.bytes)).unwrap_or(set_hash);
                    self.last_text.clear();
                    self.last_files.clear();
                }
            }
            Incoming::Files(paths) => {
                if cb.set().file_list(&paths).is_ok() {
                    // paths may come back in another form (Windows adds prefixes): remember the system's version
                    self.last_files = cb.get().file_list().unwrap_or(paths);
                    self.last_text.clear();
                }
            }
        }
        self.last_seq = None; // look again: the clipboard just changed (by us)
    }
}

/// Starts the clipboard thread. `send` is called with everything new that is copied on this computer.
/// Returns None if the clipboard cannot be used here (e.g. no desktop session).
pub fn start(opts: Options, send: impl Fn(ClipEvent) + Send + 'static) -> Option<Clipboard> {
    let (tx, rx) = mpsc::channel::<Incoming>();
    let (ready_tx, ready_rx) = mpsc::channel::<bool>();
    thread::spawn(move || {
        let mut cb = match arboard::Clipboard::new() {
            Ok(c) => {
                let _ = ready_tx.send(true);
                c
            }
            Err(e) => {
                eprintln!("Clipboard sharing is unavailable: {e}");
                let _ = ready_tx.send(false);
                return;
            }
        };
        let mut w = Watcher {
            opts,
            last_text: String::new(),
            last_files: Vec::new(),
            last_image: 0,
            last_seq: None,
            next_image_check: Instant::now(),
        };
        // Whatever is on the clipboard now is not "new": only later changes are shared.
        if opts.files {
            w.last_files = cb.get().file_list().unwrap_or_default();
        }
        w.absorb_text_and_image(&mut cb);
        w.last_seq = clipboard_seq();
        loop {
            match rx.recv_timeout(POLL) {
                Ok(incoming) => w.apply(&mut cb, incoming),
                Err(RecvTimeoutError::Timeout) => w.poll(&mut cb, &send),
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
    });
    if ready_rx.recv().unwrap_or(false) {
        Some(Clipboard { tx })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gradient(w: usize, h: usize) -> ImageData<'static> {
        let bytes: Vec<u8> = (0..w * h).flat_map(|i| [(i % 251) as u8, (i / w) as u8, 77, if i % 7 == 0 { 0 } else { 255 }]).collect();
        ImageData { width: w, height: h, bytes: Cow::Owned(bytes) }
    }

    #[test]
    fn png_round_trip_keeps_every_pixel() {
        let img = gradient(37, 23);
        let png = encode_png(&img).expect("encode");
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        let back = decode_png(&png).expect("decode");
        assert_eq!((back.width, back.height), (37, 23));
        assert_eq!(back.bytes, img.bytes);
    }

    #[test]
    fn broken_or_hostile_png_data_is_rejected() {
        assert!(decode_png(b"").is_none());
        assert!(decode_png(b"not a png at all").is_none());
        let mut png = encode_png(&gradient(10, 10)).unwrap();
        png.truncate(png.len() / 2);
        assert!(decode_png(&png).is_none());
        // wrong buffer size for the stated dimensions
        let bad = ImageData { width: 10, height: 10, bytes: Cow::Owned(vec![0; 12]) };
        assert!(encode_png(&bad).is_none());
    }

    #[test]
    fn other_png_color_types_are_converted_to_rgba() {
        // 2x1 RGB image written by hand
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 2, 1);
            enc.set_color(png::ColorType::Rgb);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[10, 20, 30, 40, 50, 60]).unwrap();
        }
        let img = decode_png(&out).unwrap();
        assert_eq!(img.bytes.as_ref(), &[10, 20, 30, 255, 40, 50, 60, 255]);
        // grayscale
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, 1, 2);
            enc.set_color(png::ColorType::Grayscale);
            enc.set_depth(png::BitDepth::Eight);
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[9, 99]).unwrap();
        }
        assert_eq!(decode_png(&out).unwrap().bytes.as_ref(), &[9, 9, 9, 255, 99, 99, 99, 255]);
    }

    #[test]
    fn the_hash_notices_small_changes() {
        let a = vec![5u8; 1000];
        let mut b = a.clone();
        assert_eq!(hash(&a), hash(&b));
        b[999] = 6;
        assert_ne!(hash(&a), hash(&b));
        b[999] = 5;
        b[0] = 4;
        assert_ne!(hash(&a), hash(&b));
        assert_ne!(hash(&[1, 2, 3]), hash(&[1, 2, 3, 0]));
    }

    #[test]
    fn copied_files_must_be_within_the_limits() {
        let dir = std::env::temp_dir().join(format!("mkshare-clip-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("folder")).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        // a folder alone is not shareable, a file with a folder next to it shares the file
        assert!(files_to_share(&[dir.join("folder")]).is_err());
        assert_eq!(files_to_share(&[dir.join("a.txt"), dir.join("folder")]).unwrap(), vec![dir.join("a.txt")]);
        assert!(files_to_share(&[dir.join("missing")]).is_err());
        // too many files
        let many: Vec<PathBuf> = (0..xfer::MAX_CLIP_FILES + 1).map(|_| dir.join("a.txt")).collect();
        assert!(files_to_share(&many).unwrap_err().contains("at most"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
