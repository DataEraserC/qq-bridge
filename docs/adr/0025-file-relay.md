# ADR-0025 文件中继：桥把本机文件变成平台取得到的 URL

来源：用户裁定。起因是真机实测：agent 在宿主机生成的图（comfyui 的输出），`send_message` 发图、`platform_call` 上传文件全部失败——平台（snowluma）跑在 podman 容器里，跟 agent 不共用文件系统。用户裁定最干净的改法是「**桥侧同时部署一个文件服务器，snowluma 等需要配置使其能访问文件服务器**」，并明确否掉了路径映射方案（「你得考虑各个平台（windows docker linux podman）、各个 onebot」——映射要每个环境单独配一遍，配不完）。

## 断层是什么

agent 给的是**它自己文件系统里的路径**，平台去 stat 的是**它自己命名空间里的同一路径**。两边经常对不上：

```
agent:  elements=[{type:"image", url:"/home/nixos/.local/share/comfyui/output/a.png"}]
平台:   stat /home/nixos/... → ENOENT（它容器里根本没有这个挂载）
```

更糟的是报错：平台回一个裸 `ENOENT: no such file or directory`，把「文件不存在」和「文件存在但不在你的命名空间里」混成一句。人（和模型）会先去查拼写、权限、软链——而这些全是对的，排查方向直接被带偏。

这个问题**必然**撞上：容器部署是各平台的默认姿势，「生成图/截图发群里」是最常见的一类需求。

## 解法：桥自带文件服务

```
agent ── send_message(image, url="/home/.../a.png") ──→ 桥
桥:     本机有这个文件？ ──有──→ 收下（fs::copy 落盘 state/relay/<token>/）
        ──换成 URL──→ http://<public_base>/f/<token>/a.png
平台 ←── file=<URL> ── 桥
平台:   HTTP GET 这个 URL ──→ 桥把文件还回去
```

- **agent 只给路径，平台只认 URL**，两边谁也不用知道对方的挂载表。docker / podman / windows、NapCat / snowluma / Lagrange，一套通用。
- **只在连接配了 `[file_relay]` 时启用**。没配 = 完全不碰路径，行为与没有这个功能时一致。
- **只动「本机真实存在」的绝对路径**（`/…`、`C:\…`、`file://…`）。`http(s)://` 和 `base64://` 原样放行——它们本来就不依赖本地命名空间。本机**没有**的路径也不动：那可能是平台侧（共享卷、平台缓存）才存在的路径，动了反而弄坏原本能用的写法。
- **覆盖两个入口**：`send_message` 的元素（image/audio/video/file）和 `platform_call` 的 params（递归遍历，upload_file 这类方法的路径在 params 里）。
- **回执说明**：`send_message` 换了路径就在回执里给 `relayed: [{from, to}]`——模型给的是本机路径，实际发出去的是 URL，不说它就永远不知道。
- **URL 是能力地址**：拿到 URL 就能取，不另设鉴权（跟 OneBot 自己回的图片 URL 同一档）。token 32 位十六进制随机；文件名消毒（非 ASCII 字母数字换 `_`）；`lookup` 拒绝带路径分隔符的名字和瞎编的 token，目录穿越进不来。
- **保留 24 小时**：桥启动时清一次过期条目。URL 在 QQ 侧取图是即时的，不需要活得更久；漏网的等下次重启。

## 配置

```toml
[file_relay]
listen = "0.0.0.0:3131"              # 桥自己监听
public_base = "http://10.89.0.1:3131" # 平台那边访问桥的地址（不含尾斜杠）
dir = "state/relay"                   # 收下的文件放哪（默认 state/relay）
```

`public_base` 是唯一需要按部署环境填的东西。**本机实测**（rootless podman 5.8 + pasta + netavark，让容器里的 QQ 直接 fetch 验证）：

| 候选地址 | 结果 |
|---|---|
| `http://host.containers.internal:3131` | ✅ 取到（message_id 回来了） |
| `http://10.147.20.150:3131`（宿主 zerotier IP） | ✅ 取到 |
| `http://10.89.0.1:3131`（容器网关） | ❌ fetch failed——网关是 netavark 桥命名空间里的 aardvark，不到宿主 |
| `http://192.168.1.8:3131`（宿主局域网 IP） | ❌ fetch failed |

注意：**「容器访问网关地址会被 pasta 转给宿主」这个推断是错的**，实测否掉了。结论就用 podman 自带的 `host.containers.internal`——它就是为这个场景准备的。桥监听 `0.0.0.0`。

## 错误提示：ENOENT 要分两种

桥拦不住平台自己的报错，但**可以往错误后面附诊断**。发送或平台调用的错误里出现 ENOENT / realpath / image not found 这类字样时，附上三条出路（配 `[file_relay]`、base64 内联、共享目录），并明说「别先去查路径拼写和权限」。

## base64 内联为什么不够

OneBot 的 `file` 字段原生支持 `base64://`，适配器透传，**今天就能用**——但它走的是桥的协议帧，单帧上限 1 MiB，base64 编码后原始内容约 750 KB 封顶。中继走 HTTP，没有这个限制（收文件上限 128 MiB，防磁盘塞满）。两条路并存：小图内联最省事，大文件走中继。

## 被否掉的方案：路径映射（path_map）

桥按配置把本机路径前缀换成平台侧前缀（`/home/nixos/... → /app/data/...`）。能解决「共享了卷但路径不一样」的单一环境，但：

- 每个部署环境（docker/podman/windows × 每种 OneBot）都要单独量一遍挂载表、配一遍映射；
- 配错还是 ENOENT，报错里只有换完的陌生路径，排查更绕；
- 解决不了「根本没共享」的环境（本机实测就是：`/app/data` 是个独立卷，不是家目录挂载）。

用户裁定不要，做成文件服务后它也没有存在的必要了。
