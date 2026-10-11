# WorkBuddy Hub Android

[English](./README.en.md) | 中文

> **这是 [`xc3qaq/WorkBuddy-Hub-Android`](https://github.com/xc3qaq/WorkBuddy-Hub-Android) 的 fork，增加了
> [**专门在 Termux 里编译运行的 Rust 重写版**](./termux-rust)——单文件二进制，不需要 APK、
> 不需要电脑。想要这个就在手机上装：
>
> ```sh
> curl -L -o wbhub https://github.com/OTFiles/WorkBuddy-Hub-Android/releases/download/termux-latest/wbhub-arm64
> chmod +x wbhub && ./wbhub login && ./wbhub serve
> ```
>
> 下面的内容是**原 Android 版**的说明，原样保留。

把 WorkBuddy 账号接到任意 OpenAI 兼容客户端。App 在手机本地起一个转发端点，你的
编辑器 / Agent / 脚本把 `baseUrl` 指过来就能用，不需要官方开放 API，也不需要电
脑。

```
手机
├─ WB Hub
│    ├─ OAuth 登录（国内版 / 国际版）
│    ├─ 本地转发端点  127.0.0.1:8765
│    └─ 悬浮窗状态面板（防止后台被系统冻结）
│
└─ 任意 OpenAI 兼容客户端
     └─ baseUrl = http://127.0.0.1:8765/v1
```

## 功能

- **账号接入**：走官方 CLI 的 OAuth 流程取凭证，登录一次即可长期使用，Token 过期
  自动刷新
- **双版本并存**：国内版与国际版是两套独立账号体系，可以各登录一个并随时切换，
  切换即时生效，无需重新登录
- **本地转发端点**：OpenAI 兼容（`/v1/models`、`/v1/chat/completions`），流式输出
  原样透传
- **悬浮窗保活**：显示一个小状态面板，让应用保持可见，避免服务在后台被系统冻结。
  面板可拖动、可收起，支持调节透明度与固定位置
- **调用记录**：记录每次调用的模型、Tokens 与积分消耗，并汇总统计
- **签到与余额**：查看剩余积分、执行每日签到

## 构建

需要 JDK 21 与 Android SDK（含 platform 35 与 build-tools）。Gradle 由项目自带
的 wrapper 提供，无需单独安装。

### Linux / macOS

```sh
echo "sdk.dir=$HOME/Android/Sdk" > local.properties
./gradlew assembleDebug
```

### Windows

在 PowerShell 中：

```powershell
"sdk.dir=$env:LOCALAPPDATA\Android\Sdk" | Out-File -Encoding ascii local.properties
.\gradlew.bat assembleDebug
```

`local.properties` 里的 SDK 路径按你的实际安装位置调整。

产物在 `app/build/outputs/apk/debug/app-debug.apk`。

### 安装到设备

```sh
adb install -r app/build/outputs/apk/debug/app-debug.apk
```

## 使用

1. **登录**：打开 App → 「凭证」→ 选择版本 → 点登录 → 在浏览器完成登录
2. **启动服务**：「API 平台」→ 打开开关
3. **开启悬浮窗**：点「开启悬浮窗」并授权。这一步是为了让服务在后台保持运行
4. **接入客户端**：「API 平台」页显示 baseUrl 与 apiKey，填进你的客户端

### 客户端配置

以 `models.json` 风格的配置为例：

```json
{
  "providers": {
    "workbuddy": {
      "baseUrl": "http://127.0.0.1:8765/v1",
      "api": "openai-completions",
      "apiKey": "wb-local",
      "models": [
        { "id": "hy3" }
      ]
    }
  }
}
```

模型名见 App「API 平台」页的列表（点模型名可复制）。

## 为什么需要悬浮窗

Android 会在应用进入后台后冻结进程，被冻结的进程不再处理网络请求，表现就是客户
端调用超时。前台服务并不足以避免这一点——本项目的实测结果是被冻结后仍然超时。

悬浮窗让应用保持"可见"状态，进程重要性因此提升，不再被冻结。这不需要 root，也不
需要额外工具，在系统设置里授权一次即可。

## 工作原理

```
客户端请求
  │  OpenAI 协议
  ▼
本地转发端点（127.0.0.1:8765）
  │  ① 改写请求体（上游要求 stream、拒绝 developer 角色）
  │  ② 补上 WorkBuddy 专有的身份头
  │  ③ 带上你的 access token
  ▼
WorkBuddy 上游（copilot.tencent.com / www.workbuddy.ai）
  │  返回 SSE 流
  ▼
原样透传给客户端
```

转发端点只监听 `127.0.0.1`，且每个请求都要带共享密钥，因此同设备上的其它应用无
法直接调用它。

上游接口是 WorkBuddy 客户端使用的私有接口，不是官方开放 API。上游改动可能导致功
能失效，届时需要跟随调整。

## 已知限制

- 悬浮窗保活依赖系统的重要性判定，在激进的后台管理策略下仍可能被限制
- 国际版账号可能需要先在官方客户端完成试用激活，否则上游会拒绝计费类请求
- 依赖 WorkBuddy 私有接口，上游更新后可能需要适配
- 仅在 Android 上验证

## 免责声明

- 本项目仅供个人学习与研究使用，仅驱动使用者自己的账号在本机调用
- 使用者需遵守 WorkBuddy 的服务条款；因使用本项目产生的任何后果由使用者自行承担
- 本项目与腾讯、WorkBuddy 无关联，未获其授权或认可；相关名称仅用于描述兼容关系

## 许可

[MIT](./LICENSE)
