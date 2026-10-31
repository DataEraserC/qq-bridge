# 文件中继部署示例：OneBot 怎么访问到桥的文件服务器

OneBot 实例的部署方式不受我们控制：有人 docker、有人 podman、有人本机裸跑、有人放另一台服务器。
`[file_relay]` 的设计就是让**桥这边配一次，平台那边零改动**——桥把本机文件收下来换成 URL，
平台只管 GET 这个 URL。唯一的配置点：`public_base` 必须写成**平台那边**能访问到桥的地址。

```toml
# 桥的 config.toml
[file_relay]
mode = "file-server"        # 缺省即此值，当前唯一实现；写别的启动即报错
listen = "0.0.0.0:3131"     # 几乎所有场景都要 0.0.0.0（见下面每个坑）
public_base = "..."         # 按下表选
# dir = "state/relay"       # 默认值，可不写
```

## 场景一：OneBot 跑在 podman 容器里（Linux，rootful/rootless 都行）

```toml
public_base = "http://host.containers.internal:3131"
```

`host.containers.internal` 由 podman 4+ 自动注入（aardvark-dns），开箱即用。
本机真机已实测（rootless podman 5.8，容器里的 QQ 直接 fetch 成功取图）。

注意：**别填容器网关 `10.89.0.1`**——实测 fetch failed，网关是桥命名空间里的
aardvark，不到宿主。也别推断「pasta 会转给宿主」，实测不会。

## 场景二：OneBot 跑在 docker 容器里（原生 Linux）

Docker **默认没有**任何「宿主」名字，Docker 20.10+ 要在 compose 里显式加：

```yaml
services:
  napcat:
    image: ...
    extra_hosts:
      - "host.docker.internal:host-gateway"
      - "host.containers.internal:host-gateway"   # 跨引擎保险：podman 也认
```

```toml
public_base = "http://host.docker.internal:3131"
```

`host-gateway` 是占位符，docker 造容器时替换成网桥网关（通常 172.17.0.1）。

## 场景三：Docker Desktop（Windows / macOS）

```toml
public_base = "http://host.docker.internal:3131"
```

自动注入，开箱即用，compose 不用加任何东西。

## 场景四：OneBot 本机裸跑（和桥同一台机器，无容器）

```toml
public_base = "http://127.0.0.1:3131"
```

## 场景五：OneBot 在另一台服务器/机器上

```toml
public_base = "http://10.147.20.150:3131"   # 桥机的 zerotier / 局域网 / 公网 IP
```

前提两件事：
1. 桥 `listen = "0.0.0.0:3131"`（听 127.0.0.1 别人连不进来）；
2. 桥机防火墙放行 3131（`ufw allow 3131` 之类）。
   走公网时建议前置反代加 TLS，或套 zerotier/tailscale 这类私网。

## 两个通用的坑（每个场景都检查）

1. **DNS 解析对了 ≠ 连得上**：桥只听 `127.0.0.1` 的话，容器/远程一律 connection
   refused。除非场景四（同机裸跑才可以用 127.0.0.1），否则 listen 写 `0.0.0.0`。
2. **Docker 不认识 `host.containers.internal`**，那是 podman 的名字；反过来
   podman 把 `host.docker.internal` 也一起注入了。所以场景二的 compose 把两个
   名字都加上，配置文件拷到任何引擎都能跑。

## 验证方法

配完后从 OneBot 那边（容器里 / 远程机器）跑一次：

```sh
curl -sS -o /dev/null -w '%{http_code}\n' http://<public_base 的 host>:3131/
```

404 = 通了（没有 token 当然 404，能拿到 HTTP 状态码就说明网络是通的）；
connection refused / fetch failed = listen 或防火墙问题；
name resolution failed = public_base 的主机名在这个环境里不存在，回上表换。
