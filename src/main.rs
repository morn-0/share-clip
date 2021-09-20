use anyhow::Result;
use arboard::{Clipboard, ImageData};
use clap::{App, Arg};
use deadpool_redis::{Config, Connection, Pool};
use futures_util::stream::StreamExt;
use mimalloc::MiMalloc;
use notify_rust::{Notification, Timeout};
use rand::rngs::OsRng;
use rayon::prelude::*;
use redis::cmd;
use rsa::{PaddingScheme, PublicKey, RsaPrivateKey, RsaPublicKey};
use std::{
    borrow::Cow,
    collections::HashSet,
    error::Error,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    sync::{mpsc, RwLock},
    time,
};

static EXIT: AtomicBool = AtomicBool::new(false);

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> Result<()> {
    ctrlc::set_handler(move || {
        if !EXIT.load(Ordering::Relaxed) {
            EXIT.store(true, Ordering::Relaxed);
        }
    })?;

    let matches = App::new("share-clip")
        .version("0.2.1")
        .author("morning")
        .about("Multi-device clipboard sharing.")
        .arg(
            Arg::with_name("url")
                .short("u")
                .long("url")
                .value_name("S")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name("code")
                .short("c")
                .long("code")
                .value_name("S")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name("name")
                .short("n")
                .long("name")
                .value_name("S")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name("fast")
                .short("f")
                .long("fast")
                .value_name("B")
                .takes_value(true)
                .required(false),
        )
        .get_matches();

    let url = matches.value_of("url").unwrap();
    let code = matches.value_of("code").unwrap();
    let name = matches.value_of("name").unwrap();

    let pool = Arc::new(Config::from_url(url).create_pool()?);
    let pre_md5 = Arc::new(RwLock::new(String::new()));
    let (publish_key, cache_key, match_key) = (
        format!("sub_{}_{}", code, name),
        format!("key:{}:{}", code, name),
        format!("key:{}:*", code),
    );

    let encrypt_key_exists = {
        cmd("EXISTS")
            .arg(cache_key.clone())
            .query_async::<_, i32>(&mut pool.get().await?)
            .await?
    };
    let encrypt_key = {
        let priv_key = if encrypt_key_exists == 0 {
            let priv_key = RsaPrivateKey::new(&mut OsRng, 1024)?;
            cmd("SET")
                .arg(cache_key.clone())
                .arg(bincode::serialize(&priv_key)?)
                .query_async::<_, ()>(&mut pool.get().await?)
                .await?;
            priv_key
        } else {
            let bytes = cmd("GET")
                .arg(cache_key.clone())
                .query_async::<_, Vec<u8>>(&mut pool.get().await?)
                .await?;
            bincode::deserialize::<RsaPrivateKey>(&bytes)?
        };

        RsaPublicKey::from(&priv_key)
    };

    let moniter_future = monitor_clip(pool.clone(), pre_md5.clone(), publish_key, encrypt_key);
    tokio::spawn(async {
        let _ = moniter_future.await;
    });

    let _ = sub_clip(pool.clone(), pre_md5.clone(), cache_key.clone(), match_key).await;
    cmd("DEL")
        .arg(cache_key.clone())
        .query_async::<_, ()>(&mut pool.get().await?)
        .await?;

    Ok(())
}

async fn monitor_clip(
    pool: Arc<Pool>,
    pre_md5: Arc<RwLock<String>>,
    publish_key: String,
    encrypt_key: RsaPublicKey,
) -> Result<()> {
    let mut clip = Clipboard::new()?;

    loop {
        let mut write = pre_md5.write().await;

        let (text, text_md5) = match clip.get_text() {
            Ok(text) => {
                let md5 = format!("{:x}", md5::compute(&text));
                (Some(text), md5)
            }
            _ => (None, "".to_string()),
        };
        let (image, image_md5) = match clip.get_image() {
            Ok(image) => {
                let md5 = format!("{:x}", md5::compute(&image.bytes));
                (Some((image.bytes.to_vec(), image.height, image.width)), md5)
            }
            _ => (None, "".to_string()),
        };

        let (mut data, bytes, md5) = if text.is_some() {
            let data = String::from("0-");
            let bytes = text.unwrap().as_bytes().to_vec();
            (data, bytes, text_md5)
        } else if image.is_some() {
            let (bytes, height, width) = image.unwrap();
            let data = format!("1{},{}-", height, width);
            (data, bytes, image_md5)
        } else {
            time::sleep(Duration::from_secs(1)).await;
            continue;
        };

        if !write.eq(&md5) {
            write.clone_from(&md5);
            write.shrink_to_fit();
            drop(write);

            data.push_str(
                bytes
                    .par_chunks(64)
                    .enumerate()
                    .map(|(index, data)| {
                        if index == 0 {
                            encrypt_key
                                .encrypt(&mut OsRng, PaddingScheme::PKCS1v15Encrypt, data)
                                .unwrap()
                        } else {
                            Vec::from(data)
                        }
                    })
                    .map(|data| base64::encode(data))
                    .collect::<Vec<String>>()
                    .join(",")
                    .as_str(),
            );

            cmd("PUBLISH")
                .arg(&publish_key)
                .arg(data)
                .query_async::<_, ()>(&mut pool.get().await?)
                .await?;
        }

        time::sleep(Duration::from_secs(1)).await;
    }
}

