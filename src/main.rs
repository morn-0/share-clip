mod clip;
mod encrypt;

use crate::{
    clip::{Clip, ClipContext},
    encrypt::Alice,
};
use anyhow::Result;
use clap::{App, Arg};
use crypto_box::{PublicKey, SecretKey};
use deadpool_redis::{Config, Connection, Pool};
use futures_util::stream::StreamExt;
use mimalloc::MiMalloc;
use notify_rust::{Notification, Timeout};
use redis::cmd;
use std::{collections::HashSet, error::Error, sync::Arc, time::Duration};
use tokio::{sync::mpsc, time};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

pub static COMMON_SECRET_KEY: [u8; 32] = [
    89, 58, 40, 58, 231, 88, 28, 80, 165, 110, 86, 42, 196, 176, 182, 77, 144, 187, 183, 189, 108,
    80, 40, 20, 179, 44, 164, 95, 115, 23, 217, 8,
];
pub static COMMON_PUBLIC_KEY: [u8; 32] = [
    241, 237, 31, 170, 119, 229, 246, 190, 146, 125, 81, 95, 39, 36, 97, 243, 44, 4, 143, 24, 121,
    16, 110, 194, 210, 64, 8, 49, 206, 178, 14, 32,
];

#[tokio::main]
async fn main() -> Result<()> {
    let common_secret_key = SecretKey::from(COMMON_SECRET_KEY);
    let common_public_key = PublicKey::from(COMMON_PUBLIC_KEY);

    let matches = App::new("share-clip")
        .version("0.3.0")
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
            Arg::with_name("confirm")
                .short("C")
                .long("confirm")
                .value_name("S")
                .takes_value(true)
                .required(false),
        )
        .get_matches();

    let url = match matches.value_of("url") {
        Some(v) => v.to_string(),
        None => panic!("Requires redis link!"),
    };
    let code = match matches.value_of("code") {
        Some(v) => v.to_string(),
        None => panic!("Unique code required!"),
    };
    let name = match matches.value_of("name") {
        Some(v) => v.to_string(),
        None => panic!("Unique name required!"),
    };
    let confirm = matches
        .value_of("confirm")
        .map(|c| c.parse::<bool>().unwrap())
        .unwrap_or(false);

    let pool = Arc::new(Config::from_url(url).create_pool()?);
    let (clip, tx, mut rx) = Clip::new().await;
    let alice = Arc::new(
        Alice::new(
            pool.get().await?,
            format!("key:{}:{}", code, name),
            common_secret_key,
            common_public_key,
        )
        .await,
    );

    let (code_clone, name_clone) = (code.to_string(), name.to_string());
    let (alice_clone, pool_clone) = (alice.clone(), pool.clone());
    tokio::spawn(async move {
        let publish_key = format!("sub_{}_{}", code_clone, name_clone);

        while let Some(clip_context) = rx.recv().await {
            let clip_context = alice_clone.encrypt(clip_context).await;
            let binary = bincode::serialize(&clip_context).expect("Serialization failure!");

            cmd("PUBLISH")
                .arg(&publish_key)
                .arg(binary)
                .query_async::<_, ()>(
                    &mut pool_clone.get().await.expect("Failed to get connection!"),
                )
                .await
                .expect("redis execution failed!");
        }
    });

    let clip_clone = clip.clone();
    let (alice_clone, pool_clone) = (alice.clone(), pool.clone());
    tokio::spawn(async move {
        let _ = sub_clip(clip_clone, pool_clone, alice_clone, name, code, confirm).await;
    });

    clip.listen(tx).await;
    Ok(())
}

