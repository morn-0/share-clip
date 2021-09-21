use crate::{clip::ClipContext, plug::Plug};
use async_trait::async_trait;
use deadpool_redis::Connection;
use rand::rngs::OsRng;
use rayon::{
    iter::{IntoParallelRefIterator, ParallelIterator},
    slice::ParallelSlice,
};
use redis::cmd;
use rsa::{PaddingScheme, PublicKey, RsaPrivateKey, RsaPublicKey};

pub struct Encrypt {
    _priv_key: RsaPrivateKey,
    pub_key: RsaPublicKey,
}

#[async_trait]
impl Plug for Encrypt {
    async fn new(mut conn: Connection, code: String, name: String) -> Self {
        let cache_key = format!("key:{}:{}", code, name);

        let exists = cmd("EXISTS")
            .arg(&cache_key)
            .query_async::<_, i32>(&mut conn)
            .await
            .expect("redis execution failed!");

        let (_priv_key, pub_key) = {
            let priv_key = if exists == 0 {
                let priv_key = RsaPrivateKey::new(&mut OsRng, 1024).expect("Failed to create key!");
                cmd("SET")
                    .arg(cache_key.clone())
                    .arg(bincode::serialize(&priv_key).expect("Serialization failure!"))
                    .query_async::<_, ()>(&mut conn)
                    .await
                    .expect("redis execution failed!");
                priv_key
            } else {
                let bytes = cmd("GET")
                    .arg(cache_key.clone())
                    .query_async::<_, Vec<u8>>(&mut conn)
                    .await
                    .expect("redis execution failed!");
                bincode::deserialize::<RsaPrivateKey>(&bytes).expect("Deserialization failure!")
            };
            let pub_key = RsaPublicKey::from(&priv_key);

            (priv_key, pub_key)
        };

        Encrypt { _priv_key, pub_key }
    }

    async fn wrap(&mut self, mut clip: ClipContext) -> ClipContext {
        let data = clip
            .bytes
            .par_chunks(64)
            .map(|data| {
                self.pub_key
                    .encrypt(&mut OsRng, PaddingScheme::PKCS1v15Encrypt, data)
                    .expect("Encryption failure")
            })
            .map(|data| base64::encode(data))
            .collect::<Vec<String>>()
            .join(",");

        clip.bytes = data.as_bytes().to_vec();
        clip
    }
}

impl Encrypt {
    pub async fn dewrap(mut clip: ClipContext, priv_key: &RsaPrivateKey) -> ClipContext {
        let mut bytes = vec![];
        let data = String::from_utf8(clip.bytes).unwrap();

        let data = data
            .split(",")
            .collect::<Vec<&str>>()
            .par_iter()
            .map(|data| base64::decode(data).expect("base64 decryption failure!"))
            .map(|data| {
                priv_key
                    .decrypt(PaddingScheme::PKCS1v15Encrypt, &data)
                    .expect("Encryption failure!")
            })
            .collect::<Vec<Vec<u8>>>();
        for i in data {
            for j in i {
                bytes.push(j);
            }
        }

        clip.bytes = bytes;
        clip
    }
}
