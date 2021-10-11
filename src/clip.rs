use anyhow::{Error, Result};
use arboard::{Clipboard, ImageData};
use clipboard_master::{CallbackResult, ClipboardHandler};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, sync::Arc};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    Mutex,
};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClipContextKinds {
    TEXT,
    IMAGE,
    NONE,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ClipContext {
    pub kinds: ClipContextKinds,
    pub prop: Vec<String>,
    pub bytes: Vec<u8>,
}

struct ClipCore {
    md5: String,
    clip: Clipboard,
}

pub struct Clip {
    core: Mutex<ClipCore>,
    tx: Sender<ClipContext>,
}

impl Clip {
    pub async fn new() -> (Arc<Self>, Receiver<ClipContext>) {
        Self::with_buffer(16).await
    }

    pub async fn with_buffer(buffer: usize) -> (Arc<Self>, Receiver<ClipContext>) {
        let core = Mutex::new(ClipCore {
            md5: String::new(),
            clip: Clipboard::new().expect("Failed to create clipboard!"),
        });
        let (tx, rx) = mpsc::channel::<ClipContext>(1024);

        (Arc::new(Clip { core, tx }), rx)
    }

    pub async fn set_clip(&self, clip: ClipContext) -> Result<()> {
        let mut core = self.core.lock().await;

        let (prop, bytes) = (clip.prop, clip.bytes);
        let md5 = format!("{:x}", md5::compute(&bytes));

        let result = match clip.kinds {
            ClipContextKinds::TEXT => core.clip.set_text(String::from_utf8(bytes)?),
            ClipContextKinds::IMAGE => {
                let img = ImageData {
                    width: prop.get(1).unwrap().parse()?,
                    height: prop.get(0).unwrap().parse()?,
                    bytes: Cow::from(bytes),
                };
                core.clip.set_image(img)
            }
            ClipContextKinds::NONE => Err(arboard::Error::ContentNotAvailable),
        };

        match result {
            Ok(_) => {
                core.md5.clone_from(&md5);
                core.md5.shrink_to_fit();
                Ok(())
            }
            Err(err) => Err(Error::new(err)),
        }
    }

    pub async fn listen(&self) {
        let mut core = self.core.lock().await;

        let (text, image) = (core.clip.get_text(), core.clip.get_image());
        let (prop, bytes, kinds) = if text.is_ok() {
            let text = text.unwrap();
            let bytes = text.as_bytes().to_vec();

            (vec![], bytes, ClipContextKinds::TEXT)
        } else if image.is_ok() {
            let image = image.unwrap();
            let prop = {
                let mut prop = Vec::with_capacity(2);
                prop.push(image.height.to_string());
                prop.push(image.width.to_string());
                prop
            };
            let bytes = image.bytes.to_vec();

            (prop, bytes, ClipContextKinds::IMAGE)
        } else {
            (vec![], vec![], ClipContextKinds::NONE)
        };

        if !ClipContextKinds::NONE.eq(&kinds) {
            let md5 = format!("{:x}", md5::compute(&bytes));

            if !core.md5.eq(&md5) {
                if let Ok(_) = self
                    .tx
                    .send(ClipContext {
                        kinds: kinds,
                        prop: prop,
                        bytes: bytes,
                    })
                    .await
                {
                    core.md5.clone_from(&md5);
                    core.md5.shrink_to_fit();
                };
            }
        }
    }
}

pub struct ClipHandle {
    pub clip: Arc<Clip>,
}

impl ClipboardHandler for ClipHandle {
    fn on_clipboard_change(&mut self) -> CallbackResult {
        let clip = self.clip.clone();
        tokio::spawn(async move {
            clip.listen().await;
        });
        CallbackResult::Next
    }
}
