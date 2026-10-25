# dsh-plugin-screen-control

让 DeepSeek Harness(DSH)里的模型能**观察并操作 Windows 桌面**,而且**不需要把目标窗口切到前台**。

TypeScript 薄壳 + Rust 常驻引擎 + 命名管道 IPC。TS 侧只做工具注册和转发,Rust 侧干所有脏活。

---

## 当前状态(先看这个)

引擎版本 **0.1.9**(2026-10-18)。

| 部分 | 状态 |
| --- | --- |
| TS 壳、工具注册、参数校验 | ✅ 已编译通过(`tsc` 0 错误) |
| 插件契约(18 个工具全部注册/可卸载) | ✅ 已独立验证 |
| 工具闭环:`tool.execute → 桥 → 命名管道 → 引擎 → 回包` | ✅ 已用 mock 引擎跑通(参数转换/协议帧/错误码/日志全过) |
| 进程守护、崩溃与超时、引擎缺失时的错误降级 | ✅ 已实现并验证 |
| Rust 引擎源码 | ✅ 18 个文件约 9170 行,rustc 1.99.0 (x86_64-pc-windows-gnu) 编译通过 |
| Rust 侧静态一致性 | ✅ 跨模块符号全部解析、无 `unwrap()`/`expect()`(业务路径)、无禁用 API |
| IPC 端到端闭环(真引擎) | ✅ 已跑通 —— 真引擎在真实桌面上枚举到真实窗口 |
| 工具闭环:`tool.execute → 桥 → 命名管道 → 真引擎 → 回包` | ✅ `selfcheck` 阶段二 B 全过,0 失败 0 跳过 |
| 新增 `win_find` + 同 exe 命令行 `find`/`capture`(0.1.9) | ✅ 单测 + 真机验收 + 只读冒烟全过 |
| `win_ocr` 子进程隔离 + OCR 尺寸护栏(0.1.9) | ✅ 整屏 OCR 崩溃被隔离在子进程里,引擎只拿到结构化错误 |
| `win_type` 非 ASCII 自动剪贴板(0.1.9) | ✅ 单测覆盖 ASCII/非 ASCII 分流;失败会退回 Unicode 注入并给 warning |
| SendInput 真注入 + 安全闸门 | ✅ 已接线并真机验证:目标不在前台 / 点被盖住 → 回 `UNSUPPORTED` |
| `MonitorInfo.dpi` | ✅ 已用 `GetDpiForMonitor` 填上(本机 144),不再是 `None` |
| `--features uia` 真 UIA 元素树 | ✅ 已编译并在靶窗上跑通,`source=uia` 10 个元素 |
| 真机验收(动作真的落到控件上) | ✅ `npm run acceptance` 全部检查 0 失败 |

**没有编译验证过的代码就不算能跑。** 上面每一行都有实际执行记录支撑;编不过的、没跑过的
一律标 ❌ 或 ⚠️,不写好听的。

`npm run selfcheck` 把"用假引擎验的"和"用真引擎验的"分成两档(阶段二 A / 二 B),
真引擎缺失时后者报 **SKIP**,不会拿 mock 的结果冒充。
`npm run acceptance` 验的是另一件事:动作有没有真的落到控件上(见第 4 节)。

---

## 架构

```
模型
 │  win_* 工具调用
 ▼
DSH 插件(TS 薄壳,lib/index.js)
 │  · defineTool 注册 18 个工具
 │  · 参数校验、把请求转成 JSON 帧
 │  · 惰性启动引擎 + 崩溃重启 + 卸载清理
 ▼
命名管道 \\.\pipe\dsh-screen-control-<pid>-<rand>   (NDJSON,UTF-8)
 ▼
Rust 常驻引擎(screen-control-engine.exe)
   · 窗口枚举 / 子窗口树 / 真 UIA(feature 开关)
   · PrintWindow / GDI 截图 → 缩放 → PNG；模板匹配(win_find / win_wait)
   · SendInput 真注入(默认) + WM_CHAR 显式路径(noactivate 守卫)
   · OCR 走短命子进程(win_ocr)，崩了只降级成结构化错误
   · DPI 感知、屏幕物理像素坐标换算、快照 TTL 校验
```

