use crate::RUNNING;
use arboard::{Clipboard as _Clipboard, ImageData};
use blake3::Hash;
use clipboard_master::{CallbackResult, ClipboardHandler, Master};
use flume::{Receiver, RecvError, Sender};
use serde::{Deserialize, Serialize};
use std::{
    borrow::Cow,
    error::Error,
    io,
    sync::{atomic::Ordering, Arc, Mutex},
    thread::{self, JoinHandle},
};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClipboardContentKinds {
    TEXT,
    IMAGE,
    NONE,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClipboardContent {
    pub kinds: ClipboardContentKinds,
    pub prop: Vec<String>,
    pub bytes: Vec<u8>,
}

pub struct Clipboard {
    core: Mutex<ClipboardCore>,
}

struct ClipboardCore {
    hash: Hash,
    clip: _Clipboard,
}

impl Clipboard {
    pub fn new() -> Self {
        let core = Mutex::new(ClipboardCore {
            hash: blake3::hash(b""),
            clip: _Clipboard::new().expect("Failed to create clipboard!"),
        });

        Clipboard { core }
    }

    pub fn set<'a>(&'a self, content: ClipboardContent) -> Result<(), Box<dyn Error + 'a>> {
        let mut core = self.core.lock()?;

        let (prop, bytes) = (content.prop, content.bytes);
        let hash = blake3::hash(&bytes);

        if let Err(e) = match content.kinds {
            ClipboardContentKinds::TEXT => core
                .clip
                .set_text(String::from_utf8_lossy(&bytes).to_string()),
            ClipboardContentKinds::IMAGE => core.clip.set_image({
                let width = match prop.get(1) {
                    Some(width) => width,
                    None => return Err(Box::new(arboard::Error::ContentNotAvailable)),
                };
                let height = match prop.get(0) {
                    Some(height) => height,
                    None => return Err(Box::new(arboard::Error::ContentNotAvailable)),
                };

                ImageData {
                    width: width.parse()?,
                    height: height.parse()?,
                    bytes: Cow::from(&bytes),
                }
            }),
            ClipboardContentKinds::NONE => Err(arboard::Error::ContentNotAvailable),
        } {
            return Err(Box::new(e));
        }

        core.hash.clone_from(&hash);
        Ok(())
    }

    pub fn get<'a>(&'a self) -> Result<ClipboardContent, Box<dyn Error + 'a>> {
        let mut core = self.core.lock()?;

        let (text, image) = (core.clip.get_text(), core.clip.get_image());
        let (prop, bytes, kinds) = if let Ok(text) = text {
            let bytes: Vec<u8> = Vec::from(text.as_bytes());

            (vec![], bytes, ClipboardContentKinds::TEXT)
        } else if let Ok(image) = image {
            let prop = vec![image.height.to_string(), image.width.to_string()];
            let bytes: Vec<u8> = Vec::from(&*image.bytes);

            (prop, bytes, ClipboardContentKinds::IMAGE)
        } else {
            (vec![], vec![], ClipboardContentKinds::NONE)
        };

        Ok(ClipboardContent { kinds, prop, bytes })
    }
}

pub struct Listener {
    clipboard: Arc<Clipboard>,
    handle: Option<JoinHandle<()>>,
    receiver: Receiver<ClipboardContent>,
}

impl Listener {
    pub fn new(clipboard: Arc<Clipboard>) -> Self {
        let (sender, receiver) = flume::bounded(64);

        let handle = {
            let clipboard = clipboard.clone();

            thread::spawn(move || {
                if let Err(e) = Master::new(MyHandler { clipboard, sender }).run() {
                    panic!("{:#}", e);
                }
            })
        };

        Listener {
            clipboard,
            handle: Some(handle),
            receiver,
        }
    }

    pub async fn recv(&self) -> Result<ClipboardContent, RecvError> {
        self.receiver.recv_async().await
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        if let Ok(mut content) = self.clipboard.get() {
            let _ = self.clipboard.set(match content.kinds {
                ClipboardContentKinds::TEXT | ClipboardContentKinds::IMAGE => content,
                ClipboardContentKinds::NONE => {
                    content.kinds = ClipboardContentKinds::TEXT;
                    content
                }
            });
        }

        if let Some(handle) = self.handle.take() {
            if let Err(e) = handle.join() {
                panic!("{:#?}", e);
            }
        }
    }
}

struct MyHandler {
    clipboard: Arc<Clipboard>,
    sender: Sender<ClipboardContent>,
}

impl MyHandler {
    pub fn handle(&self) -> Result<(), Box<dyn Error + '_>> {
        let content = self.clipboard.get()?;

        if !ClipboardContentKinds::NONE.eq(&content.kinds) {
            let hash = blake3::hash(&content.bytes);

            let mut core = self.clipboard.core.lock()?;
            if !core.hash.eq(&hash) {
                if let Ok(()) = self.sender.send(content) {
                    core.hash.clone_from(&hash);
                }
            }
        }

        Ok(())
    }
}

impl ClipboardHandler for MyHandler {
    fn on_clipboard_change(&mut self) -> CallbackResult {
        if !RUNNING.load(Ordering::SeqCst) {
            CallbackResult::Stop
        } else {
            if let Err(e) = self.handle() {
                println!("Handle clipboard failure! {:?}", e)
            }

            CallbackResult::Next
        }
    }

    fn on_clipboard_error(&mut self, error: io::Error) -> CallbackResult {
        if !RUNNING.load(Ordering::SeqCst) {
            CallbackResult::Stop
        } else {
            CallbackResult::StopWithError(error)
        }
    }
}
