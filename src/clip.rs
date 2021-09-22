use anyhow::{Error, Result};
use arboard::{Clipboard, ImageData};
use clipboard_master::{CallbackResult, ClipboardHandler};
use serde::{Deserialize, Serialize};
use std::{borrow::Cow, sync::Arc};
use tokio::sync::{
    mpsc::{self, Receiver, Sender},
    RwLock,
};

#[derive(Debug, Serialize, Deserialize)]
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

pub struct Clip {
    md5: Arc<RwLock<String>>,
    clipboard: Arc<RwLock<Clipboard>>,
    tx: Sender<ClipContext>,
}

impl Clip {
    pub async fn new() -> (Arc<Self>, Receiver<ClipContext>) {
        Self::with_buffer(16).await
    }

    pub async fn with_buffer(buffer: usize) -> (Arc<Self>, Receiver<ClipContext>) {
        let md5 = Arc::new(RwLock::new(String::new()));
        let clipboard = Arc::new(RwLock::new(
            Clipboard::new().expect("Failed to create clipboard!"),
        ));
        let (tx, rx) = mpsc::channel::<ClipContext>(1024);

        (
            Arc::new(Clip {
                md5: md5,
                clipboard: clipboard,
                tx,
            }),
            rx,
        )
    }

    pub async fn set_clip(&self, clip: ClipContext) -> Result<()> {
        let mut pre_md5 = self.md5.write().await;
        let mut clipboard = self.clipboard.write().await;

        let (prop, bytes) = (clip.prop, clip.bytes);
        let md5 = &format!("{:x}", md5::compute(&bytes));

        match match clip.kinds {
            ClipContextKinds::TEXT => clipboard.set_text(String::from_utf8(bytes)?),
            ClipContextKinds::IMAGE => {
                let img = ImageData {
                    width: prop.get(1).unwrap().parse()?,
                    height: prop.get(0).unwrap().parse()?,
                    bytes: Cow::from(bytes),
                };
                clipboard.set_image(img)
            }
            ClipContextKinds::NONE => Err(arboard::Error::ContentNotAvailable),
        } {
            Ok(_) => {
                pre_md5.clone_from(md5);
                pre_md5.shrink_to_fit();
                Ok(())
            }
            Err(err) => Err(Error::new(err)),
        }
    }

    pub async fn listen(&self) {
        let mut pre_md5 = self.md5.write().await;
        let mut clipboard = self.clipboard.write().await;

        let (text, text_md5) = match clipboard.get_text() {
            Ok(text) => {
                let md5 = format!("{:x}", md5::compute(&text));
                (Some(text), md5)
            }
            _ => (None, String::from("")),
        };
        let (image, image_md5) = match clipboard.get_image() {
            Ok(image) => {
                let md5 = format!("{:x}", md5::compute(&image.bytes));
                (Some((image.bytes.to_vec(), image.height, image.width)), md5)
            }
            _ => (None, String::from("")),
        };

        let (prop, bytes, md5, kinds) = if text.is_some() {
            let bytes = text.unwrap().as_bytes().to_vec();
            (vec![], bytes, text_md5, ClipContextKinds::TEXT)
        } else if image.is_some() {
            let (bytes, height, width) = image.unwrap();
            let prop = {
                let mut prop = Vec::with_capacity(2);
                prop.push(height.to_string());
                prop.push(width.to_string());
                prop
            };
            (prop, bytes, image_md5, ClipContextKinds::IMAGE)
        } else {
            (vec![], vec![], String::from(""), ClipContextKinds::NONE)
        };

        if !md5.eq("") && !pre_md5.eq(&md5) {
            if let Ok(_) = self
                .tx
                .send(ClipContext {
                    kinds: kinds,
                    prop: prop,
                    bytes: bytes,
                })
                .await
            {
                pre_md5.clone_from(&md5);
                pre_md5.shrink_to_fit();
            };
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