为什么这么分:输入注入和 UIA 全是 `unsafe` Win32 调用,放在 Node 里既慢又容易把 DSH 主进程带崩。
引擎是独立进程,崩了重启就行,不牵连 DSH。

详细协议见 [docs/IPC.md](docs/IPC.md)。

---

## 目录

```
dsh-plugin-screen-control/
├── package.json          # dsh.bundle.patch 指向 cordis.patch.yml
├── cordis.patch.yml      # 正式安装:bundle 挂载声明
├── cordis.yml            # 开发调试:--patch 手动 overlay
├── tsconfig.json
├── src/
│   ├── index.ts          # 插件入口,8 个 defineTool
│   ├── bridge.ts         # 命名管道客户端 + 进程守护
│   └── types.ts          # 协议类型
├── lib/                  # tsc 产物(DSH 实际加载这个)
├── rust-engine/          # Rust 常驻引擎
├── scripts/
│   ├── build-engine.ps1       # 编译引擎
│   ├── link-into-profile.ps1  # 挂进 profile(默认 dry-run)
│   ├── mock-engine.mjs        # 假引擎:只讲协议,用来验桥层
│   └── selfcheck.mjs          # 独立验证脚本(四阶段)
└── docs/IPC.md           # 协议契约
```

---

## 快速开始

### 0. 前置:让插件能解析到 DSH 的类型/模块

插件被 profile 以 `link:` 方式安装时,Node 会按**真实路径**解析 bare import,
所以插件自己的 `node_modules` 里必须有 `@deepseek-ai/*`。`package.json` 已经用
`file:` 依赖声明了,装一次即可:

```powershell
cd C:\Users\69544\.dsh\bundles\dsh-plugin-screen-control
npm install
```

> `npm ci` 会清掉这些 junction,清完重跑一次 `npm install` 就行。

### 1. 编译 TS 壳

```powershell
npm run build        # 产出 lib/
npm run typecheck    # 只想检查类型
```

### 2. 编译 Rust 引擎

本机已装好工具链(见下面「工具链现状」),直接跑:

```powershell
npm run build:engine                 # release 构建,产物 target\release\screen-control-engine.exe
npm run build:engine -- -Profile debug
```

脚本自己会做这几件事,不需要手工铺环境:

- **gcc 不在 PATH 时从 `C:\mingw64\bin` 临时补上** —— windows-gnu 目标靠 gcc 当链接器,
  少了它 cargo 会在最后一步报 `linker 'gcc' not found`,而那行错通常被几百行 warning 淹掉;
- 把 cargo 的 stderr 收进临时文件再回放 —— PS 5.1 会把原生命令的 stderr 当成致命错误,
  不处理的话**第一条 warning 就把脚本打断**;
- 编完独立回读产物是否真的存在(不信 cargo 的退出码),再同步一份到 `bin\`。

`-Target` 只有真要交叉编译到别的 triple 时才需要。带 `-Target` 时产物落在
`target\<triple>\<profile>\`,而 TS 桥的候选路径只认 `target\release\`、`target\debug\` 和 `bin\`,
所以脚本会额外拷一份到 `bin\`,保证"编完一定找得到"。

### 工具链现状(2026-10-15 装好并验证)

| 项 | 值 |
| --- | --- |
| rustc / cargo | 1.99.0 |
| 默认 host | `x86_64-pc-windows-gnu`(无 MSVC 链接器,用 `C:\mingw64` 的 gcc 8.1.0 链接) |
| RUSTUP_HOME | `C:\Users\69544\.rustup` |
| CARGO_HOME | `C:\Users\69544\.cargo`(已加进用户 PATH) |
| 工具链镜像 | 用户级环境变量 `RUSTUP_DIST_SERVER=https://rsproxy.cn`、`RUSTUP_UPDATE_ROOT=https://rsproxy.cn/rustup` |
| crates.io 镜像 | `C:\Users\69544\.cargo\config.toml` 里 replace-with `rsproxy-sparse` |

装的时候踩过的坑,记下来免得重来一遍:官方源 `static.rust-lang.org` 直连只有 ~44 KB/s,
而 `rsproxy.cn` 能跑到 ~1.5 MB/s;清华/中科大/阿里云的 `channel-rust-stable.toml` 还是旧版。
另外网上流传的安装器地址有一半是 404,下到 **153 字节**的文件就是 404 网页 ——
真安装器约 **14.0 MB**,下完请验:`MZ` 开头 + SHA256 对得上官方 `.sha256`。

