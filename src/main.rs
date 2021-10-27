mod clip;
mod encrypt;

use crate::{
    clip::{Clip, ClipContext, ClipHandle},
    encrypt::Alice,
};
use clap::{App, Arg};
use clipboard_master::Master;
use crypto_box::{PublicKey, SecretKey};
use deadpool_redis::{Config, Connection, Pool};
#[cfg(target_os = "windows")]
use mimalloc::MiMalloc;
use notify_rust::{Notification, Timeout};
use redis::cmd;
#[cfg(any(target_os = "macos", target_os = "linux"))]
use snmalloc_rs::SnMalloc;
use std::{
    collections::HashMap,
    error::Error,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use tokio::{
    fs::File,
    io::AsyncReadExt,
    time::{self, timeout},
};
use tokio_stream::StreamExt;

#[cfg(target_os = "windows")]
#[global_allocator]
static GLOBAL_ALLOCATOR: MiMalloc = MiMalloc;

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[global_allocator]
static GLOBAL_ALLOCATOR: SnMalloc = SnMalloc;

pub static COMMON_SECRET_KEY: [u8; 32] = [
    89, 58, 40, 58, 231, 88, 28, 80, 165, 110, 86, 42, 196, 176, 182, 77, 144, 187, 183, 189, 108,
    80, 40, 20, 179, 44, 164, 95, 115, 23, 217, 8,
];
pub static COMMON_PUBLIC_KEY: [u8; 32] = [
    241, 237, 31, 170, 119, 229, 246, 190, 146, 125, 81, 95, 39, 36, 97, 243, 44, 4, 143, 24, 121,
    16, 110, 194, 210, 64, 8, 49, 206, 178, 14, 32,
];

static RUNNING: AtomicBool = AtomicBool::new(true);
static GLOBAL_TIMEOUT: f64 = 2.5;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let matches = App::new("share-clip")
        .version("0.3.3")
        .author("morning")
        .about("Multi-device clipboard sharing.")
        .arg(
            Arg::with_name("url")
                .short("u")
                .long("url")
                .value_name("url")
                .takes_value(true)
                .required_unless("gen-key"),
        )
        .arg(
            Arg::with_name("code")
                .short("c")
                .long("code")
                .value_name("value")
                .takes_value(true)
                .required_unless("gen-key"),
        )
        .arg(
            Arg::with_name("name")
                .short("n")
                .long("name")
                .value_name("value")
                .takes_value(true)
                .required_unless("gen-key"),
        )
        .arg(
            Arg::with_name("confirm")
                .short("C")
                .long("confirm")
                .value_name("bool")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("secret-key")
                .long("secret-key")
                .value_name("file")
                .takes_value(true)
                .requires("public-key"),
        )
        .arg(
            Arg::with_name("public-key")
                .long("public-key")
                .value_name("file")
                .takes_value(true)
                .requires("secret-key"),
        )
        .arg(
            Arg::with_name("gen-key")
                .long("gen-key")
                .help("Generate key pairs"),
        )
        .get_matches();

    if matches.is_present("gen-key") {
        encrypt::gen_key().await?;
        return Ok(());
    };

    let url = match matches.value_of("url") {
        Some(v) => v.to_string(),
        None => panic!("Requires redis url!"),
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

    // Initialization redis
    let pool = Arc::new(Config::from_url(url).create_pool()?);
    // Initialization clipboard
    let (clip, mut rx) = Clip::new().await;
    // Initialization alice
    let secret_key = match matches.value_of("secret-key") {
        Some(v) => {
            let mut file = File::open(v).await?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer).await?;
            bincode::deserialize::<[u8; 32]>(&buffer)?
        }
        None => COMMON_SECRET_KEY,
    };
    let public_key = match matches.value_of("public-key") {
        Some(v) => {
            let mut file = File::open(v).await?;
            let mut buffer = Vec::new();
            file.read_to_end(&mut buffer).await?;
            bincode::deserialize::<[u8; 32]>(&buffer)?
        }
        None => COMMON_PUBLIC_KEY,
    };
    let alice = Arc::new(
        Alice::new(
            pool.get().await?,
            format!("key:{}:{}", code, name),
            SecretKey::from(secret_key),
            PublicKey::from(public_key),
        )
        .await,
    );

    let wait = Duration::from_secs_f64(GLOBAL_TIMEOUT);

    // Run publisher
    let publisher = {
        let (alice_clone, pool_clone, publish_key) = (
            alice.clone(),
            pool.clone(),
            format!("sub_{}_{}", code, name),
        );

        let publisher = async move {
            while RUNNING.load(Ordering::SeqCst) {
                if let Ok(Some(clip_context)) = timeout(wait, rx.recv()).await {
                    let clip_context = alice_clone.encrypt(clip_context).await;
                    let binary = bincode::serialize(&clip_context).expect("Serialization failure!");

                    let mut conn = pool_clone.get().await.expect("Failed to get connection!");
                    cmd("PUBLISH")
                        .arg(&publish_key)
                        .arg(binary)
                        .query_async::<_, ()>(&mut conn)
                        .await
                        .expect("redis execution failed!");
                }
            }
        };

        tokio::spawn(publisher)
    };

    // Run subscriber
    let subscriber = {
        let (clip_clone, match_key, cache_key) = (
            clip.clone(),
            format!("key:{}:*", code),
            format!("key:{}:{}", code, name),
        );

        let subscriber = async move {
            let mut device_futures = HashMap::new();

            while RUNNING.load(Ordering::SeqCst) {
                let keys = cmd("KEYS")
                    .arg(&match_key)
                    .query_async::<_, Vec<String>>(
                        &mut pool.get().await.expect("Failed to get connection!"),
                    )
                    .await
                    .expect("redis execution failed!");

                for key in keys {
                    let rev_key = key.chars().rev().collect::<String>();
                    let key = rev_key[rev_key.find(':').unwrap() + 1..]
                        .chars()
                        .rev()
                        .collect::<String>();

                    if device_futures.contains_key(&key) || key.eq(&cache_key) {
                        continue;
                    }

                    let (clip_clone, pool_clone, alice_clone) =
                        (clip_clone.clone(), pool.clone(), alice.clone());
                    device_futures.insert(
                        key.clone(),
                        tokio::spawn(async move {
                            let _ =
                                on_device(clip_clone, pool_clone, alice_clone, key, confirm).await;
                        }),
                    );
                }

                time::sleep(wait).await;
            }

            for f in device_futures.values_mut() {
                let _ = f.await;
            }
        };

        tokio::spawn(subscriber)
    };

    // Run Listener
    thread::spawn(|| {
        let _ = Master::new(ClipHandle { clip }).run();
    });

    tokio::signal::ctrl_c().await.unwrap();
    RUNNING.store(false, Ordering::SeqCst);

    // Wait for resource release
    publisher.await?;
    subscriber.await?;

    Ok(())
}

async fn on_device(
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
    let mut message = pubsub.on_message();

    let wait = Duration::from_secs_f64(GLOBAL_TIMEOUT);

    while RUNNING.load(Ordering::SeqCst) {
        if let Ok(Some(msg)) = timeout(wait, message.next()).await {
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
                    .action("accept", "Accept")
                    .action("reject", "Reject")
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

            #[cfg(any(target_os = "macos", target_os = "windows"))]
            match clip.set_clip(clip_content).await {
                Ok(_) => {
                    notify.timeout(Timeout::Milliseconds(1000 * 5)).show()?;
                }
                _ => {}
            };
        }
    }

    Ok(())
}