async fn sub_clip(
    pool: Arc<Pool>,
    pre_md5: Arc<RwLock<String>>,
    cache_key: String,
    match_key: String,
) -> Result<()> {
    let (sub_tx, mut sub_rx) = mpsc::channel::<String>(1024);
    let mut subscribed = HashSet::new();

    let pool_clone = pool.clone();
    tokio::spawn(async move {
        loop {
            if let Some(key) = sub_rx.recv().await {
                let dev_sub_future = sub_clip_on_device(pool_clone.clone(), pre_md5.clone(), key);
                tokio::spawn(async {
                    let _ = dev_sub_future.await;
                });
            }
        }
    });

    loop {
        if EXIT.load(Ordering::Relaxed) {
            break;
        }

        let keys = cmd("KEYS")
            .arg(&match_key)
            .query_async::<_, Vec<String>>(&mut pool.get().await?)
            .await?;

        for key in keys {
            if !subscribed.contains(&key) && !key.eq(&cache_key) {
                subscribed.insert(key.clone());
                sub_tx.send(key).await?;
            }
        }

        time::sleep(Duration::from_secs(1)).await;
    }

    Ok(())
}

async fn sub_clip_on_device(
    pool: Arc<Pool>,
    pre_md5: Arc<RwLock<String>>,
    key: String,
) -> Result<(), Box<dyn Error>> {
    let mut clip = Clipboard::new()?;

    let decrypt_key = {
        let bytes = cmd("GET")
            .arg(key.clone())
            .query_async::<_, Vec<u8>>(&mut pool.get().await?)
            .await?;
        bincode::deserialize::<RsaPrivateKey>(&bytes)?
    };
    let (subscribe_key, name) = {
        let key_array = key.split(":").collect::<Vec<&str>>();
        let (code, name) = (*key_array.get(1).unwrap(), *key_array.get(2).unwrap());
        (format!("sub_{}_{}", code, name), name)
    };

    let mut pubsub = Connection::take(pool.get().await?).into_pubsub();
    pubsub.subscribe(subscribe_key).await?;

    loop {
        if let Some(msg) = pubsub.on_message().next().await {
            let payload = msg.get_payload::<String>()?;

            let marker: &i32 = &payload[0..1].parse()?;
            let payload = &payload[1..];

            let mut split = payload.split("-");
            let prop = split.next();
            let data = match split.next() {
                Some(v) => v,
                None => continue,
            };

            let bytes = std::sync::Mutex::new(vec![]);
            data.split(",")
                .collect::<Vec<&str>>()
                .par_iter()
                .enumerate()
                .map(|(index, data)| {
                    let data = base64::decode(data).unwrap();
                    if index == 0 {
                        decrypt_key
                            .decrypt(PaddingScheme::PKCS1v15Encrypt, &data)
                            .unwrap()
                    } else {
                        data
                    }
                })
                .collect::<Vec<Vec<u8>>>()
                .iter()
                .for_each(|data| {
                    data.iter()
                        .for_each(|data| bytes.lock().unwrap().push(*data));
                });

            let mut write = pre_md5.write().await;
            let (md5, body) = match marker {
                0 => {
                    let text = String::from_utf8_lossy(&bytes.lock().unwrap()).to_string();
                    let md5 = format!("{:x}", md5::compute(&text));
                    let body = {
                        let mut body = format!(
                            "{}",
                            &text[0..(if text.len() > 10 { 10 } else { text.len() })]
                        );
                        if text.len() > 10 {
                            body.push_str("...");
                        }
                        body
                    };
                    clip.set_text(text)?;

                    (md5, body)
                }
                1 => {
                    let mut prop_split = prop.unwrap().split(",");
                    let height: usize = prop_split.next().unwrap().parse()?;
                    let width: usize = prop_split.next().unwrap().parse()?;

                    let vec = bytes.lock().unwrap().clone();
                    let image = ImageData {
                        width: width,
                        height: height,
                        bytes: Cow::from(&vec),
                    };
                    clip.set_image(image)?;

                    (format!("{:x}", md5::compute(&vec)), "[图片]".to_string())
                }
                _ => continue,
            };

            Notification::new()
                .summary(&format!("来自的 {} 的剪切板共享", name))
                .body(&body)
                .icon("notification-new-symbolic")
                .timeout(Timeout::Milliseconds(1000 * 5))
                .show()?;

            write.clone_from(&md5);
            write.shrink_to_fit();
        }
    }
}
