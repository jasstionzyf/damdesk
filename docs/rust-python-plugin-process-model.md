# Rust 宿主 ↔ Python 插件：交互与进程模型（实现侧）

> 配套文档：协议契约见 `docs/industry-pack-protocol.md`（v1.1 冻结）。本文只讲**实现**：谁是谁、谁启动谁、怎么通信、怎么死。
> 代码位置：`crates/pack-host/src/process.rs`（子进程与 stdio JSON-RPC）、`crates/app-web/src/compliance_api.rs`（组装 `PackHostConfig`）、`crates/app-tauri/src/shell.rs`（桌面形态装配）。

---

## 0. 一句话

**`pack-host` 不是进程，是宿主进程里的连接管理器；每个插件（pack）对应一个被 spawn 的独立 Python 子进程，父子通过 stdio JSON-RPC 说话，插件要用算力必须回调宿主的能力网关。**

---

## 1. 进程拓扑

桌面形态（`app-tauri.exe`，**一个**进程）：

```text
app-tauri.exe  (PID 27424)                     ← 父进程（唯一真实进程）
 ├─ WebView2 渲染进程（系统托管）                  ← 显示 React 界面
 ├─ axum HTTP 服务线程 = app-web 代码              ← 127.0.0.1:<随机端口>，托管 web-dist + /api/*
 │    └─ PackHost 对象（pack-host crate，内存结构）  ← 管理连接/状态机/冷却，不是进程
 │         ├─ python.exe -m dampack_launcher <packA>   ← 子进程，PPID = 27424
 │         └─ python.exe -m dampack_launcher <packB>   ← 子进程，PPID = 27424
 └─ 能力网关 Gateway（宿主侧 HTTP）                  ← 插件回调 io.read / detect.* / op.* / vl.judge 时由宿主真跑
```

容器/测试形态（`dam-dev` 内单跑）：上面的 `app-tauri.exe` 换成 `app-web` 二进制进程，其余结构完全相同，父进程变成 `app-web`。

| 名字 | 是什么 | 是进程吗 |
|---|---|---|
| `app-tauri` | Tauri 桌面壳（开窗口、解析资源目录、起服务线程） | 是（桌面形态唯一真实进程） |
| `app-web` | axum HTTP 服务（`/api/*` + 静态托管） | 桌面：同进程线程；容器测试：独立二进制 |
| `pack-host` (`PackHost`) | 单个插件的连接管理器：懒启动、握手、重试、冷却、超时、cancel | **否**，内存对象 |
| 插件（python） | 第三方/官方包代码，跑在 `dampack_launcher` 里 | **是**，OS 级独立子进程 |
| 能力网关 `Gateway` | 宿主侧 HTTP，唯一算力入口（插件不许自己读图/联网） | 否，宿主进程内的服务 |

---

## 2. 启动时序（懒启动，不是开机就拉）

```text
壳 app-tauri
 1. TcpListener::bind(("127.0.0.1", 0))            # 随机端口，避免冲突
 2. app_web::prepare(&ServeConfig{data_dir, dist, token, models_root, ...})
 3. 起线程 serve → WebView2 打开 http://127.0.0.1:<port>

首次判定 / 设置页点「测试」时（app-web 内，compliance_api）
 4. GatewayConfig::from_manifest(manifest, work_dir) → Gateway::start(...)   # 随机端口 + 一次性 token
 5. PackHostConfig::new(gateway.base_url(), gateway.token(), work_dir)
      .with_launcher_dir(<launcher 脚本目录>)
      .runtime_python = <解释器绝对路径>            # ← KI-28：当前无人赋值，必须补
 6. PackHost::open(pack_dir, cfg)
 7. ensure_ready() → Command::new(interpreter())
      .args(["-m", "dampack_launcher", <pack_dir>])
      .env(DAMPACK_GATEWAY_URL / _TOKEN / _PACK_DIR / _WORK_DIR / _LOG_LEVEL)
      .stdin(piped) .stdout(piped) .stderr(piped)      # cwd = 包根目录
 8. health 握手（2s 上限）
      成功 → 子进程进入可用态；失败 → 重启 ×2 → 仍失败 → cooldown 300s
```

---

## 3. 解释器怎么选（`process.rs::interpreter()`）

优先级（前面的赢）：

```text
1) <pack>/venv/Scripts/python.exe   （Windows；每包独立 venv，装包时用内置 runtime 创建）
2) cfg.runtime_python               （随安装包内置的 python-build-standalone，绝对路径）
3) "python3"                        （裸字符串，交给宿主 OS 的 PATH 解析）
```

### 3.1 真机实证：第 3 档在 Windows 上是个陷阱（KI-28）

2026-10-21 Windows 真机实测：

```text
where python →
  C:\Users\Administrator\AppData\Local\Programs\Python\Python311\python.exe        ← 真解释器 3.11.9
  C:\Users\Administrator\AppData\Local\Microsoft\WindowsApps\python.exe|python3.exe  ← Store「App 执行别名」桩，PATH 先命中
```

判定请求的原始返回：

```text
status=400  插件引擎不可用：插件握手失败（com.soujpg.pack.ecommerce@1.0.0#…）：plugin_load_failed：
重启 2 次仍失败，进入冷却 300s：…stderr 尾部：Python was not found; run without ar…
```

