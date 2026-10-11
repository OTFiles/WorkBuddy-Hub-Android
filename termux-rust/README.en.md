# wbhub — WorkBuddy Hub for Termux

**An OpenAI-compatible endpoint on your phone, backed by your own WorkBuddy /
CodeBuddy account.**

This is a **Rust rewrite of
[`WorkBuddy-Hub-Android`](https://github.com/xc3qaq/WorkBuddy-Hub-Android)**, built
to run under [Termux](https://termux.dev). One static binary — no Android Studio,
no APK, no PC.

```sh
curl -L -o wbhub https://github.com/OTFiles/WorkBuddy-Hub-Android/releases/download/termux-latest/wbhub-arm64
chmod +x wbhub
./wbhub login      # sign in once, in a browser
./wbhub serve      # start the endpoint
```

Then point any OpenAI-compatible client at it:

```
baseUrl = http://127.0.0.1:8765/v1
apiKey  = the key printed by `wbhub serve`
```

---

## Why a Termux build at all

The original is an Android app (Kotlin + Compose). Its core is three pieces of
logic that turn out to have nothing to do with Android:

| Original file | Android dependency |
|---|---|
| `proto/Wire.kt` — protocol constants, body reshaping, identity headers | **none** |
| `bridge/BridgeServer.kt` — a hand-written HTTP server and SSE relay over `ServerSocket` | **none** |
| `proto/UpstreamClient.kt` — upstream HTTP | one line of `android.util.Log` |

The protocol and the server never needed a GUI. Once they live in Termux:

* **No keep-alive overlay.** Termux has `termux-wake-lock`, and a Termux process
  is already a foreground terminal — more robust than an app the ROM is free to
  freeze.
* **No APK.** A 2.3 MB static binary you can `scp` to the phone.
* **Scriptable.** Check-in, balance and usage fit in cron or Tasker instead of
  demanding taps on a screen.
* **Can act as a small server on the Wi-Fi.** `--lan` lets a laptop use the
  endpoint running on the phone.

---

## Install

### Option 1: download (recommended)

```sh
uname -m                     # aarch64 → wbhub-arm64;  armv7l → wbhub-arm

pkg install -y curl
curl -L -o wbhub https://github.com/OTFiles/WorkBuddy-Hub-Android/releases/download/termux-latest/wbhub-arm64
# Verify if you like — a truncated download is otherwise indistinguishable from a crash:
curl -L -o wbhub.sha256 https://github.com/OTFiles/WorkBuddy-Hub-Android/releases/download/termux-latest/wbhub-arm64.sha256
sha256sum -c wbhub.sha256
chmod +x wbhub
mkdir -p ~/.local/bin && mv wbhub ~/.local/bin/
wbhub --version
```

`~/.local/bin` is normally already on `PATH`; if not:

```sh
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
```

### Option 2: build on the phone

Termux ships a `rust` package, and a native build needs **no NDK** (unlike the
cross-compilation in CI):

```sh
pkg install -y rust git clang binutils
git clone https://github.com/OTFiles/WorkBuddy-Hub-Android.git
cd WorkBuddy-Hub-Android/termux-rust
cargo build --release          # roughly 5–15 minutes depending on the device
cp target/release/wbhub ~/.local/bin/
```

> The first build pulls about 200 crates. `openssl` is not needed: TLS uses
> `rustls`, which is pure Rust.

---

## Usage

### 1. Sign in

```sh
wbhub login                    # 国内版 (CN)
wbhub login --region global    # international build
```

The command opens a session, prints the authorisation URL, and tries to hand it
to `termux-open-url` so the browser comes up directly. Complete the sign-in
there; the command polls until the token lands.

* The two builds are separate account systems — sign in to one of each.
* Running `login` again for the same build adds another account.
* Tokens renew automatically five minutes before expiry and are written back to
  the account they came from.

### 2. Serve

```sh
wbhub serve
```

```
Base URL : http://127.0.0.1:8765/v1
API Key  : wb-local-xxxxxxxx
局域网   : 未开启（--lan 可开启）
```

| Flag | Effect |
|---|---|
| `--port 9000` | listen elsewhere |
| `--lan` | also serve the local network, with a separate key |
| `--bind 0.0.0.0` | bind explicitly |
| `--quiet` | no per-request log line |

**Keep it running in the background** (tmux + a wake lock is the reliable pair):

```sh
termux-wake-lock
tmux new -s wbhub 'wbhub serve'
# detach with Ctrl-B D; return with: tmux attach -t wbhub
```

### 3. Point a client at it

Any client that speaks the OpenAI protocol:

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

Model ids come from `wbhub models`:

```
$ wbhub models
模型                           倍率         上下文        标记
hy4-preview                  x0.29      960000     推理 图片
hy3                          x0.00      192000     推理 图片
glm-5.3                      x0.79      1000000    推理 图片
deepseek-v4.1-flash          x0.11      1000000    推理 图片
...
```

### 4. Share over the LAN

```sh
wbhub serve --lan
```

Startup then also prints a **LAN key** and the phone's Wi-Fi address. On the
laptop, use `http://<phone-ip>:8765/v1` with that key.

The LAN uses a **separate key**: if it leaks, `wbhub config --rotate-lan-key`
replaces it without touching the local clients' configuration. LAN mode is off by
default — the endpoint holds a credential, so widening it should be deliberate.

### 5. Day-to-day commands

```sh
wbhub status              # active account, port, key
wbhub balance             # remaining credit
wbhub balance --json      # for scripts
wbhub checkin             # claim the daily bonus
wbhub models --ids        # model ids only
wbhub accounts            # every account; * marks the active one
wbhub use 8a433605        # switch by id, nickname, or prefix
wbhub calls               # recent calls
wbhub calls --summary     # totals: calls, tokens, credit
wbhub config --json
wbhub config --set port=9000
wbhub config --rotate-key
```

State lives in `$PREFIX/var/lib/wbhub/` (override with `--data-dir` or
`WBHUB_DIR`):

```
wb-auth-cn.json      CN accounts
wb-auth-global.json  global accounts
settings.json        port and keys
calls.jsonl          call log
```

**Credential files are `0600`**, and the keys are generated on first run rather
than defaulted — a hard-coded default would be shared by every install, which on
a LAN-enabled bridge is the same as having no key at all.

> An existing `/data/data/com.wbhub.app/files/wb-auth-cn.json` from the Android
> build can be copied straight in: the field names are identical.

---

## Differences from the Android build

Kept, deliberately, line for line:

* **The protocol layer.** Request-body reshaping for `/v2/chat/completions`
  (force `stream`, `developer` → `system`, flatten `tool_choice`), the full set of
  `X-IDE-*` / `X-No-*` identity headers, the leading `system` message the
  international gateway requires, the model roster as an *intersection* of
  `/v3/config`'s two lists, the error classification (`hard_credit` /
  `soft_rate` / `session_dead` / …), and both the personal and enterprise
  billing read paths.
* **Relay, never buffer.** The upstream always streams; frames are forwarded as
  they arrive so the client sees the first token immediately.
* **Usage scanned while forwarding.** The upstream reports token counts and
  credit in a trailing frame; the relay reads it out of the same bytes it passes
  on, and records it.
* **Two-layer auth and inbound hardening.** Separate local and LAN keys;
  `Host` and `Origin` must name this device or a private address; bodies must be
  JSON — otherwise a cross-site form post could drive a paid model call.

Changed:

| | Android build | This build |
|---|---|---|
| Shape | APK + Compose UI | single-file CLI |
| Keep-alive | overlay + foreground service + WakeLock + alarm | `termux-wake-lock` + tmux |
| Credential store | `SharedPreferences` + internal file | one `0600` JSON file |
| Sign-in | embedded `WebView` | print URL + poll (which is all the protocol ever was) |
| Call log | SQLite | JSONL, readable with `jq` |
| Model roster | in-memory cache | in-memory cache + periodic refresh |
| Account rotation | yes | **no** — one account per running server |
| Check-in / growth tasks | yes | `checkin` only; growth tasks not ported |
| Built-in agent page | yes | no — use any client you like in the terminal |

The last three are deliberate omissions: rotation and growth tasks belong in a
multi-account gateway such as `workbuddy2api-panel`, whereas this project is
meant to be one clean single-account endpoint on a phone.

---

## Security

* The endpoint holds your bearer token and listens on **`127.0.0.1` only** by
  default.
* With `--lan`, traffic is plain HTTP: anyone on the same network can sniff the
  key. Use it on a network you trust.
* The `Host` / `Origin` checks block the "web page re-resolves its own domain to
  your loopback address" class of cross-site attack.
* Credential files are `0600`; keys are randomly generated.
* Credentials come from the official CLI's OAuth flow and target a **private**
  upstream interface, not a public API. It can break whenever the upstream
  changes; confirm this is allowed by your account's terms before relying on it.

---

## Licence

MIT, matching the original. Copyright for the original work belongs to `xc3qaq`
and its contributors; this Rust port is released under the same licence.

Not affiliated with, authorised by, or endorsed by Tencent, WorkBuddy or
CodeBuddy; names are used only to describe compatibility. For personal study and
research only — respect the upstream terms of service.