不想用国内镜像了,删掉那两个用户级环境变量即可。`config.toml` 已备份为 `config.toml.bak-rustup前`。

### 3. 独立自检(不经过 DSH)

```powershell
npm run selfcheck
```

四个阶段:

1. **插件契约** —— 假 Context 加载 `lib/index.js`,18 个工具全注册、契约完整。
2. **二 A 工具闭环** —— 引擎指向 `scripts/mock-engine.mjs`(一个只讲协议、不做任何 Win32 调用的假引擎),
   **走真实工具路径**打一遍:`win_engine_status`/`win_list_windows`(含过滤)/`win_observe`/
   `win_capture`/`win_find`(命中和未命中两种)/`win_click`/`win_type`/`win_press`/`win_scroll`,
   外加错误码透传和日志转发。这一档不需要 Rust,但它证明的是**桥层和协议**,不是引擎本身。
3. **二 B 真引擎** —— spawn 真 exe 打 `engine_ping` + `win_list_windows`。
   引擎没编译出来时明确报 **SKIP**。
4. **卸载清理** —— 调所有 disposer,确认工具注销、不抛异常、mock 子进程被杀掉。

> 命令行一次性模式(`find` / `capture`)不经过桥层,真机验证在 `npm run acceptance` 里。

### 4. 真机验收(动作真的落到控件上)

```powershell
npm run build:testbed     # 用 gcc 编译靶窗(已经编好了,除非改了 .c)
npm run acceptance
```

`selfcheck` 验的是"协议通不通",这里验的是**动作有没有真的落到目标控件上**,而且判据
不是引擎自己的回报,是靶窗(`scripts/testbed-window.exe`)自己写出来的状态文件 ——
引擎说"我发了",靶窗说"我收到了",两边对上才算过。检查项(全部 0 失败):

- 起靶窗、起真引擎、ping 通、`MonitorInfo.dpi` 是真值(不是 `None`);
- 元素树里认出普通编辑框 / 密码框(`is_password=true`)/ 按钮;
- **`win_find` 自匹配**:拿靶窗整窗截图当模板回搜同一个窗口,得分 > 0.99 且位置=窗口左上角;
  缩过图的模板 + `min_score=1.0` 必走 `found=false` 分支;模板比区域大 → `INVALID_ARGS`;
- **`win_ocr` 端到端**:识别靶窗上的 "OCR TEST 12345"(整条 OCR 路径在子进程里跑,
  跑完引擎进程仍存活);
- **命令行 `capture` / `find`**:`capture` 输出一行 JSON + PNG 落盘;`find` 自匹配退出码 0,
  缩过图的模板 + `--min-score 1.0` 退出码 3 且 stdout 仍是合法 JSON;
- **WM_CHAR 打字真的进了普通编辑框**(含中文,读靶窗状态文件确认);
- 往密码框打字被拒 `PASSWORD_FIELD`,且事后密码框内容长度仍是 0;
- 目标不在前台时**键盘 SendInput 被拒 `UNSUPPORTED`** —— 这条是回归:接线前它会把字
  打进用户正在用的那个窗口;
- 点不在目标窗口上时**鼠标 SendInput 被拒 `UNSUPPORTED`**,该路径在闸门处就返回,
  不会真的点下去;
- 100 轮 `win_observe` + 20 轮 `win_capture` 之后引擎句柄数不增长(实测 delta=0)。

靶窗用 `SW_SHOWNOACTIVATE` 显示,**故意不抢前台** —— 测试工具自己先把前提破坏了,
测出来的结论就不算数。uia 版本单独验:

```powershell
$env:DSH_SCREEN_CONTROL_ENGINE = "$PWD\rust-engine\target-uia\release-uia\screen-control-engine.exe"
npm run acceptance
```

### 5. 挂进 profile

```powershell
npm run link                    # dry-run,先看它要改什么
npm run link -- -Apply          # 真写入(改前自动备份 package.json)
```

然后重启 DSH。用 `win_engine_status` 确认插件加载情况。

