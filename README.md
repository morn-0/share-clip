share-clip
======

### 多设备剪切板共享

<img src="share-clip.gif" />

特性
------
* 基于 Redis (可轻松支持广域网/局域网的共享, 自动发现同身份下的机器), 便携/高性能
* 全数据加密 (加密算法采用 XSalsa20Poly1305)
* 支持 Linux (X11/Wayland) macOS, Windows 平台, (Android 开发中, iOS 暂无开发计划)
* 支持文本和图片
* 支持共享提示 (macOS 不支持), 询问框确认共享 (仅支持 Linux)

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
