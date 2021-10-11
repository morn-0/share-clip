use crate::clip::ClipContext;
use crypto_box::{
    aead::{
        generic_array::{
            typenum::{
                bit::{B0, B1},
                UInt, UTerm,
            },
            GenericArray,
        },
        Aead,
    },
    rand_core::OsRng,
    PublicKey, SalsaBox, SecretKey,
};
use deadpool_redis::Connection;
use redis::cmd;
use tokio::{fs::File, io::AsyncWriteExt};

pub struct Alice {
    encrypt: SalsaBox,
    nonce: GenericArray<u8, UInt<UInt<UInt<UInt<UInt<UTerm, B1>, B1>, B0>, B0>, B0>>,
    comm_secret_key: SecretKey,
}

impl Alice {
    pub async fn new(
        mut conn: Connection,
        device_key: String,
        comm_secret_key: SecretKey,
        comm_public_key: PublicKey,
    ) -> Self {
        let (priv_secret_key, priv_public_key) = {
            let secret_key = SecretKey::generate(&mut OsRng);
            let public_key = secret_key.public_key();
            (secret_key, public_key)
        };
        let nonce = crypto_box::generate_nonce(&mut OsRng);

        cmd("SET")
            .arg({
                let mut key_pre = device_key.clone();
                key_pre.push_str(":public");
                key_pre
            })
            .arg(bincode::serialize(&priv_public_key.as_bytes()).expect("Serialization failure!"))
            .query_async::<_, ()>(&mut conn)
            .await
            .expect("redis execution failed!");
        cmd("SET")
            .arg({
                let mut key_pre = device_key.clone();
                key_pre.push_str(":nonce");
                key_pre
            })
            .arg(bincode::serialize(&nonce.as_slice()).expect("Serialization failure!"))
            .query_async::<_, ()>(&mut conn)
            .await
            .expect("redis execution failed!");

        let encrypt = SalsaBox::new(&comm_public_key, &priv_secret_key);
        Alice {
            encrypt,
            nonce,
            comm_secret_key,
        }
    }

    pub async fn encrypt(&self, mut clip: ClipContext) -> ClipContext {
        clip.bytes = self
            .encrypt
            .encrypt(&self.nonce, &clip.bytes[..])
            .expect("Encryption failure!");

        clip
    }

    pub async fn decrypt(
        &self,
        mut conn: Connection,
        key: &String,
        mut clip: ClipContext,
    ) -> anyhow::Result<ClipContext> {
        let data = cmd("GET")
            .arg({
                let mut key = key.clone();
                key.push_str(":public");
                key
            })
            .query_async::<_, Vec<u8>>(&mut conn)
            .await?;
        let priv_public_key = PublicKey::from(bincode::deserialize::<[u8; 32]>(&data)?);

        let data = cmd("GET")
            .arg({
                let mut key = key.clone();
                key.push_str(":nonce");
                key
            })
            .query_async::<_, Vec<u8>>(&mut conn)
            .await?;
        let data = bincode::deserialize::<Vec<u8>>(&data)?;
        let nonce =
            GenericArray::<u8, UInt<UInt<UInt<UInt<UInt<UTerm, B1>, B1>, B0>, B0>, B0>>::from_slice(
                data.as_slice(),
            );

        let decrypt = SalsaBox::new(&priv_public_key, &self.comm_secret_key);
        clip.bytes = decrypt
            .decrypt(nonce, &clip.bytes[..])
            .expect("Decryption failure!");

        Ok(clip)
    }
}

pub async fn gen_key() -> anyhow::Result<()> {
    let (secret_key, public_key) = {
        let secret_key = SecretKey::generate(&mut OsRng);
        let public_key = secret_key.public_key();
        (secret_key, public_key)
    };

    let mut secret_file = File::create("secret_key").await?;
    secret_file.write_all(&secret_key.to_bytes()).await?;

    let mut public_file = File::create("public_key").await?;
    public_file.write_all(public_key.as_bytes()).await?;

    Ok(())
}