---

## 工具

| 工具 | 作用 |
| --- | --- |
| `win_engine_status` | 引擎状态、路径、重启次数、最近错误。**任何 win_* 报错先看它** |
| `win_list_windows` | 列出窗口(hwnd/标题/类名/PID/位置/尺寸/Z-order) |
| `win_capture` | 截图 → 压缩 PNG 文件路径(不走 Base64;可带 `ocr=true`) |
| `win_observe` | 元素树 JSON + 高亮缩略图 + `snapshot_id` |
| `win_click` | 点击(SendInput 真注入,带闸门;显式 uid 或屏幕坐标) |
| `win_type` | 输入文本(非 ASCII 自动走剪贴板,ASCII 走 Unicode 注入) |
| `win_press` | 按键(Enter/Tab/Esc/方向键/F1..F12/单字母数字,可带 ctrl/alt/shift/win) |
| `win_scroll` | 滚轮(delta 按格计,120=1 格) |
| `win_launch` | 拉起进程(不负责等窗口;用 `win_wait` 等) |
| `win_window_state` | maximize / minimize / restore / fullscreen / exit_fullscreen |
| `win_drag` | 拖拽(按下→插值轨迹→抬起,带 1px 微动) |
| `win_drag_path` | **路径绘制**:圆/弧/贝塞尔/任意折线,一次按下走完;弧长匀速 + 1ms 定时器 |
| `win_wait` | 条件等待:等窗口 / 等图(模板匹配轮询)/ 等文字(OCR 轮询) |
| `win_find` | **一次性**模板匹配:告诉模板现在在哪、有多像(不轮询) |
| `win_image_trace` | **图片 → 笔顺**:Otsu 二值化 / 细化 / 提笔顺 / 简化 / 重采样;配 `shot_id` 一条链路直接画 |
| `win_record` | **录制人手鼠标轨迹**(按住左键的段 + 毫秒时间戳),配 `win_drag_path.times_ms` 原速回放 |
| `win_ocr` | Windows 原生 OCR 读窗口/区域(子进程隔离,崩了不影响引擎) |
| `win_web_cdp` | Chrome DevTools Protocol(发命令/列目标/新建标签页) |

动作类工具(`win_click` / `win_press` / `win_scroll` / `win_drag` / `win_drag_path`)都支持
可选的 `verify` 参数:动作前抓基准、动作后轮询比对,返回 `effect.changed`。
这样能区分**"指令投递成功"和"目标真的反应了"**——静默无效(点了没反应)以前只能靠人眼看截图,
现在一次调用就能判定。

`win_capture` 还会返回 `coord.shot_id`;把它传给动作工具的 `shot_id`,**截图里的像素坐标
就能直接当参数用**,引擎按该截图的缩放自动换算成屏幕坐标——不用再手乘"截图 1400 宽 ×1.8286"。

### 命令行一次性模式(0.1.9 新增)

`win_find` / `win_capture` 的能力不经过 DSH 也能用:同一个 exe 支持一次性子命令
(不走命名管道、不启动 server),适合脚本/冒烟:

```powershell
rust-engine\target\release\screen-control-engine.exe --help

# 截图存原尺寸 PNG(不缩放,可直接当模板)
...\screen-control-engine.exe capture --out C:\tmp\shot.png --hwnd 123456 --region 0,0,400,300

# 一次性找模板:stdout 一行 JSON;退出码 0=找到 / 3=没找到 / 2=错误
...\screen-control-engine.exe find --template C:\tmp\tpl.png --hwnd 123456 --min-score 0.95
```

参数与 `win_capture` / `win_find` 一致,坐标同样是屏幕物理像素。细节见 [docs/IPC.md](docs/IPC.md)。

### 坐标约定(重要)

**所有对外坐标都是屏幕物理像素。**

`win_observe` / `win_list_windows` 返回的 `rect` 是屏幕物理像素;`win_click` 的 `x`/`y`
也是。引擎内部再 `ScreenToClient` 换算成目标窗口客户区坐标去投递消息。
引擎启动时声明 `PER_MONITOR_AWARE_V2`,保证 1 逻辑单位 = 1 物理像素。

### 推荐调用顺序

