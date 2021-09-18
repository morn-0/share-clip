use anyhow::Result;
use clap::{App, Arg};
use clipboard::{ClipboardContext, ClipboardProvider};
use deadpool_redis::{Config, Connection, Pool};
use futures_util::stream::StreamExt;
use lazy_static::lazy_static;
use mimalloc::MiMalloc;
use rand::rngs::OsRng;
use rayon::prelude::*;
use redis::cmd;
use rsa::{PaddingScheme, PublicKey, RsaPrivateKey, RsaPublicKey};
use std::{
    collections::HashSet,
    error::Error,
    sync::{self, Arc, Mutex},
    time::Duration,
};
use tokio::{
    sync::{
        mpsc::{self, Sender},
        RwLock,
    },
    time,
};

lazy_static! {
    static ref EXIT: sync::RwLock<i32> = sync::RwLock::new(0);
}

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[tokio::main]
async fn main() -> Result<()> {
    let matches = App::new("share-clip")
        .version("1.0")
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
        .get_matches();

    ctrlc::set_handler(move || {
        if *(EXIT.read().unwrap()) == 0 {
            *(EXIT.write().unwrap()) = 1;
        }
    })?;

    let url = matches.value_of("url").unwrap_or_default();
    let pool = Arc::new(Config::from_url(url).create_pool()?);

    let code = matches.value_of("code").unwrap_or_default();
    let name = matches.value_of("name").unwrap_or_default();

    let priv_cache_key = format!("key:{}:{}", code, name);
    let priv_key_exists = {
        cmd("EXISTS")
            .arg(priv_cache_key.clone())
            .query_async::<_, i32>(&mut pool.get().await?)
            .await?
    };

    let pub_key = {
        let priv_key = if priv_key_exists == 0 {
            let priv_key = RsaPrivateKey::new(&mut OsRng, 2048)?;
            cmd("SET")
                .arg(priv_cache_key.clone())
                .arg(bincode::serialize(&priv_key)?)
                .query_async::<_, ()>(&mut pool.get().await?)
                .await?;
            priv_key
        } else {
            let bytes = cmd("GET")
                .arg(priv_cache_key.clone())
                .query_async::<_, Vec<u8>>(&mut pool.get().await?)
                .await?;
            bincode::deserialize::<RsaPrivateKey>(&bytes)?
        };

        RsaPublicKey::from(&priv_key)
    };

    let (clip_tx, mut clip_rx) = mpsc::channel::<String>(1024);
    let pre_text = Arc::new(RwLock::new(String::new()));

    let pre_text_clone = pre_text.clone();
    tokio::spawn(async move {
        let _ = monitor_clip(pre_text_clone, clip_tx).await;
    });

    let publish_key = format!("sub_{}_{}", code, name);
    let pool_clone = pool.clone();
    tokio::spawn(async move {
        loop {
            if let Some(i) = clip_rx.recv().await {
                let bytes = i
                    .as_bytes()
                    .par_chunks(96)
                    .filter_map(|data| {
                        pub_key
                            .encrypt(&mut OsRng, PaddingScheme::PKCS1v15Encrypt, data)
                            .ok()
                    })
                    .map(|data| base64::encode(data))
                    .collect::<Vec<String>>()
                    .join(",");

                cmd("PUBLISH")
                    .arg(&publish_key)
                    .arg(bytes)
                    .query_async::<_, ()>(&mut pool_clone.get().await.unwrap())
                    .await
                    .unwrap();
            }
        }
    });

    let _ = sub_clip(
        pool.clone(),
        pre_text.clone(),
        priv_cache_key.clone(),
        format!("key:{}:*", code),
    )
    .await;

    cmd("DEL")
        .arg(priv_cache_key.clone())
        .query_async::<_, ()>(&mut pool.get().await?)
        .await?;

    Ok(())
}

async fn monitor_clip(pre_text: Arc<RwLock<String>>, sender: Sender<String>) -> Result<()> {
    let mut clip_ctx: ClipboardContext = ClipboardProvider::new().expect("创建剪切板对象失败!");

    loop {
        let content = {
            let _write = pre_text.write().await;
            let content = match clip_ctx.get_contents() {
                Ok(v) => v,
                Err(_) => continue,
            };
            content
        };

        if !pre_text.read().await.eq(&content) {
            match sender.send(content.clone()).await {
                Ok(_) => {
                    let mut write = pre_text.write().await;
                    write.clear();
                    write.push_str(&content);
                }
                Err(_) => {}
            }
        }

        time::sleep(Duration::from_secs(1)).await;
    }
}

async fn sub_clip(
    pool: Arc<Pool>,
    pre_text: Arc<RwLock<String>>,
    priv_cache_key: String,
    all_code_key: String,
) -> Result<()> {
    let (sub_tx, mut sub_rx) = mpsc::channel::<String>(1024);
    let mut subed = HashSet::new();

    let pool_clone = pool.clone();
    tokio::spawn(async move {
        loop {
            let pool_clone = pool_clone.clone();
            let pre_text_clone = pre_text.clone();
            if let Some(key) = sub_rx.recv().await {
                tokio::spawn(async move {
                    let _ = dev_sub_clip(pool_clone, key, pre_text_clone).await;
                });
            }
        }
    });

    loop {
        if *(EXIT.read().unwrap()) == 1 {
            break;
        }

        let keys = cmd("KEYS")
            .arg(&all_code_key)
            .query_async::<_, Vec<String>>(&mut pool.get().await?)
            .await?;
        for key in keys {
            if !subed.contains(&key) && !key.eq(&priv_cache_key) {
                subed.insert(key.clone());
                sub_tx.send(key).await?;
            }
        }

        time::sleep(Duration::from_secs(1)).await;
    }

    Ok(())
}

async fn dev_sub_clip(
    pool: Arc<Pool>,
    key: String,
    pre_text: Arc<RwLock<String>>,
) -> Result<(), Box<dyn Error>> {
    let mut clip_ctx: ClipboardContext = ClipboardProvider::new()?;

    let priv_key = {
        let bytes = cmd("GET")
            .arg(key.clone())
            .query_async::<_, Vec<u8>>(&mut pool.get().await?)
            .await?;
        bincode::deserialize::<RsaPrivateKey>(&bytes)?
    };
    let (subscribe_key, code, name) = {
        let key_array = key.split(":").collect::<Vec<&str>>();
        let (code, name) = (*key_array.get(1).unwrap(), *key_array.get(2).unwrap());
        (format!("sub_{}_{}", code, name), code, name)
    };

    let mut pubsub = Connection::take(pool.get().await?).into_pubsub();
    pubsub.subscribe(subscribe_key).await?;
    println!("Sub -> {}-{}", code, name);

    loop {
        if let Some(msg) = pubsub.on_message().next().await {
            let payload = msg.get_payload::<String>()?;

            let bytes = Mutex::new(vec![]);
            payload
                .split(",")
                .par_bridge()
                .map(|cipher| {
                    let data = base64::decode(cipher).unwrap();
                    let data = priv_key
                        .decrypt(PaddingScheme::PKCS1v15Encrypt, &data)
                        .unwrap();
                    data
                })
                .for_each(|data| {
                    data.iter().for_each(|i| bytes.lock().unwrap().push(*i));
                });
            let text = String::from_utf8_lossy(&bytes.lock().unwrap()).to_string();

            let mut write = pre_text.write().await;
            write.clear();
            write.push_str(text.as_str());
            clip_ctx.set_contents(text)?;
        }
    }
}