结论：
- 「机器上有 python」≠ 能用。Windows 的 `python3` 常解析到 Store 别名桩。
- 安装/激活（解包 + 注册 manifest）**不 spawn 进程**，所以 `activated=true` 不代表可运行；一判定才暴露。
- 打包形态必须把 `cfg.runtime_python` 设成内置解释器的**绝对路径**，并做 `-c "import json"` 探活；失败要报出尝试过的路径，**禁止静默回落 PATH**。
- `dampack_launcher.py` 同理：`launcher_dir()` 现在靠 `DAMPACK_LAUNCHER_DIR` env 或 `CARGO_MANIFEST_DIR/../../tools` 兜底，后者在 release 包里是构建机路径，必须改成随包内置（`<resource_dir>/launcher`）显式传入。

---

## 4. 通信：三条通道，互不混用

| 通道 | 方向 | 内容 | 约束 |
|---|---|---|---|
| 插件 stdin | 宿主 → 插件 | JSON-RPC 2.0 请求 / 通知 | 每行一个 JSON，UTF-8，`ensure_ascii=False`，写完立即 flush |
| 插件 stdout | 插件 → 宿主 | 应答 / 通知 | **只准协议帧**；日志、进度、异常栈一律不能进 stdout |
| 插件 stderr | 插件 → 宿主 | 日志/异常 | 宿主收集，作为 `plugin_load_failed` / `plugin_crash` 的诊断尾部 |
| 能力网关 HTTP | 插件 → 宿主 | `io.read`、`detect.ocr`、`detect.u2net`、`detect.white`、`op.geometric`、`op.generative`、`vl.judge`、`report.emit` | 只认启动时注入的 `DAMPACK_GATEWAY_URL` + `DAMPACK_GATEWAY_TOKEN`；插件不得扫端口、读宿主配置、联网 |

要点：
- **插件不碰像素算法**：真推理（u2net 分割、OCR、白度统计、VL 判定）全在宿主侧执行，插件只做编排与几何求解。
- 网关端口每次启动随机、token 一次性（进程退出即废），因此插件进程天然不能跨进程复用凭证。
- 超时语义：请求发出 → 等 `manifest.timeout_secs` → 超时发 `cancel` 通知（无 id）→ 宽限 2s → 仍未收尾则 kill 进程组；结果一律 `undetermined` + `note=plugin_timeout`，**绝不落 pass/fail**。

---

## 5. 生命周期与故障处理

```text
懒启动 → health(2s) → 可用
   │
   ├─ 断链（stdout EOF / 进程已退）→ 记一次崩溃 → 重启（max_restarts=2）→ 成功则继续派发
   │                                   └─ 重试耗尽 → cooldown 300s，结果 undetermined + note=plugin_crash
   ├─ 调用超时 → cancel 通知 → 宽限 2s → kill 进程组
   └─ 宿主 shutdown() → drop_child()（进程组一并回收）
```

为什么必须独立进程（架构取舍，不是实现细节）：

| 需求 | 线程做不到的 |
|---|---|
| 崩溃隔离 | 插件第三方代码 panic/段错误/死循环 → 线程会把宿主一起拖死；子进程可 kill 重启 |
| 解释器隔离 | 每包 venv（numpy 1.26 vs 2.x、原生库 ABI/DLL/OpenMP）走线程无法隔离 |
| 依赖隔离 | `requirements.txt` 只装进包内 venv，不污染宿主共享 runtime |
| 强制超时 | 线程无法安全强杀，子进程可直接 terminate |
| 可信边界 | 能力网关 + 一次性 token 把「插件能做什么」收敛成显式清单 |

---

## 6. 形态差异（同一套代码，两种宿主）

| 形态 | 宿主进程 | app-web 形态 | python 解释器来自 | PPID 指向 |
|---|---|---|---|---|
| 桌面安装包（Windows） | `app-tauri.exe` | 同进程 axum 线程 | `<resource_dir>/runtime/python/python.exe`（内置，KI-28 待接线） | `app-tauri.exe` |
| 容器/开发测试 | `app-web` 二进制（`/cargo-target/dam/release/app-web`，env `DAM_WEB_DATA/PORT/BIND`） | 独立进程 | 容器内 `python3`（3.11.2） | `app-web` |

实测（真机 2026-10-21）：

```powershell
Get-CimInstance Win32_Process | Where-Object { $_.Name -match 'python|app-tauri' } |
  Select-Object ProcessId,ParentProcessId,Name,CommandLine

ProcessId ParentProcessId Name           CommandLine
    27424            7428 app-tauri.exe  "C:\Users\Administrator\AppData\Local\souJpg DAM\app-tauri.exe"
```

插件处于 cooldown 或未判定时看不到 `python.exe`；一旦判定成功派发，会出现 `python.exe`，其 `ParentProcessId` = 上面 `app-tauri.exe` 的 PID。

---

## 7. 自检命令

```bash
# 容器内：谁在跑、父子是谁
docker exec dam-dev bash -lc "ps -eo pid,ppid,cmd | grep -E 'app-web|dampack_launcher' | grep -v grep"

# 真机：进程树 + 命令行
sshpass -p '***' ssh Administrator@<host> "powershell -NoProfile -Command \"Get-CimInstance Win32_Process | Where-Object { \$_.Name -match 'python|app-tauri' } | Select-Object ProcessId,ParentProcessId,Name,CommandLine | Format-Table -AutoSize\""

# 解释器选择实测（插件日志里的握手失败尾部会写明尝试的路径/系统提示）
tail -50 "%APPDATA%\com.soujpg.dam.desktop\logs\*.log"
```