```
win_list_windows          → 拿到 hwnd
win_observe(hwnd)         → 拿 snapshot_id + 元素 uid + 元素树
win_click(snapshot_id,uid)→ 精确点击
win_observe(hwnd)         → 再观察一次,确认效果   ← 不要跳过这步
```

`snapshot_id` 默认 5 秒过期。过期后带 `snapshot_id` 的动作会被引擎拒绝并返回
`STALE_SNAPSHOT` —— 这是故意设计的,防止模型拿着过时信息瞎点。

---

## 安全边界

- **密码框硬拒绝**:`ES_PASSWORD` 样式或 UIA `IsPassword` 命中时,`win_type` 直接返回
  `PASSWORD_FIELD`,一个字符都不会输入。
- **前台/光标会短暂被动**:默认走 SendInput 真注入,注入前会用 `AttachThreadInput` +
  `SetForegroundWindow` 把目标顶到前台(否则按键会打进用户正在用的窗口),注入完把前台
  和光标都还回去;抢不过来就回 `UNSUPPORTED`,不会硬往别的窗口里打。
- **剪贴板会被覆盖**:`win_type` 的 auto 模式遇到非 ASCII 会自动写剪贴板再 Ctrl+V,
  返回值 `warnings` 里会明说;要避免请显式传 `method="sendinput"`。
- **句柄不泄漏**:HDC/HBITMAP/HANDLE 全部 RAII 封装,`Drop` 时释放。
- **进程隔离**:引擎是独立进程,崩溃不影响 DSH;桥层有限流重启(60 秒内最多 3 次)。
  `win_ocr` 的截图+OCR 还在引擎下的短命子进程里跑,那层崩了只返回结构化错误。
- **可卸载**:插件卸载时 `ctx.effect` 触发的 cleanup 会杀掉引擎进程。

---

## 与原始需求书的差异(没有做到的部分)

这些是**如实记录**,不是"以后再补"的场面话:

1. **共享内存零拷贝没做**。Node 读不了 Windows 共享内存(需要原生插件)。
   阶段一改为:引擎把 PNG 写到临时文件,只把**路径**回传 —— 依然避免了 Base64 膨胀,
   但不是零拷贝。
2. **DXGI Desktop Duplication 没做**。阶段一是 `PrintWindow(PW_RENDERFULLCONTENT)` 优先、
   GDI `BitBlt` 兜底。DXGI 需要 D3D11 绑定,放在阶段二。
3. **真 UIA 是可选特性,但已经编译并跑通了**。默认仍走 `EnumChildWindows` 子窗口树
   (快、稳、够用);真 `IUIAutomation` 走 `--features uia --profile release-uia`。
   实测在靶窗上 `source=uia`,元素树从 4 个涨到 10 个(多出标题栏、菜单栏、最小化/
   最大化/关闭按钮)。默认不开的原因:UIA 是跨进程 COM 调用,慢一个量级,而且
   `uiautomation` 0.25 内部有 `unwrap()`,目标进程无响应时会 panic ——
   所以配了 `release-uia` 这个 `panic="unwind"` 的 profile 才敢用。
4. **独立虚拟光标没做**。
5. **还剩一批"没接上"的辅助函数**。第二轮接线后重数,`dead_code` 警告从 27 降到 23。
   这一轮**接上了**的:

   - `NoActivateGuard` 现在真的挡在前面了 —— SendInput 的键盘、鼠标两条路各有闸门
     (键盘要求目标就在前台,鼠标要求那个点没被别的窗口盖住),不合格直接回 `UNSUPPORTED`;
   - `MonitorInfo.dpi` 不再是 `None`,改用 `GetDpiForMonitor`(本机 144);
   - `EngineError::Unsupported` 现在会被构造(就是上面那两道闸门)。

   还没接的,逐条列清楚:

   - `ipc::send_event` **仍然没被调用,而且这一轮决定不接**:引擎日志走 stderr 已经端到端
     通了(`bridge.ts` 读子进程 stderr 转进插件日志,`selfcheck` 有断言在测),再开一条
     命名管道事件帧是同一份信息的第二条路,只会多一个可能不一致的来源。函数留着,
     是给以后"引擎主动推送、与请求无关"的事件用的;
   - `EngineError::Timeout` 仍然从不构造 —— 引擎侧的等待没有超时上限,超时是 TS 桥掐的;
   - 阶段二备用(`dpi_of_window`、`monitor_dpi_for_window`、`monitor_from_point`、
     `z_order_of`、`count_descendants`、`WindowKey`/`key_of`、`element_screen_rect`):
     写好了但还没接到工具路径上。

   这一轮还**删掉**了 `window::is_password_edit_from_class` —— 它按类名把 "Edit" 一类
   全判成密码框,是错的(见下)。

   ⚠️ **密码框硬拒绝**依然是投递前的最后一道(`input.rs` 调 `window::is_password_edit`),
   它排在两道新闸门**之前**:先看是不是密码框,再看能不能投。