async fn sub_clip(
    clip: Arc<Clip>,
    pool: Arc<Pool>,
    alice: Arc<Alice>,
    name: String,
    code: String,
    confirm: bool,
) -> Result<()> {
    let mut subscribed = HashSet::new();
    let (sub_tx, mut sub_rx) = mpsc::channel::<String>(1024);
    let (match_key, cache_key) = (format!("key:{}:*", code), format!("key:{}:{}", code, name));

    let clip_clone = clip.clone();
    let (alice_clone, pool_clone) = (alice.clone(), pool.clone());
    tokio::spawn(async move {
        loop {
            if let Some(key) = sub_rx.recv().await {
                let dev_sub_future = sub_clip_on_device(
                    clip_clone.clone(),
                    pool_clone.clone(),
                    alice_clone.clone(),
                    key,
                    confirm,
                );
                tokio::spawn(async {
                    let _ = dev_sub_future.await;
                });
            }
        }
    });

    loop {
        let keys = cmd("KEYS")
            .arg(&match_key)
            .query_async::<_, Vec<String>>(&mut pool.get().await?)
            .await?;

        for key in keys {
            let rev_key = key.chars().rev().collect::<String>();
            let key = rev_key[rev_key.find(':').unwrap() + 1..]
                .chars()
                .rev()
                .collect::<String>();

            if !subscribed.contains(&key) && !key.eq(&cache_key) {
                subscribed.insert(key.clone());
                sub_tx.send(key).await?;
            }
        }

        time::sleep(Duration::from_secs_f64(1.5)).await;
    }
}

async fn sub_clip_on_device(
    clip: Arc<Clip>,
    pool: Arc<Pool>,
    alice: Arc<Alice>,
    key: String,
    confirm: bool,
) -> Result<(), Box<dyn Error>> {
    let (subscribe_key, name) = {
        let key_array = key.split(":").collect::<Vec<&str>>();
        let (code, name) = (*key_array.get(1).unwrap(), *key_array.get(2).unwrap());
        (format!("sub_{}_{}", code, name), name)
    };

    let mut pubsub = Connection::take(pool.get().await?).into_pubsub();
    pubsub.subscribe(subscribe_key).await?;

    loop {
        if let Some(msg) = pubsub.on_message().next().await {
            let binary = msg.get_payload::<Vec<u8>>()?;

            let clip_content = bincode::deserialize::<ClipContext>(&binary)?;
            let clip_content = alice.decrypt(pool.get().await?, &key, clip_content).await?;

            let summary = format!("Clipboard sharing from {}", name);
            let body = match clip_content.kinds {
                clip::ClipContextKinds::TEXT => "[TEXT]",
                clip::ClipContextKinds::IMAGE => "[IMAGE]",
                clip::ClipContextKinds::NONE => "[NONE]",
            };

            let mut notify = Notification::new();
            notify.summary(&summary).body(body).auto_icon();

            #[cfg(target_os = "linux")]
            if confirm {
                notify
                    .timeout(Timeout::Milliseconds(1000 * 30))
                    .action("accept", "Accept") // IDENTIFIER, LABEL
                    .action("reject", "Reject") // IDENTIFIER, LABEL
                    .show()?
                    .wait_for_action(|action| match action {
                        "accept" => {
                            let clip_clone = clip.clone();
                            tokio::spawn(async move {
                                if let Ok(_) = clip_clone.set_clip(clip_content).await {
                                    Notification::new()
                                        .summary("Accept successfully")
                                        .auto_icon()
                                        .timeout(Timeout::Milliseconds(1000 * 1))
                                        .show()
                                        .expect("Notification of failure to send!");
                                };
                            });
                        }
                        _ => (),
                    });
            } else {
                match clip.set_clip(clip_content).await {
                    Ok(_) => {
                        notify.timeout(Timeout::Milliseconds(1000 * 5)).show()?;
                    }
                    _ => {}
                };
            }

            #[cfg(not(target_os = "linux"))]
            match clip.set_clip(clip_content).await {
                Ok(_) => {
                    notify.timeout(Timeout::Milliseconds(1000 * 5)).show()?;
                }
                _ => {}
            };
        }
    }
}
