share-clip
======

### 多设备剪切板共享

<img src="share-clip.gif" />

特性
------
* 采用 Rust 实现。便捷/高性能
* 基于 Redis（可轻松支持广域网/局域网的共享）
* 自动发现同身份下的机器
* ~~全数据 RSA 分段加密~~（出于性能及其他原因考虑替换为 XSalsa20Poly1305 加密）
* 支持文本和图片
* 支持共享提示（macOS 不支持），询问框确认共享（仅支持 Linux）

安装
------
#### 二进制文件
<https://github.com/cc-morning/share-clip/releases>

#### 源码编译
```bash
git clone https://github.com/cc-morning/share-clip.git
cd share-clip
cargo build --release
```

快速上手
------

#### 1.安装 Redis
```bash
docker run --name redis -d -p 6379:6379 redis
```
#### 2.运行客户端

Windows 电脑:

```bash
share-clip -u redis://:@127.0.0.1 -c cc-morning -n windows
```

Linux 电脑:

```bash
share-clip -u redis://:@127.0.0.1 -c cc-morning -n linux
```
