#![feature(duration_consts_float)]

mod clipboard;
mod encrypt;

use crate::{
    clipboard::{Clipboard, ClipboardContent, ClipboardContentKinds, Listener},
    encrypt::Alice,
};
use clap::{App, Arg};
use crypto_box::{PublicKey, SecretKey};
use deadpool_redis::{Config, Connection, Pool};
use env_logger::{Builder, Env};
use mimalloc::MiMalloc;
use notify_rust::{Notification, Timeout};
use redis::cmd;
use std::{
    collections::HashMap,
    error::Error,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{
    fs::File,
    io::AsyncReadExt,
    signal,
    task::JoinHandle,
    time::{self, timeout},
};
use tokio_stream::StreamExt;

#[global_allocator]
static GLOBAL_ALLOCATOR: MiMalloc = MiMalloc;

static RUNNING: AtomicBool = AtomicBool::new(true);
static GLOBAL_TIMEOUT: Duration = Duration::from_secs_f64(1.5);

static COMMON_SECRET_KEY: [u8; 32] = [
    89, 58, 40, 58, 231, 88, 28, 80, 165, 110, 86, 42, 196, 176, 182, 77, 144, 187, 183, 189, 108,
    80, 40, 20, 179, 44, 164, 95, 115, 23, 217, 8,
];
static COMMON_PUBLIC_KEY: [u8; 32] = [
    241, 237, 31, 170, 119, 229, 246, 190, 146, 125, 81, 95, 39, 36, 97, 243, 44, 4, 143, 24, 121,
    16, 110, 194, 210, 64, 8, 49, 206, 178, 14, 32,
];

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut builder = Builder::new();
    builder.parse_env(Env::new().filter("LOG")).init();

    let matches = App::new("share-clip")
        .version("0.3.7")
        .author("morning")
        .about("Multi-device clipboard sharing.")
        .arg(
            Arg::new("url")
                .short('u')
                .long("url")
                .value_name("value")
                .takes_value(true)
                .required_unless_present("gen-key"),
        )
        .arg(
            Arg::new("code")
                .short('c')
                .long("code")
                .value_name("value")
                .takes_value(true)
                .required_unless_present("gen-key"),
        )
        .arg(
            Arg::new("name")
                .short('n')
                .long("name")
                .value_name("value")
                .takes_value(true)
                .required_unless_present("gen-key"),
        )
        .arg(
            Arg::new("confirm")
                .short('C')
                .long("confirm")
                .value_name("bool")
                .takes_value(true),
        )
        .arg(
            Arg::new("secret-key")
                .long("secret-key")
                .value_name("file")
                .takes_value(true)
                .requires("public-key"),
        )
        .arg(
            Arg::new("public-key")
                .long("public-key")
                .value_name("file")
                .takes_value(true)
                .requires("secret-key"),
        )
        .arg(
            Arg::new("gen-key")
                .long("gen-key")
                .help("Generate key pairs"),
        )
        .get_matches();

    if matches.is_present("gen-key") {
        return encrypt::gen_key().await;
    }

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

    let pool = Arc::new(Config::from_url(url).create_pool(None)?);

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

    let clipboard = Arc::new(Clipboard::new());

    let publisher = {
        let alice = alice.clone();
        let pool = pool.clone();
        let clipboard = clipboard.clone();
        let key = format!("sub_{}_{}", code, name);

        tokio::spawn(async move {
            if let Err(e) = publisher(alice, pool, clipboard, key).await {
                panic!("{:#}", e);
            }
        })
    };

    let subscriber = {
        let alice = alice.clone();
        let pool = pool.clone();
        let clipboard = clipboard.clone();
        let match_key = format!("key:{}:*", code);
        let cache_key = format!("key:{}:{}", code, name);

        tokio::spawn(async move {
            if let Err(e) = subscriber(alice, pool, clipboard, match_key, cache_key, confirm).await
            {
                panic!("{:#}", e);
            }
        })
    };

    signal::ctrl_c().await?;

    RUNNING.store(false, Ordering::SeqCst);

    publisher.await?;
    subscriber.await?;

    for cache_key in cmd("KEYS")
        .arg(format!("key:{}:{}:*", code, name))
        .query_async::<_, Vec<String>>(&mut pool.get().await?)
        .await?
    {
        cmd("DEL")
            .arg(cache_key)
            .query_async(&mut pool.get().await?)
            .await?;
    }

    Ok(())
}