### 人工通读 + 编译时发现并修掉的问题

通读阶段(编译前):

- `WindowFilter.title` 与工具参数 `title_contains` 对不上 → 过滤会静默失效,已统一;
- `PingResult.dpi_aware` 是 bool,协议要求字符串;`monitor_count` 缺失 → 已补;
- `MonitorInfo.primary` 与协议 `is_primary` 不一致、缺 `index` → 已统一;
- `win_list_windows` 的 `limit` 参数引擎侧根本没实现(mock 实现了,真引擎会忽略)→ 已补。

第一次真正 `cargo build` 时暴露的(11 个错误,已全部修掉):

- `build.rs` 生成 `#![windows_subsystem]` 再 `include!` 注入 → 内层属性被插到文件中部,
  rustc 报"an inner attribute is not permitted in this context"。改成 main.rs 自己写
  `#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]`;
- `PrintWindow` 在 windows-sys 0.61.2 里**被归到了 `Win32::Storage::Xps` 命名空间**
  (win32metadata 的归类)。为一个函数开整个 XPS feature 不划算,改为按官方签名自声明
  `unsafe extern "system"`;
- `BOOL` 不在 `Win32::Foundation`,在 `windows_sys::core`;
- `ClientToScreen` / `ScreenToClient` / `EnumDisplayMonitors` / `GetMonitorInfoW` 在
  `Graphics::Gdi`,不在 `WindowsAndMessaging`;`MONITORINFOF_PRIMARY` 反过来;
- `HMONITOR` 是 `*mut c_void`,不是 `isize`;
- `GetWindowThreadProcessId` 在这份绑定里**只有两个参数**,线程 ID 是**返回值**(两处);
- `capture::finalize` 里 `raw.backend` 在 `scale_if_needed(raw, ..)` 之后才读 → use-after-move。

构建脚本(`scripts/build-engine.ps1`)也修了三处:

- PS 5.1 下 cargo 的 stderr 被当成致命错误,**第一条 warning 就打断脚本** →
  改用 `Start-Process` 的 Win32 级重定向(顺带干掉了 `NativeCommandError` 噪音);
- 没检查 gcc 是否在 PATH → 现在会从常见 MinGW 目录自动补,找不到才报错退出 2;
- 末行提示引用了不存在的 `scripts\smoke-test.mjs` → 改成 `npm run selfcheck`。

**还有一个只有编译完才会暴露的坑**:Cargo 包名是 `dsh-screen-control-engine`,而 Cargo 默认
拿包名当二进制名,产物就叫 `dsh-screen-control-engine.exe` —— 但 TS 桥的 `ENGINE_EXE`
常量找的是 `screen-control-engine.exe`。不修的话引擎**永远找不到**。
已在 `Cargo.toml` 显式加 `[[bin]] name = "screen-control-engine"`。

---

### 第二轮(接线 + uia + 真机验收)发现的坑

这四个都不是"写错一个符号",是**只有真跑起来才会露头**的那种:

1. **`uiautomation` 0.25 的 `Variant` 是结构体,不是枚举**。`Variant::Bool(v)` /
   `Variant::I4(h)` 这种写法编译不过 —— 那两个变体在 `Value` 枚举上,`Variant` 只是
   windows `VARIANT` 的包装,还得再 `get_value()`。改用库自带的 `element.is_password()`
   和 `element.get_native_window_handle()`,反而更短。
