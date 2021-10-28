use arboard::{Clipboard as _Clipboard, ImageData};
use blake3::Hash;
use clipboard_master::{CallbackResult, ClipboardHandler};
use minivec::{mini_vec, MiniVec};
use serde::{Deserialize, Serialize};
use smallvec::{smallvec, SmallVec};
use std::{
    borrow::Cow,
    error::Error,
    io,
    sync::{atomic::Ordering, Arc},
};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Mutex,
};

use crate::RUNNING;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClipboardContentKinds {
    TEXT,
    IMAGE,
    NONE,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClipContent {
    pub kinds: ClipboardContentKinds,
    pub prop: SmallVec<[String; 8]>,
    pub bytes: MiniVec<u8>,
}

struct ClipboardCore {
    hash: Hash,
    clip: _Clipboard,
}

pub struct Clipboard {
    core: Mutex<ClipboardCore>,
    tx: Sender<ClipContent>,
}

impl Clipboard {
    pub async fn new() -> (Arc<Self>, Receiver<ClipContent>) {
        Self::with_buffer(16).await
    }

    pub async fn with_buffer(buffer: usize) -> (Arc<Self>, Receiver<ClipContent>) {
        let core = Mutex::new(ClipboardCore {
            hash: blake3::hash(b""),
            clip: _Clipboard::new().expect("Failed to create clipboard!"),
        });
        let (tx, rx) = mpsc::channel::<ClipContent>(1024);

        (Arc::new(Clipboard { core, tx }), rx)
    }

    pub async fn set_content(&self, content: ClipContent) -> Result<(), Box<dyn Error>> {
        let mut core = self.core.lock().await;

        let (prop, bytes) = (content.prop, content.bytes);
        let hash = blake3::hash(&bytes);

        let result = match content.kinds {
            ClipboardContentKinds::TEXT => core
                .clip
                .set_text(String::from_utf8_lossy(bytes.as_slice()).to_string()),
            ClipboardContentKinds::IMAGE => {
                let img = ImageData {
                    width: prop.get(1).unwrap().parse()?,
                    height: prop.get(0).unwrap().parse()?,
                    bytes: Cow::from(bytes.as_slice()),
                };
                core.clip.set_image(img)
            }
            ClipboardContentKinds::NONE => Err(arboard::Error::ContentNotAvailable),
        };

        match result {
            Ok(_) => {
                core.hash.clone_from(&hash);
                Ok(())
            }
            Err(err) => Err(Box::new(err)),
        }
    }

    pub async fn get_content(&self) -> Result<ClipContent, Box<dyn Error>> {
        let mut core = self.core.lock().await;

        let (text, image) = (core.clip.get_text(), core.clip.get_image());
        let (prop, bytes, kinds) = if text.is_ok() {
            let text = text.unwrap();
            let bytes: MiniVec<u8> = MiniVec::from(text.as_bytes());

            (smallvec![], bytes, ClipboardContentKinds::TEXT)
        } else if image.is_ok() {
            let image = image.unwrap();
            let prop = {
                let mut prop = SmallVec::with_capacity(2);
                prop.push(image.height.to_string());
                prop.push(image.width.to_string());
                prop
            };
            let bytes: MiniVec<u8> = MiniVec::from(&*image.bytes);

            (prop, bytes, ClipboardContentKinds::IMAGE)
        } else {
            (smallvec![], mini_vec![], ClipboardContentKinds::NONE)
        };

        Ok(ClipContent {
            kinds: kinds,
            prop: prop,
            bytes: bytes,
        })
    }
}

pub struct MyHandler {
    pub clipboard: Arc<Clipboard>,
}

impl MyHandler {
    pub async fn handle(&self) -> Result<(), Box<dyn Error>> {
        let content = self.clipboard.get_content().await?;

        if !ClipboardContentKinds::NONE.eq(&content.kinds) {
            let hash = blake3::hash(&content.bytes);

            let mut core = self.clipboard.core.lock().await;
            if !core.hash.eq(&hash) {
                if let Ok(_) = self.clipboard.tx.send(content).await {
                    core.hash.clone_from(&hash);
                };
            }
        }

        Ok(())
    }
}

impl ClipboardHandler for MyHandler {
    #[tokio::main]
    async fn on_clipboard_change(&mut self) -> CallbackResult {
        if !RUNNING.load(Ordering::SeqCst) {
            return CallbackResult::Stop;
        }

        match self.handle().await {
            Ok(_) => {}
            _ => println!("Handle clipboard failure!"),
        };

        CallbackResult::Next
    }

    fn on_clipboard_error(&mut self, error: io::Error) -> CallbackResult {
        if !RUNNING.load(Ordering::SeqCst) {
            CallbackResult::Stop
        } else {
            CallbackResult::StopWithError(error)
        }
    }
}