async fn publisher(
    alice: Arc<Alice>,
    pool: Arc<Pool>,
    clipboard: Arc<Clipboard>,
    key: String,
) -> Result<(), Box<dyn Error>> {
    let listener = Listener::new(clipboard);

    while RUNNING.load(Ordering::SeqCst) {
        if let Ok(Ok(content)) = timeout(GLOBAL_TIMEOUT, listener.recv()).await {
            let content = alice.encrypt(content).await;
            let binary = bincode::serialize(&content)?;

            let mut conn = pool.get().await?;
            cmd("PUBLISH")
                .arg(&key)
                .arg(binary)
                .query_async::<_, ()>(&mut conn)
                .await?;
        }
    }

    Ok(())
}

async fn subscriber(
    alice: Arc<Alice>,
    pool: Arc<Pool>,
    clipboard: Arc<Clipboard>,
    match_key: String,
    cache_key: String,
    confirm: bool,
) -> Result<(), Box<dyn Error>> {
    let mut device_futures: HashMap<String, JoinHandle<()>> = HashMap::new();

    while RUNNING.load(Ordering::SeqCst) {
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

            if device_futures.contains_key(&key) || key.eq(&cache_key) {
                continue;
            }

            let clipboard = clipboard.clone();
            let pool = pool.clone();
            let alice = alice.clone();
            device_futures.insert(
                key.clone(),
                tokio::spawn(async move {
                    let _ = on_device(clipboard, pool, alice, key, confirm).await;
                }),
            );
        }

        time::sleep(GLOBAL_TIMEOUT).await;
    }

    for device_future in device_futures.values_mut() {
        if let Err(e) = device_future.await {
            eprintln!("{:#}", e);
        }
    }

    Ok(())
}

async fn on_device(
    clipboard: Arc<Clipboard>,
    pool: Arc<Pool>,
    alice: Arc<Alice>,
    key: String,
    confirm: bool,
) -> Result<(), Box<dyn Error>> {
    let (subscribe_key, name) = {
        let key_array = key.split(':').collect::<Vec<&str>>();
        let (code, name) = (*key_array.get(1).unwrap(), *key_array.get(2).unwrap());
        (format!("sub_{}_{}", code, name), name)
    };

    let mut pubsub = Connection::take(pool.get().await?).into_pubsub();
    pubsub.subscribe(subscribe_key).await?;
    let mut message = pubsub.on_message();

    while RUNNING.load(Ordering::SeqCst) {
        if let Ok(Some(msg)) = timeout(GLOBAL_TIMEOUT, message.next()).await {
            let binary = msg.get_payload::<Vec<u8>>()?;

            let content = bincode::deserialize::<ClipboardContent>(&binary)?;
            let content = alice.decrypt(pool.get().await?, &key, content).await?;

            let summary = format!("Clipboard sharing from {}", name);
            let body = match content.kinds {
                ClipboardContentKinds::TEXT => "[TEXT]",
                ClipboardContentKinds::IMAGE => "[IMAGE]",
                ClipboardContentKinds::NONE => "[NONE]",
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
                    .wait_for_action(|action| {
                        if action == "accept" {
                            if clipboard.set(content).is_ok() {
                                Notification::new()
                                    .summary("Accept successfully")
                                    .auto_icon()
                                    .timeout(Timeout::Milliseconds(1000))
                                    .show()
                                    .expect("Notification of failure to send!");
                            };
                        }
                    });
            } else if clipboard.set(content).is_ok() {
                notify.timeout(Timeout::Milliseconds(1000 * 5)).show()?;
            }

            #[cfg(any(target_os = "macos", target_os = "windows"))]
            if clipboard.set(content).is_ok() {
                notify.timeout(Timeout::Milliseconds(1000 * 5)).show()?;
            }
        }
    }

    Ok(())
}