2. **UIA 路径下每个编辑框都会被判成密码框 —— 等于把打字功能整个关掉。**
   `window::is_password_edit_from_class` 按类名兜底,而类名叫 "Edit" 的控件绝大多数不是
   密码框。接线后实测:普通编辑框 `is_password=true`,`win_type` 一律回 `PASSWORD_FIELD`。
   改成"以 UIA 的 `IsPassword` 为准,取不到再用窗口样式(要有 HWND)兜底",并删掉那个函数。
   这个坑编译过、`selfcheck` 也过,只有真机验收能抓到。
3. **uid 路径的 SendInput 点击会点在"光标当前所在的位置"**。`resolve_target` 在 uid 分支
   返回 `screen_point: None`,而降级路径直接把这个 `None` 传下去,于是"点某个元素"
   变成了"在鼠标现在待的地方点一下"。已补算屏幕坐标,并加"这点必须真落在目标窗口上"的闸门。
4. **`panic="abort"` 配 `uiautomation` 内部的 `unwrap()` = 引擎猝死**。`release` profile 是
   `panic="abort"`,`catch_unwind` 在那里形同虚设。新增 `release-uia` profile
   (`panic="unwind"`),并在 `uia::observe` 里夹住 panic,让"某个窗口查不动"退化成
   一次失败的调用 + 回退子窗口树,而不是整个引擎消失。

5. **PowerShell 5.1 的 `Set-Content -Encoding UTF8` 会写 BOM,而 profile 的 `package.json`
   被 DSH 用 Node 的 `JSON.parse` 读 —— 一个 BOM 就能让 DSH 启动直接崩**
   (2026-10-16 实测:`Unexpected token ''`)。`link-into-profile.ps1` 原本就是这么落盘的,
   同一份文件由官方安装器(走 Node)写出来是 LF、无 BOM,对照组一摆就清楚。现在改成
   `[System.IO.File]::WriteAllText(..., UTF8Encoding($false))`,并保留原文件的换行风格。
   比 bug 本身更值得记的是**验证方式本身是瞎的**:回读用 `Get-Content | ConvertFrom-Json`,
   而 PS 会自己吞掉 BOM,于是"回读验证通过"和"文件带 BOM"能同时成立。现在先查头 3 字节,
   再交给 `node -e` 的 `JSON.parse`(DSH 用的就是那个解析器)。注意这份 `.ps1` 自己**必须**
   带 BOM,否则 PS 5.1 按 ANSI 解码,中文注释里的引号会被当成尾字节吃掉、整个脚本解析失败。

验收工具自己也踩了两个坑,一并记下(免得下次再花时间):

- 靶窗用 `CreateWindowExA` 配 UTF-8 字节写中文标题 → 按系统 GBK 代码页变成乱码,
  脚本再也搜不到这个窗口。现在界面文字一律 ASCII、脚本按类名找;
- 靶窗每次覆盖写状态文件 → 读的人正好撞上"已截断、还没写完"的窗口,读到空文件,
  报出一个跟被测代码毫无关系的假故障。改成先写临时文件再 `MoveFileEx` 原子替换。

---

## 已知风险

- `windows-sys` 的 feature 列表和函数签名**已过编译器**,但只在
  `x86_64-pc-windows-gnu` + windows-sys 0.61.2 这一个组合上验过;换 windows-sys 版本要重来。
- `uiautomation` 是可选依赖,开启 feature 后才有 UIA 路径。这条路径已编译并跑通
  (`--profile release-uia --features uia`),但它的**正例只在靶窗上验过**,
  没在 Chrome/Electron/WPF 这类真实复杂窗口上验过。
- **SendInput 键盘路径的"正例"没法自动化验证**:正例要求目标窗口就是前台窗口,而让它到前台
  就得抢用户焦点 —— 那正是这个插件明令不做的事。所以自动化只验了"不在前台时拒绝",
  正例得人工把目标窗口切到前台后再跑。鼠标路径同理(正例会真的移动用户的鼠标)。
- `PostMessage` 投递点击对 Chromium/Electron/游戏引擎常常无效,只能靠 SendInput 降级 ——
  这是 Windows 的固有限制,不是本插件的 bug。
- SendInput 会真的移动用户鼠标。降级路径只在 PostMessage 失败后触发,但仍会打断用户操作。
