# wbhub — WorkBuddy Hub for Termux

**在 Termux 里跑一个 OpenAI 兼容端点，用你自己的 WorkBuddy / CodeBuddy 账号额度。**

这是 [`WorkBuddy-Hub-Android`](https://github.com/xc3qaq/WorkBuddy-Hub-Android) 的
**Rust 重写版**，专门为 Termux 编译。装到手机上就是一个单文件二进制，不需要 Android
Studio、不需要 APK、不需要电脑。

```sh
pkg install -y curl
curl -L -o wbhub-arm64 https://github.com/OTFiles/WorkBuddy-Hub-Android/releases/download/termux-latest/wbhub-arm64
chmod +x wbhub-arm64
mv wbhub-arm64 wbhub        # 可选：改短名字，后面用 ./wbhub
./wbhub login               # 浏览器登录一次
./wbhub serve               # 起服务
```

然后任意 OpenAI 兼容客户端把 `baseUrl` 指过来：

```
baseUrl = http://127.0.0.1:8765/v1
apiKey  = wbhub serve 启动时打印的那串
```

---

## 为什么要另做一个 Termux 版

原项目是 Android App（Kotlin + Compose）。它的核心其实是三段**与 Android 无关**的
逻辑：

| 原文件 | Android 依赖 |
|---|---|
| `proto/Wire.kt`（协议常量、请求体改写、身份头） | **无** |
| `bridge/BridgeServer.kt`（`ServerSocket` 手写 HTTP + SSE 透传） | **无** |
| `proto/UpstreamClient.kt`（上游 HTTP） | 仅一行 `android.util.Log` |

也就是说，「协议」和「服务」两件事本来就不需要 GUI，也不需要 Android。把它们搬进
Termux 之后：

* **不用悬浮窗保活** —— Termux 有 `termux-wake-lock`，而且手机上的 Termux 进程本来
  就是前台终端，比被 ROM 冻结的 App 稳。
* **不用装 APK** —— 一个 2.3 MB 的静态二进制，`scp` 过去就能跑。
* **可脚本化** —— 签到、查余额、看用量都能进 cron / Tasker，不用点屏幕。
* **能当 Wi-Fi 上的小服务器** —— `--lan` 打开后笔记本也能用手机上这个端点。

---

## 安装

### 方式一：直接下载（推荐）

```sh
# 先确认架构
uname -m                     # aarch64 = 用 wbhub-arm64；armv7l = 用 wbhub-arm

pkg install -y curl
curl -L -o wbhub-arm64 https://github.com/OTFiles/WorkBuddy-Hub-Android/releases/download/termux-latest/wbhub-arm64
# 校验（可选但建议：手机上下载很容易被截断）
# 注意 .sha256 里的文件名就是 wbhub-arm64，所以这里要保持同样的名字
curl -L -o wbhub-arm64.sha256 https://github.com/OTFiles/WorkBuddy-Hub-Android/releases/download/termux-latest/wbhub-arm64.sha256
sha256sum -c wbhub-arm64.sha256
chmod +x wbhub-arm64
mkdir -p ~/.local/bin && mv wbhub-arm64 ~/.local/bin/wbhub
wbhub --version
```

`~/.local/bin` 一般已经在 `PATH` 里；没有的话加一行到 `~/.bashrc`：

```sh
export PATH="$HOME/.local/bin:$PATH"
```

### 方式二：在手机上自己编译

Termux 自带 `rust` 包，**不需要 NDK**（不像 GitHub Actions 里的交叉编译）：

```sh
pkg install -y rust git clang binutils
git clone https://github.com/OTFiles/WorkBuddy-Hub-Android.git
cd WorkBuddy-Hub-Android/termux-rust
cargo build --release          # 约 5-15 分钟，视手机而定
cp target/release/wbhub ~/.local/bin/
```

> 首次编译要拉 ~200 个 crate。`openssl` 不需要：TLS 走 `rustls`，纯 Rust 实现。

---

## 使用

### 1. 登录

```sh
wbhub login                # 默认国内版
wbhub login --region global   # 国际版
```

命令会请求一个新会话，把授权链接打印出来，并尝试用 `termux-open-url` 直接打开浏览器。
在浏览器里完成登录即可，命令会自己轮询到 token 落地。

* 国内版和国际版是两套独立账号体系，可以各登录一个。
* 一个版本可以存多个账号（再跑一次 `login` 就是加号）。
* token 过期前 5 分钟会自动续期，写回原账号。

### 2. 起服务

```sh
wbhub serve
```

```
Base URL : http://127.0.0.1:8765/v1
API Key  : wb-local-xxxxxxxx
局域网   : 未开启（--lan 可开启）
```

常用参数：

| 参数 | 作用 |
|---|---|
| `--port 9000` | 换端口 |
| `--lan` | 同时服务局域网（用另一个 key，见下） |
| `--bind 0.0.0.0` | 手动指定监听地址 |
| `--quiet` | 不打每个请求的日志 |

**保持后台运行**（推荐配合 tmux + wake lock）：

```sh
termux-wake-lock
tmux new -s wbhub 'wbhub serve'
# 脱离：Ctrl-B 然后 D；回来：tmux attach -t wbhub
```

### 3. 接到客户端

任何说 OpenAI 协议的客户端都行：

```json
{
  "providers": {
    "workbuddy": {
      "baseUrl": "http://127.0.0.1:8765/v1",
      "api": "openai-completions",
      "apiKey": "wb-local-xxxxxxxx"
    }
  }
}
```

模型名用 `wbhub models` 看：

```
$ wbhub models
模型                           倍率         上下文        标记
hy4-preview                  x0.29      960000     推理 图片
hy3                          x0.00      192000     推理 图片
glm-5.3                      x0.79      1000000    推理 图片
deepseek-v4.1-flash          x0.11      1000000    推理 图片
...
```

### 4. 局域网共享

```sh
wbhub serve --lan
```

启动后会多打印一个 **LAN Key** 和手机在 Wi-Fi 上的地址，笔记本上把 `baseUrl` 换成
`http://<手机IP>:8765/v1`、`apiKey` 换成 LAN Key 即可。

局域网用的是**独立 key**：万一泄露了，`wbhub config --rotate-lan-key` 换掉就行，
本机客户端的配置不用动。默认不开局域网——端点手里握着你的凭证，值得显式打开一次。

### 5. 日常命令

```sh
wbhub status              # 当前账号、端口、key
wbhub balance             # 剩余额度
wbhub balance --json      # 给脚本用
wbhub models --ids        # 只列模型名
wbhub accounts            # 所有账号（* 标记当前使用的）
wbhub use 8a433605        # 按 id / 昵称 / 前缀切换
wbhub calls               # 最近的调用记录
wbhub calls --summary     # 调用次数 / token / 积分汇总
wbhub stats               # 调用次数 + 当前积分，一条命令看全
wbhub doctor              # 逐层自检：DNS / TCP / TLS / 接口 / 凭证
wbhub config --json       # 看设置
wbhub config --set port=9000
wbhub config --rotate-key
wbhub export --out auth.json    # 导出凭证给别的工具
```

### 6. 终端界面（TUI）

```sh
wbhub tui
```

一个类似原 App 的终端界面，**没有内置 Agent**：终端里用别的客户端就行，这一点从
一开始就是有意这样取舍的。

```
 WorkBuddy Hub  国内版 · 18923001005                              服务运行中 :8765
  1 概览    2 模型    3 调用    4 成长    5 设置
┌ 账号与额度 ─────────────────────────────────────────────────────────────┐
│账号          18923001005                                                │
│剩余积分      1135.20                    周期结束 2026-11-01             │
│调用次数      42 次累计，今日 7 成功 / 0 失败                            │
│Token 用量    输入 128,430，输出 31,902   已消耗 12.48                   │
└─────────────────────────────────────────────────────────────────────────┘
```

| 页 | 内容 |
|---|---|
| 1 概览 | 账号、Token 有效期、**剩余积分**、**调用次数 / Token / 已消耗**、Base URL 与 API Key |
| 2 模型 | 模型名、倍率（免费标绿）、上下文、推理/图片标记 |
| 3 调用 | 每次调用的时间、模型、结果、tokens、积分、失败原因，外加汇总 |
| 4 成长 | 连登、补签卡、可兑换档位、抽奖次数、猫猫位置、夜猫子进度 |
| 5 设置 | 端口、局域网开关、调用记录上限、刷新间隔，以及换密钥 |

按键：`Tab` 切页 · `1`–`5` 直接跳 · `r` 刷新 · `s` 启停服务 · `←→` 调数值 ·
`Enter` 开关 · `k` 换密钥 · 成长页里 `c` 签到 / `d` 日常 / `t` 旅行 / `n` 夜猫子 ·
`q` 退出。

服务是**在 TUI 里启停的**：按 `s` 后它会在进程内起监听，用 `q` 退出时一并停掉，
不需要另开一个终端跑 `serve`。界面自适应窄屏——手机竖屏 50×20 也放得下关键信息。

### 7. 出问题时先跑自检

```sh
wbhub doctor
```
```
WorkBuddy 连通性自检（数据目录 /data/data/com.termux/files/usr/var/lib/wbhub）

  ✓ 账号         189****1005（国内版）· cn:8a433605
  ✓ DNS 解析     copilot.tencent.com → 14.29.51.236, 113.108.215.151
  ✓ TCP 连接     113.108.215.151:443 用时 15 ms
  ✓ TLS 握手     证书校验通过
  ✓ 配置接口      HTTP 200
  ✓ 模型列表      19 个模型
  ✓ 额度接口      剩余 1100.00
  ! 本地端点      127.0.0.1:8765 已有服务在监听（要另起一个先换端口）

  结论：这一侧没问题。
```

它逐层区分「网络不通」和「账号失效」——这两种在报错里长得一模一样，但要做的事完全
不同。没通过时退出码非 0，可以直接放进脚本。

### 8. 签到与成长中心

```sh
wbhub checkin             # 只签到
wbhub daily               # 补签 → 礼包/补偿 → 兑换连登档位 → 抽奖
wbhub daily --with-checkin --with-travel   # 顺手签到 + 推猫猫旅行
wbhub travel              # 派出猫猫 / 到站后领取
wbhub travel --status     # 只看猫在哪
wbhub tasks               # 成长任务进度
wbhub night --status      # 夜猫子任务进度
wbhub night               # 上报夜猫子（仅 23:00–08:00 有效）
```

成长中心只有国内版账号有；国际版会直接告诉你没有。这些动作在上游都是幂等的，
所以 `wbhub daily` 可以每天重复跑、也可以塞进 cron：

```sh
# 每天 9 点签到并领完当日奖励
0 9 * * * WBHUB_DIR=$PREFIX/var/lib/wbhub $PREFIX/bin/wbhub daily --with-checkin >> $PREFIX/var/log/wbhub.log 2>&1
```

`cronie` 在 Termux 里要 `pkg install cronie && crond`；不想装 cron 的话用
`termux-job-scheduler` 也行。

数据都在 `$PREFIX/var/lib/wbhub/`（可用 `--data-dir` 或 `WBHUB_DIR` 改）：

```
wb-auth-cn.json      国内版账号
wb-auth-global.json  国际版账号
settings.json        端口与密钥
calls.jsonl          调用记录
```

**凭证文件是 `0600` 权限**，key 首次运行时随机生成（不是写死的默认值——写死的默认值
在所有安装之间共享，等于没有 key）。

---

## 和原 Android 版的差异

保留下来的：

* **协议层逐条对齐** —— `/v2/chat/completions` 的请求体改写（强制 `stream`、
  `developer` → `system`、`tool_choice` 扁平化）、全套 `X-IDE-*` / `X-No-*` 身份头、
  国际版额外要求的前导 `system` 消息、`/v3/config` 的模型名册取交集、错误分类
  （`hard_credit` / `soft_rate` / `session_dead` / …）、以及积分与企业额度的两种读取路径。
* **透传而非缓冲** —— 上游永远是流式，转发时逐块送出，客户端能立刻看到第一个 token。
* **usage 边转发边扫描** —— 上游把 token 数和积分消耗放在最后一帧，转发时顺手扫出来，
  写进调用记录。
* **双层鉴权 + 入站加固** —— 本机/LAN 两把 key；`Host` 与 `Origin` 必须指向本机或私有
  网段，请求体必须是 JSON（否则一个跨站表单就能触发一次付费调用）。
* **成长中心** —— 连登补签、档位兑换、抽奖、猫猫旅行、夜猫子、成长任务，接口与顺序
  都照搬原实现（补签优先、兑换发抽奖次数所以抽奖在最后）。

改动了的：

| | 原 Android 版 | 这个版本 |
|---|---|---|
| 运行形态 | APK + Compose UI | 单文件 CLI |
| 保活 | 悬浮窗 + 前台服务 + WakeLock + 闹钟 | `termux-wake-lock` + tmux |
| 凭证存储 | `SharedPreferences` + 内部文件 | 一个 0600 的 JSON |
| 登录 | 内置 `WebView` | 打印链接 + 轮询（协议本来就是这个） |
| 调用记录 | SQLite | JSONL（`jq` 直接能读） |
| 模型名册 | 内存缓存 | 内存缓存 + 定时刷新 |
| 多账号轮换 | 有 | **没有**，一次服务一个账号 |
| 签到/成长任务 | 有（自动） | 有（`daily` / `travel` / `night`，手动或 cron 触发） |
| 内置 Agent 页 | 有 | 没有（终端里用别的客户端就行） |

轮换是有意不做的：它更适合放在 `workbuddy2api-panel` 那种多账号网关里，而这个项目
的定位是「手机上一个干净的单账号端点」。

### 另外顺手修掉的几个原版问题

移植过程中有几处原实现的行为会让人踩坑，这里一并改掉：

| 问题 | 原实现 | 这边 |
|---|---|---|
| `stream: false` 的客户端 | 一律回 SSE，客户端解析失败 | 攒成完整的 `chat.completion` 再返回 |
| 非 JSON 响应 | 当成成功（`Step(true, text)`），于是一个 401 会显示「礼包已领取」 | 一律算失败并说明原因 |
| 会话过期 | 报「返回了非 JSON」加一页 HTML | 报「账号会话已失效（HTTP 401），请重新运行 `wbhub login`」 |
| 错误详情 | 整页 HTML 塞进调用记录，撑坏表格 | 压成一行可读的话 |
| 分块请求体 | 只查 `Content-Length`，chunked 无上限 | `Limited` 硬截断，超限直接 413 |
| 连接超时 | 30 秒 socket 超时 | 30 秒请求头超时 + 90 秒上游静默超时 |
| 客户端中途断开 | 记录丢失 | `Drop` 里补一条 `aborted` 记录 |
| `wbhub models \| head` | panic（Rust 默认忽略 SIGPIPE） | 恢复 SIGPIPE 默认处置，安静退出 |
| 模型列表拉取失败 | `/v1/models` 一直空 | 空的时候懒加载重试一次 |
| 传输层报错 | 只有一句 `transport error` | 带上完整原因链和中文分类（连接失败 / 超时 / TLS / 请求体） |
| 手机睡醒后第一次请求 | 复用死连接直接 502 | 空闲连接只留 10 秒 + TCP keepalive + 连接级失败重试一次 |

---

## 安全

* 端点持有你的 bearer token。默认**只监听 `127.0.0.1`**。
* 开 `--lan` 时，HTTP 是明文的：同网段的人可以嗅探到 key。只在可信 Wi-Fi 上用。
* `Host` / `Origin` 校验会挡住「网页 DNS 重绑定到本机」这类跨站攻击。
* 凭证文件权限 `0600`，key 随机生成。
* 凭证来自官方 CLI 的 OAuth 流程，指向的是私有接口而**不是**官方开放 API。上游一改就
  可能失效，用之前请自行确认这符合你账号的服务条款。

---

## 许可

MIT，与原项目一致。原项目版权归 `xc3qaq` 及贡献者；本 Rust 移植版同样以 MIT 发布。

本项目与腾讯、WorkBuddy、CodeBuddy 无关联，未获其授权或认可；相关名称仅用于描述兼容
关系。仅供个人学习与研究使用，请遵守上游服务条款。
