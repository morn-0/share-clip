share-clip
======

### 多设备剪切板共享

<img src="share-clip.gif" />

特性
------
* 基于 Redis (可轻松支持广域网/局域网的共享, 自动发现同身份下的机器), 便携/高性能
* 全数据加密 (加密算法采用 XSalsa20Poly1305), 支持自定义密钥
* 支持 Linux (X11/Wayland), macOS, Windows 平台, (Android 开发中, iOS 暂无开发计划)
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

使用自定义密钥
-----------

默认使用程序内置的公共密钥对(该方式同样安全 [^1], 但是可信的 redis 是前提), 若在公共或不可信的 redis 中建议使用自定义密钥

### 1.构建密钥对
```bash
share-clip --gen-key
```
该命令会在当前目录下生成 secret_key 和 public_key 密钥对文件(同 code 下的机器使用一对相同的密钥, 需自行将该密钥对文件分发至不同的设备中)

### 2.使用自定义的密钥对运行
```bash
share-clip -u redis://:@127.0.0.1 -c cc-morning -n linux --secret-key /xxx/xxx/secret_key --public-key /xxx/xxx/public_key
```


[^1]: https://datatracker.ietf.org/doc/html/rfc8439
