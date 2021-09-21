pub mod encrypt;

use crate::clip::ClipContext;
use async_trait::async_trait;
use deadpool_redis::Connection;

#[async_trait]
pub trait Plug {
    async fn new(conn: Connection, code: String, name: String) -> Self;
    async fn wrap(&mut self, clip: ClipContext) -> ClipContext;
}
