# dsh-plugin-screen-control · 工具手册

Windows 桌面操作插件：TypeScript 薄壳（`lib/index.js`）+ Rust 常驻引擎
（`screen-control-engine.exe`，NDJSON over 命名管道）。本文件是 16 个 `win_*` 工具的
契约说明：**输入、成功输出、失败返回、超时行为**，以及必须知道的坑。

- 引擎版本：`0.2.4`（2026-10-25 构建；`cargo test` 61 passed / 0 failed / 1 ignored；
  `selfcheck` 0 失败 0 跳过；已部署到 `target/release/`）
  - `0.2.4`：新增 **`win_record`**（§2.3f）+ `win_drag_path.times_ms`——录人手鼠标轨迹
    并**按原速重放**（"看一遍人怎么画，然后一模一样再画一遍"）。
  - `0.2.3`：新增 **`win_image_trace`**（§2.3e）——图片 → 笔顺（Otsu 二值化 /
    Zhang-Suen 细化 / 提笔顺 / 断线接回 / 简化 / 等距重采样），配 `shot_id`
    一条链路直接画：**模型不再需要自己看图报坐标**。
  - `0.2.2`：**截图坐标自动换算**（§2.3d）——`win_capture` 返回 `coord.shot_id`，
    动作工具传 `shot_id` 后所有坐标按那张截图的像素解释，不用再手乘缩放系数。
  - `0.2.1`：动作工具加 **`verify`**（§2.3c）——动作前后自动比对画面/窗口，交出
    `effect.changed`，把"报成功其实没反应"的静默失败堵住。修 **`win_capture` 传
    `hwnd`+非零起点 `region` 时截错区域**的 bug（PrintWindow 只认整窗，旧实现直接
    拿了窗口左上角；连带 `verify` 的 diff 恒为 0）。
  - `0.2.0`：新增 **`win_drag_path`**（§2.3b）：一次按下沿整条路径走完，支持圆/圆弧/贝塞尔/
    任意折线/多笔，弧长匀速 + `timeBeginPeriod(1)`；真机实测一次调用画完 501px 圆弧只要
    627ms、径向误差均值 1.03px。修 `--log-level` 在 `--pipe`/find/capture 三种模式下被
    误判成"未知参数"直接退出的 bug（`scripts/dev-probe.mjs` 因此恢复可用）。
  - `0.1.9`：新增 `win_find`（一次性模板匹配，§2.7）；同 exe 新增 `find` / `capture`
    命令行模式（外部脚本用，§2.7 末尾）；`win_ocr` 改走**隔离子进程**（整屏 OCR 不再可能
    把引擎本体打崩，代价 +80~150ms，2026-10-18 整屏实测通过）；`win_type` 遇非 ASCII 自动
    写剪贴板 + Ctrl+V（覆盖剪贴板并 warning）；`win_press` 描述补全；`docs/IPC.md` 补全。
  - `0.1.8`：`win_press` 改走**硬件扫描码**注入（`wVk=0` + `KEYEVENTF_SCANCODE`），
    按下/抬起之间保持 50ms（原来塞同一批 SendInput＝0ms，逐帧轮询的游戏会整键漏掉）。
    `cargo test` 27 passed / 0 failed / 1 ignored（新增 4 条扫描码单测）；
    `node scripts/acceptance.mjs` 0 失败 / 1 跳过（提权时跳过键盘闸门；新增
    「win_press 后靶窗 keys 增长」检查）；`npm run selfcheck` 全绿。**游戏内已验证通过**
    （2026-10-17 20:57 绝区零实测：F2 打开「日程」面板）。
  - `0.1.7`：`win_list_windows` 新增 `process_name` 进程名过滤（第 0 节就是为它写的）。
  - `0.1.6`：新增 6 个工具（第 2 节）。
- **引擎路径以 `win_engine_status` 报出的为准**：`bridge.ts` 的候选顺序是
  `DSH_SCREEN_CONTROL_ENGINE` 环境变量 → `rust-engine/target/release/` → `rust-engine/target/debug/`
  → `bin/`。**`bin/` 只是最后兜底**，`target/debug` 会压过它——真机实测报出的就是
  `target/release/screen-control-engine.exe`。所以要换版本，得覆盖 `target/release/`（`bin/` 里那份
  现在与它字节一致、时间戳相同，但排在后面，改它没用）。
- 坐标：一律**屏幕物理像素**（不是逻辑像素，不受 DPI 缩放影响）
- 首次报错先看 `win_engine_status`

### 🔒 每轮收尾：压缩归档（强制，每次用完 `win_*` 都要做）

**每次屏幕操控任务结束，同一轮内把这次过程压成一条，追加到下面的「操作流水」。**
不许攒到"下次复盘"，也不许只在回答里说、不落盘——不落盘等于下次重新踩一遍。
有新坑点 / 新路径 / 更快的链路时，**同时**更新文首「遗留问题」。

- 一次 `edit` 追加，**禁止整文件 `write`**；改前先备份（备份名如实写清内容）。
- 一条一行、最多 3 行：`时间 | 任务一句话 | 结果 | 实测耗时 | 新坑点（没有就写"无"）`。
- 只写**能让下次更快**的东西：新坑点、被推翻的旧结论、新测到的路径与耗时基线。
  原始 JSON、截图路径、逐步回读数值、报错堆栈一律不抄——抄了就是给以后每一轮加钱。
- 没有新信息也要留一行（用来攒耗时基线），但**同一个结论不许写第二遍**。
- 「操作流水」最多 10 条，超了删最旧一条（保持文件不膨胀）。

**操作流水**（最新在最上面）

- 2026-10-25 22:45 | 查「滚网页一滚到底」：`win_scroll` 单位错位，改工具描述 | 成功 | ~15min | ①**delta 单位是「格」不是 Windows 原始增量**（引擎 `notches = delta.clamp(-100,100)`，每格发一个 ±120 wheel 事件），实测 -3 → 记事本顶行 001→010 即**位移 9 行**（Windows 默认一格 3 行）；②旧描述写「一格 120，-360 = 3 格」，模型传 -360 被引擎当成 **360 格**、clamp 成 100 格，长网页直接冲到底、中间内容全跳过——**描述和实现的单位差 120 倍**；③**大 delta 自己也不可靠**：一次 SendInput 塞 100 个 wheel 事件，记事本只生效 28 行（≈9 格）且耗时 463ms（3 格仅 42ms），应用会丢事件；④已改 `src/index.ts` 描述为「单位是格，一次 1~5 格，别传 120 的倍数」，**改动要重启 dsh web 才对模型生效**；⑤排查手法：开记事本 + 200 行文本滚完比对顶行行号，比滚用户正在用的浏览器安全。

- 2026-10-25 22:40 | npm 网页建 granular token 并发布 dsh-qq-gateway@1.0.0 | 成功 | ~45min | 坑点：① 令牌页 `/tokens/granular-access-tokens/new` 直达/刷新会白屏只显示列表，须从 Access Tokens 点「生成新令牌」并等 hydration（约 5s）；② 表单校验失败会整页重置，**Bypass 2FA 勾选会丢**，必须最后勾且勾完别再动其他控件；③ 只勾 bypass 不够，Packages and scopes 不选包范围报 "must select at least one package"，Organizations 选了 Read and write 但无组织报 "at least one organization"，要改回 No access；④ 用 token 发布前先 `curl -H "Authorization: Bearer <t>" https://registry.npmjs.org/-/npm/v1/tokens` 看 `bypass_2fa`，false 必 EOTP（token 元数据里 scopes name=null 表示全包可写）；⑤ 首次发布出现 `0.0.0-stage` 是 npm staged publishing 占位 stub，1~2 分钟后真实版本才出现在 dist-tags；⑥ 装新包报 notarget 是 npm 本地缓存旧 packument，加 `--prefer-online --cache <新目录>`；⑦ Edge 下 `win_capture` 用 printwindow 正常、bitblt 全白；显式 `ocr:true` 让引擎崩 `0xC0000005` 两次（重启后 shot_id 从头计数，旧 shot 失效），OCR 尽量别用；⑧ DSH 窗口最小化约 2 分钟后会自己弹回前台，点浏览器前若又被盖住就再 minimize 一次。

- 2026-10-25 12:25 | 画图 v4：11 色量化重画（想用更多色阶救渐变），结果**不如 8 色版**，结论已定性 | 失败（有收获） | ~50min | 数据：①11 色（9 个自定义色 + 白）出 165 环 / 43044 px，黑色拦水坝 51s + 灌色 125 次 + 各色重描 51s；②验收：对 11 色目标图平均误差 **19.1**、完全一致 83.2%；对**原图**平均误差 22.5 / 中位 5 / >40 占 14.0%，三项都比 8 色版（17.0 / 6 / 8.3%）差；③根因不是颜色不够，是 **1px 边界的抖动**：白缝 5125px（227 块）与「目标白却画成彩」5386px 双向对等，说明边界整体在斜边上偏 1px；色阶越多边界总长越长，抖动损失盖过了渐变收益；④修法（未做）：RDP 别削台阶角（`RDP_EPS=0` 保留像素角点整数坐标），让铅笔严格踩在边界像素上；代价点数×2、绘制时间×2；⑤**坑：新开 mspaint 实例后自定义色板清空**，得重新逐个灌 9 个色（4 次调用/色：点彩虹+ →双击 hex 框 → 打字 → 点确定）；其中一次静默失败（色没进色板），必须回读采样色板行核对，别信"点过了"；⑥`win_window_state restore` 对**已关闭**的 mspaint 返回"窗口不存在"，要 `win_list_windows{process_name:"mspaint"}` 先确认；⑦细到无法灌色的色块（dt<3，本例仅 0.6%）生成脚本 `Temp/repair_smalls.py`，沿主方向排线后 1px 笔补画。
- 2026-10-25 01:15 | 画图 v3：8 色量化 + 色块边界环（marching squares 像素边）+ 油漆桶灌色，新建 610×610 画布重画女仆像 | 成功 | ~45min | 数据：①247 个边界环 / 48629 屏幕像素，1px 铅笔描一遍 63s；灌色 115 次（只填离边界 ≥3px 的色块，覆盖 98.5% 面积）约 40s；再按各色重描一遍 63s；最终对 8 色目标图**平均误差 11.6、90.2% 像素完全一致**（旧扫描线法 74.8）；②**坑：描边色绝不能等于任何灌色色**——同色线会被桶顺着穿过去，第一次用 #F1F2FB 描边时灌色把整片头发染浅（命中率剩 10%）；正解＝第一遍全部用**黑色**描边当拦水坝，灌完色再按各色块重描一遍；③**坑：灌色种子必须离边界 ≥3px**（plan 里存了 `d`），1px 坐标误差就会点到隔壁色块、把大块染错；④画布坐标标定：mspaint 100% 时 **x 轴 1:1（origin 975）、y 轴有 0.3% 压缩**（`y=596.72+0.99714×doc`），标定法＝点 4 个已知文档坐标的像素→截图取色反算，两轴分别拟合；⑤`win_image_trace` 直接走工具会把整份 strokes JSON 灌进上下文（93KB 溢出），批量请求一律走 `dev-probe.mjs --file`；⑥引擎只认 png，jpg 参考图先转。

- 2026-10-25 02:40 | 画图：照干净线稿（610×610 黑白 jpg）重勾，1.5× 放大落笔 | 成功 | ~10min | 数据：①**有干净线稿时别再走分区抠图**：`gray<140` → Zhang-Suen → 追踪 → 平滑/RDP 1.0/2px 重采样，451 段并成 78 笔 / 3941 点；②线宽用距离变换取每笔中位数自动分档（w≥4 画 7px、其余 4px），比人工指定准；③1.5× 放大（base (214,434)，610→915）后 7px 笔 ≈ 原图 4.7px 线，视觉几乎一致；④80 个请求 20s；⑤这一版没有双线、糊线问题，说明之前所有畸形都是「从彩色插画反推线稿」造成的。

- 2026-10-25 02:05 | 画图：按部位分层重勾（头发/皮肤/头饰白分区 → 交界处只由前景出线）| 成功 | ~20min | 数据：①做法：颜色分块 + 线稿像素按最近邻归属 → 每块取「与更低优先级相邻」的边界，优先级 头发5 > 头饰白4 > 皮肤3 > 背景0，交界处自然只留一条线（刘海压脸只出刘海下沿）；②**坑：BFS 补标时不能把背景当未标注**（`label==0` 同时代表背景会让整张背景被染色，边界变成贯穿全图的直线）；③**坑：背景要用 alpha 判**（`alpha<20`），不能靠亮度；④区块内部细节线沿用原骨架，但离边界 3px 内的丢掉，否则和边界重复成双线；⑤分区后眼睛不再出现双圈，头/脸交界只剩一条线；仍残留「两个物件各自边缘贴得近」的平行线（头饰与头发），后续可做平行线合并；⑥单画头发时，眼睛会被算成「头发块」（虹膜是蓝的）→ 用皮肤 bbox 把整条线包在脸内的丢掉，27 笔 9 秒即可只出头发的轮廓与压脸的那条边。

- 2026-10-25 01:40 | 画图：换高清参考图（610×610 带 alpha）重抠线稿 + 1.4× 放大落笔 | 成功 | ~15min | 数据：①参考图是 RGBA、底色 34% 透明，`convert("RGB")` 会变纯黑 → 必须 `alpha_composite` 到白底再处理；②**实心暗块（蝴蝶结）不能直接细化**（会变成中轴线），先 `ink & ~erode(ink,2)` 取「壳」，再 Zhang-Suen，实心块就变成外轮廓；③每条描线用距离变换取原图墨线粗度中位数，按粗度自动分档（3px→画 4px 笔、5px→7px 笔），比按长度/按区域猜准得多；④浅粉眉毛不满足暗阈值，要单独按色相补进 ink（限定眉毛 y 带，避免把腮红算进来）；⑤**画的时候按 1.4× 放大**（基准点 + 坐标×1.4）：笔宽不变但相对变细，细节不再糊成一坨；108 笔 27s。

- 2026-10-25 01:10 | 画图：按用户指定四档线宽重画（眉毛 16 / 脸外框+眼眶 11 / 蝴蝶结+衣领 7 / 外形 4）| 成功 | ~10min | 数据：①**画笔粗细完整阶梯**（滑块 x=48）：y≥862→2px，838~858→4px，835~837→7px，822~834→8px，802~821→11px，796→16px，再上 23/41/51px；点轨道跳档、同档内相邻 y 不变，每档必须画一笔回读行数确认；②粗细分档靠**手标描线编号**最靠谱：把 60 条折线画成彩色+编号图（`debug_idx.png`）→ 看图定分类 → 写死索引集合，比按长度自动分类准；③4 档 64 个请求 14s，与单档几乎同价；④粗档再往上刻度很密：y 789~796 都是 16~17px（几乎没差），要明显更粗得继续往上到 23px 档。

- 2026-10-25 00:55 | 画图：按线宽分三档重画线稿（`win_drag_path` 三批 + 中途改笔宽）| 成功 | ~12min | 数据：①**画笔粗细滑块实测阶梯**（1298×1609 窗口、滑块 x=48）：y≥862 → 2px，838~858 → 4px，822~834 → 8px，再往上迅速 23/41/51px；点轨道能跳到位，但同一档内相邻 y 得到同一档，必须**每档后画一笔回读行数**验证；②分档画法：按线长排序，≥230 外形 8px、90~230 主结构 4px、其余细节 2px，每档先点滑块再一次批量发折线，60 笔 / 63 请求 / 14s；③粗轮廓+细细节的层次一出来就明显，比单档好看得多；④裁剪交付按页面原点换算：原点屏幕 (144,324) → 图内 (153,324)，图内坐标 = 屏幕 + 9。

- 2026-10-25 00:35 | 画图：清屏后只用纯黑线稿勾外形（参考图抠线→细化→追踪折线→60 笔 `win_drag_path`）| 成功 | ~9min | 数据：①清屏 `Ctrl+A`+`Delete` 后回白，白页 bbox 屏幕 (144,324)–(1135,1487)，对 991×1163 文档正好 1:1（窗口 1298×1609，截 1:1，屏幕=图+(-9,0)）；②线稿链路：暗像素阈值（灰<85 且非底色档）+ Zhang-Suen 细化 → 3 遍滑动平均 → RDP 1.15 → 2.4px 重采样，2× 放大后 60 笔 / 2930 点；③**坑：numpy 版细化的邻居必须按 P2..P9 顺时针摆**（用「shift 序」直接切片会连错环，80 轮不收敛、线稿五官全丢）；④**实测 60 笔 `win_drag_path` 13s（≈4.4ms/点）**，比扫描线快一个量级，且没有横缝；⑤**坑：`Ctrl+Shift+S` 另存为默认类型是「Microsoft 画图项目文件 (*.paint)」，手输 .png 会得到 HEIF 容器（`ftypmif1`，PIL 打不开）**；「保存类型」下拉点击 / `Alt+Down` / `F4` 都打不开列表，且该对话框 UIA 树里有 `is_password:true` 节点 → 引擎拒绝 `win_press`（报「祖先含 ES_PASSWORD」），另存 PNG 这条链本轮未打通，交付改用截图裁剪；⑥3px 笔在 1:1 截图里就是 3px 宽、满不透明（回读 (0,0,0)），黑块在窗口图 (575,137)。

- 2026-10-25 00:20 | 画图：复核「颜色只能从 20 色板挑」这个旧结论 + 打穿自定义 RGB 全流程 | 成功 | ~15min | 数据：①**旧结论作废，画图能填任意 RGB**：点色板右侧彩虹「+」（本轮到 `maid_paint.png` 窗口 rect(212,190,1733,1280) 时是屏幕 (1437,360)）→ 弹「编辑颜色」→ 它是 WinUI3 ContentDialog，**画在主窗口内部**，`win_list_windows` 找不到独立 hwnd，靠整窗截图定位；②`win_ocr region` 一次就拿到 `#hex`/`RGB`/`红色`/`确定` 的文字矩形（比截图量尺准），点 (1250,483) 进 hex 框、点 (844,1266) 确定；③**坑：hex 框必须先 `Ctrl+A` 再打字**，不选全直接追加会被校验判非法、整串字符静默丢弃（`win_type` 仍报 delivered，截图才知道没进去）；④实打 `F7A8C4` → 确定 → 黑底画一笔 → 回读像素 **(247,168,196) 纯色 225px**（仅边缘几个抗锯齿杂色），自定义色精确到字节；`Ctrl+Z` 撤销后回读纯黑，(0,0,0)×8400，画布已复原；⑤**画布 991×1163 只显示 402×362 = 缩放 27%**，3px 屏幕笔 ≈ 7 文档像素，这才是"像素感 / 线太粗"的根因，不是笔宽设定；⑥键盘在 mspaint 里全局有效（`Escape` 能关弹窗），只有 hex 框的"追加"被业务校验拦下。
- 2026-10-25 01:20 | 引擎 0.2.4：`win_record`（录人手鼠标轨迹）+ `win_drag_path.times_ms` 按原速重放 | 成功 | ~40min | 数据：①另一进程注入 1200ms 圆弧，录制到 **1 笔 / 109 点 / 时间戳 0→1198ms**，起点 (475,1268)、终点 (645,1268)，与注入路径端点完全一致；②重放 `timed=true, planned_ms=1198, elapsed_ms=1224`（+2.2%），`verify` 报 `changed=true`；③实现用 `GetCursorPos`+`GetAsyncKeyState` **轮询 120Hz** 而不是 WH_MOUSE_LL 钩子：钩子要自起消息泵、回调里做重活会被系统摘掉，而引擎是单线程 tokio；④`times_ms` 存在时**必须跳过重采样**（时间戳按原始点一一对应，重采样改了点数就对不上时间），且只有部分笔有时间戳时整段退回匀速；⑤录制阻塞引擎是刻意的——录的时候不该有自己的注入污染轨迹；⑥验收脚本偶发 1 项失败：testbed 窗口被别的窗口压住时 `win_find` 自匹配得分从 1.0 掉到 ~0.98（位置仍对），重跑即过——跑 acceptance 前先把无关窗口挪开或最小化。
- 2026-10-25 00:45 | 引擎 0.2.3：新增 `win_image_trace`（图 → 笔顺），跑通"截图→追踪→一笔画出"整链 | 成功 | ~60min | 数据：①mspaint 1044×750 画布 11 笔涂鸦 → 31 条笔顺 / 1575 点 / **12ms**；手画圆识别成 1 条 `closed=true length_px=442.99`（理论 439.8，误差 0.7%），圆心 (233,528) 与实际一致；②全链实测：trace 拿笔顺 + `shot_id` → 平移后 `win_drag_path` 226 点 **568ms** 一笔画完，`verify` 报 `changed=true diff 2.54%`；③算法坑（都靠单测抓出来）：Otsu 在"白底少量纯黑线"上算出的阈值是 0，判据必须用 `v <= 阈值`（用 `<` 会把整条线判成背景）；按"度数切节点"提笔顺会把一个圆切成 64 段，改成**贪心+惯性**走法后又断成 5 段弧，最后靠"端点≤3px 接回 + 80% 重叠的碎片丢弃"才成 1 笔；④**`win_press "b"` 选不了画图里的画笔**（两次都无效，之前记的"键盘能切工具"是错的），切工具只能点图标：1298×1609 窗口下画笔在窗口内 (333,169)；⑤点工具栏坐标猜错会静默变成"选择"，拖拽只会拉选框，`verify` 报的 changed 是选框不是墨迹——**切换工具后必须截图回读高亮**。
- 2026-10-25 00:20 | 引擎 0.2.2：`win_capture` 返回 `coord.shot_id`，动作工具支持 `shot_id` 自动换算截图坐标 | 成功 | ~45min | 数据：①mspaint 窗口 1298×1609、截图 1291×1600（scale 1.00542/1.00562），`shot_id + (300,145)` 点击实测落到屏幕 `(293,146)`、`points [[350,700],[450,800]]` 画线实测落点 `(343,704)`–`(443,805)`，与公式一致；②登记表 10 分钟 TTL / 上限 32 张，过期报 `INVALID_ARGS` 并提示重截；③x/y 两轴**分别**换算——缩放按长边等比，两轴系数可能差千分之几，一个系数套两轴会在图边缘累积出几像素偏移；④`shape` 先展开成点列再统一换算（半径按两轴平均），保证多笔之间不混用两套坐标系；⑤`shot_id` 只改坐标解释方式，不传就还是屏幕物理像素，老调用零改动。
- 2026-10-24 23:45 | 引擎 0.2.1：动作工具加 `verify`（前后比对画面/窗口）+ 修 `win_capture` 区域截错 | 成功 | ~55min | 数据：①mspaint 点「选择」工具 `changed=true diff 1.397%`、1 次轮询 4ms；**再点同一个已选中的工具 `changed=false diff 0%`**、19 次轮询 1208ms——静默无效第一次能被机器抓到；②`window` 模式点「文件」1 次轮询命中 `Microsoft.UI.Content.PopupWindowSiteBridge`，点画布空白处 1200ms 无新窗口 `changed=false`；③**抓出真 bug**：`win_capture` 传 `hwnd`+非 (0,0) 起点 `region` 时 PrintWindow 后端返回的是窗口**左上角**那一块（PrintWindow 只会整窗渲染，旧实现按 region 尺寸建 DIB 拿的就是左上角），连带 `verify` 的 diff 恒 0；改成「整窗渲染 + 按窗口内坐标裁切」，区域截图已回读确认（220×120 截到画布而非工具栏）；④`pixels` 取样是每 3 像素 1 个、三通道差和 >24 才算变，压得掉合成噪声但压不掉动画，动画类目标必须调大 `threshold`；⑤`verify` 命中即返回，实测只多花 4~10ms；没变化时要等满 `timeout_ms`（默认 1200）。
- 2026-10-24 23:00 | 引擎 0.2.0：新增 `win_drag_path`（圆/弧/贝塞尔/任意折线，一次按下走完）+ 1ms 定时器 + `--log-level` 解析修复 | 成功 | ~70min | 数据：①501px 圆弧一次调用 627ms 画完（计划 601ms，+4.3%），246 点，径向误差均值 1.03 / 最大 3.04px，圆心实测 (799.7,998.6) vs 请求 (800,1000)、半径 +0.19px——比 6 段直弦（1.27/4.18）和 300 点拼接（1.09/3.39）都更圆，而且只要一次工具调用；②圆心/闭合：`circle` 起笔=收笔都在 (300,910)，221 点 565ms；多笔 3 笔 725ms（计划 700），`max_step_px` 已排除笔间飞行（原来会报 166px）；③`timeBeginPeriod(1)` 实测每步 1.0~1.3ms，不开时默认 15.6ms 一档、200 步弧要 3 秒；④**DSH 窗口最小化后会被自己弹回前台**（Electron 行为），必须把 minimize 和绘制放进同一条 pwsh 里，中间不能插任何工具调用，否则 `ensure_point_reaches` 一律判"被 DSH 盖住"拒绝执行；⑤`--log-level` 在 `--pipe`/find/capture 三种模式里被 `extract_log_level` 取走后没跳过 → 报"未知参数"直接退出，这才是 `dev-probe.mjs` 起不来引擎的原因（不是脚本的坑），cli.rs 三处已修；⑥解锁二进制要先杀掉正在跑的引擎（`Stop-Process`），否则 `cargo build` 报 os error 5。




---


## ⚠️ 遗留问题（先看这里）

1. **`win_type` 中文串位**：0.1.9 起 `method:"auto"` 遇非 ASCII 自动改走**剪贴板**
   （引擎自己写 `CF_UNICODETEXT` + Ctrl+V，会覆盖剪贴板并在 warning 里说明）；显式
   `sendinput` 仍是原来的 `KEYEVENTF_UNICODE` 注入（长中文串仍可能串位，别用）。实测中文精确落盘。
2. **中文输入法开着时按键可能被 IME 吞掉**：目标在 IME 组合状态时收到的是
   `VK_PROCESSKEY (0xE5)`，`win_press` 发真扫描码也一样。**未修**，绕法：先切英文输入法。
3. `win_web_cdp`：**已真机验证通过**（2026-10-16，Edge 155 无头独立实例，
   `node scripts/cdp-selftest.mjs` → 10/10 项）。验证时抓出并修掉一个真 bug：
   引擎原来用 `read_to_end` 读 HTTP 响应，而 Chromium 收到 `Connection: close` **也不关连接**，
   于是 `list_targets` 把一份完整的 200 响应报成 `ENGINE_ERROR` 超时（真机报 os error 10060）。
   现改为按 `Content-Length` / chunked 分帧读取，`cdp::tests` 里有用假服务器的回归测试。
   边界：验的是「本机 Edge + 9222 + 独立 profile」这条链路；Electron 或别的端口同理但没逐一验过。
4. ~~`docs/IPC.md` 的方法表还没同步~~：0.1.9 已补齐（16 个方法含 `win_focus_guard_probe`、
   `win_find`、find/capture CLI、OCR 隔离说明、剪贴板语义）。
5. 新工具要**重启 DSH / 开新会话**才会出现在工具列表里（运行中的会话不热加载插件）。
6. 历史遗留测试 `snapshot::tests::expired_is_stale` 已标 `#[ignore]` 并注明原因
   （`snapshot_ttl_ms=0` 契约上不可达，且 `now == expires_at` 的边界判定依赖墙钟跳动）；
   补了一条确定性的 `expired_after_ttl_elapses` 覆盖真实过期路径。测试集现在全绿：
   21 passed / 0 failed / 1 ignored。
7. **`win_launch` / `win_drag` / `win_window_state` 已人工真机验收通过**（2026-10-16 21:26–21:28，
   在新会话里实调，每步都有回读）。验收暴露的真实坑点，**别误判成工具 bug**：
   - `win_launch notepad.exe` 会返回 `still_running=false`：Win11 的 `system32\notepad.exe`
     是跳板，拉起商店版后自己就退（实测 500ms 内退出）。窗口照常出现，读 `warnings` 别当失败。
   - 记事本这类 UWP 窗口的 **hwnd 会被系统重建**（pid 不变、hwnd 变），跨步骤别沿用旧 hwnd，
     每步重新 `win_list_windows` 取。
   - `win_drag` 起点必须落在**真标题栏**上：记事本 y=47 已在标签栏，拖了几乎不动（实测只挪了
     (-77,-9)）；y=35 才精确。拖后 `rect` 位移与预期完全一致（+300/+200，尺寸不变）。
   - `win_window_state` 的 `style` / `ex_style` 是**它自己回读**的，`win_list_windows` 不返回这两个值，
     没有第二个工具能交叉验证：maximize→restore→minimize→restore 的 rect 与 style 变化自洽
     （`0x14CF0000` ↔ `0x15CF0000`(+WS_MAXIMIZE) ↔ `0x34CF0000`(+WS_MINIMIZE)），但 style 属单工具自证。
8. **启动/查找链路的 58 秒事故**（2026-10-16 21:35，模型自己的会话）："打开 QQ 给某人发消息"总耗时 58 秒，
   大半浪费在找应用上——先 `win_list_windows` 枚举 3 次（含 `visible_only:false, limit:300` 全量），
   又用 `ls` 挨个探 `Program Files` / `LOCALAPPDATA`，最后 `tasklist` 全量扫描。**顺序反了**：
   进程都没跑，哪来的窗口可找。规范已写进第 0 节。附带查出两件事：bash 是 MSYS 环境，
   `tasklist /FI` 里的 `/FI` 会被改写成 `.../bash/2.56.0.2/FI` 直接报错（得写 `//FI`）；
   本机 App Paths 里没有 `QQ.exe`，`start "" qq` 定位不到它。
9. **游戏（DirectInput / RawInput / Unity）键盘注入**：**已真机验证通过**（2026-10-17 20:57，绝区零）。
   根因是注入方式，不是权限/时序：`win_press` 原来发虚拟键码（`wVk` 有值、没 `KEYEVENTF_SCANCODE`），
   游戏的键盘层直接丢；改成 `wVk=0` + `KEYEVENTF_SCANCODE`（硬件扫描码）+ 按下保持 50ms 后，
   F2 一次就打开了「日程」面板。判据：`sequence_ms≈51` + 前后整屏截图对比。
   **别再退回 VK 注入或 0ms 同批 down/up**；`keybd_event` 长按也没用（它同样只发 VK）。
10. **DSH 窗口会自己回到前台盖住游戏**：整屏截图/`win_capture` 截到的可能是聊天框（2026-10-18 踩过两次）。
    判据：`win_list_windows` 里 `is_foreground`。做法：先 `win_window_state minimize`（回读确认）→ 采完再 `restore`；
    点游戏坐标前也要确认游戏在前台，否则点击落到 DSH 上。
11. **`.ps1` 运行与编码**：本机没装 PowerShell 7（`pwsh` 不存在），用
    `powershell -NoProfile -ExecutionPolicy Bypass -File xxx.ps1`；脚本必须 UTF-8 with BOM，
    否则 PS 5.1 按 GBK 读中文直接语法错。**`edit`/`write` 落盘会写掉 BOM**，每次改完必须补 BOM，
    再用 `[System.Management.Automation.Language.Parser]::ParseFile()` 验证。
12. **日程面板会停在上次页签**（2026-10-18 实测停在「训练」）：F2 后固定点 (1302,314) 切「日常」
    再等 `panel.daily` 锚点；面板右缘「>」不是卡片翻页，点了没反应。
13. **锚点模板制作法**：`win_capture region=... max_dim=0` 裁出来就是原始像素小图，直接存 `anchors/`
    即可，不用截整屏再剪。裁前先 minimize DSH 并确认游戏前台；模板要避开会变的部分
    （如小红点/数字），只取稳定的 UI 底色+文字。
14. **Edge/Chromium 窗口**：`win_capture` 只能用 `printwindow`（`bitblt` 出全白）；**别开 `ocr:true`**——
    实测连续两次把引擎打崩（`code=0xC0000005`），重启后 `shot_id` 从 1 重新计数、旧 shot 全部失效。
    网页表单操作靠「截图→read_image 量坐标→shot_id 点击」即可，别依赖 UIA（Chrome 只暴露 Legacy Window）。
    另外 DSH 主窗最小化后约 2 分钟会自己弹回前台（Electron 行为），点网页前先 `win_list_windows` 确认
    目标窗口 `is_foreground`，被盖住就再 minimize 一次；`win_click` 的「被…盖住」拒绝就是判据。
14. **引擎 release 是 Windows GUI 子系统**：PowerShell 里 `& exe ...` 拿不到 stdout/退出码
    （`$LASTEXITCODE` 为空），必须用 .NET `ProcessStartInfo` 重定向；bash / node 管道正常。
    `zzz-daily.ps1` 的 `Invoke-EngineRaw` 已按此实现（2026-10-18 脚本自检报"不支持 find"就是它）。
15. **批量输入/画线的正确姿势（2026-10-24 更新）**：DSH 常驻引擎**只服务一个客户端**，
    另开连接会一直超时；要一次发成百上千条请求，用 `node scripts/dev-probe.mjs --engine
    rust-engine/target/release/screen-control-engine.exe --file <请求.json>`（它自己 spawn
    独立引擎实例，改完引擎不重启 DSH 也能自测）。
    **0.2.0 起 `--log-level` 的"未知参数"bug 已修**，`dev-probe.mjs` 恢复可用（旧记录
    "dev-probe 带 --log-level 会打印用法退出"作废）。画弧线**一律用 `win_drag_path`**，
    不再需要自己 spawn 引擎 + 手写 NDJSON 去拼 `win_drag`。
    另：**WinUI3 应用（画图/Win11 记事本）不吃 `win_click` 的 PostMessage**，
    点工具图标没反应，必须用 SendInput——1px 的 `win_drag`（`steps:2`）可当点击用。

---

## 0. 启动与查找互锁（2026-10-16 新增，优先级高于后面所有章节）

**为什么有这一节**：一次"打开 QQ 发条消息"实测 58 秒，大半浪费在找应用上。根因是顺序反了——
进程都没跑就先枚举窗口，找不到又退化成全量枚举和满盘 `ls`。

### 0.1 打开应用：先查进程，再决定启不启动

1. **第一步永远是查进程**，一条命令，不要去找 exe：
   `tasklist //FI "IMAGENAME eq qq.exe"`（`pwsh` 里 `Get-Process qq` 等价）
   - ⚠️ **必须写 `//FI`**：bash 是 MSYS 环境，会把 `/FI` 改写成 `C:/Users/.../bash/2.56.0.2/FI`，
     实测直接报"无效参数/选项"——上次白跑掉的时间之一。
2. **进程在跑 → 不要再启动**，直接进 0.2。
3. **进程没跑 → 让系统去定位，不要遍历目录**，按顺序试、命中即停：
   - `win_launch {path:"cmd.exe", args:["/C","start","","<名字>"]}`：让系统走 App Paths + PATH。
   - App Paths 注册表（**三个位置都要查**）：
     `reg query "HKLM\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\<app>.exe" //ve`，
     外加 `HKCU\...` 和 `HKLM\SOFTWARE\WOW6432Node\...` 同名键。
   - 开始菜单快捷方式：`ls "/c/ProgramData/Microsoft/Windows/Start Menu/Programs"` 按关键字找 `.lnk`，
     再用 `win_launch {path:"cmd.exe", args:["/C","start","","<.lnk 全路径>"]}` 拉起
     （`start` 认 `.lnk`，`win_launch` 本身不认）。
   - 第 0.3 节的已知路径表。
   - 都没有 → **停下来问用户要路径**，不要 `ls -R "/c/Program Files"` 式硬找。
4. 启动后**不要 sleep**，立刻 `win_wait {condition:"window", title_contains:"…"}` 等窗口；
   冷启动那几秒是应用自己的，压不掉。

### 0.2 找窗口：一次过滤到位，别升级成全量枚举

- **`win_list_windows {process_name:"QQ.exe"}`** —— 0.1.7 新增的进程名过滤，一次调用同时回答
  "进程有没有窗口"和"hwnd 是多少"。名字不区分大小写，写 `QQ` 和 `QQ.exe` 等价，
  **不做子串匹配**（写 `edge` 不会命中 `msedge.exe`；真机验过）。
  0.1.6 及以前只有 `pid`：先 `tasklist` 拿 PID 再传 `pid`，一样能用。
- **窗口过滤不等于进程在不在**：它只回答"有没有顶层窗口"。QQ 关到托盘时进程在跑、主窗口只是隐藏，
  得 `visible_only:false` 才捞得到（真机实测：QQ 有 5 个顶层窗口全叫 `QQ`，默认只返回可见的那 1 个）。
- **找不到就换条件，不要退化成 `visible_only:false` + `limit:300` 全量枚举**：一屏几百条既烧 token，
  又不解决"名字记错了"。全量枚举只在"确实不知道叫什么"时用一次，并且带上 `limit`。
- hwnd 会被系统重建（UWP 尤其），**每步重新取**，禁止沿用上一步的值。

### 0.3 最小链路（发消息类任务，目标 ≤15 秒）

`查进程 →（没跑才）启动 → win_wait 等窗口 → win_list_windows{process_name} 取 hwnd
→ win_observe / win_click 定位输入框 → win_type → win_press Enter → win_capture 回读确认`

- 15 秒**不含应用冷启动**：应用已经开着时这条链正常 5–10 秒走完；冷启动那几秒不算在内，
  也不许为了"看起来快"跳过 `win_wait` 去瞎点。
- 只读工具（`win_list_windows` / `win_observe` / `win_capture` / `win_ocr`）可以连着调；
  **每个写动作（click / type / press / drag / window_state）之后必须回读**，没回读不许说成功。
- 本机实测已知路径（省掉重新定位）：
  - QQ：`C:\Program Files\Tencent\QQNT\QQ.exe`。**App Paths 里没有 QQ**（HKLM / HKCU / WOW6432Node
    三处实测都没有，`where qq` 也找不到），所以 `start "" qq` 定位不到它：要么给全路径，
    要么走开始菜单 `腾讯软件\QQ`。
  - Edge：`C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe`（App Paths 里有 `msedge.exe`，
    `start "" msedge` 可用）。
- `process_name` 是引擎 0.1.7 加的，**TS 侧要重启 DSH / 开新会话**才出现在工具签名里（见遗留问题 5）。

---

## 1. 速查

| 工具 | 作用 |
| --- | --- |
| `win_engine_status` | 引擎状态、路径、重启次数、最近错误 |
| `win_list_windows` | 列窗口（hwnd/标题/类名/PID/进程名/位置/尺寸/Z-order），可按标题/类名/`pid`/`process_name` 过滤 |
| `win_capture` | 截图 → PNG 文件路径（可顺带 OCR） |
| `win_observe` | 元素树 + 高亮缩略图 + `snapshot_id` |
| `win_click` | 后台点击（PostMessage 优先，SendInput 降级） |
| `win_type` | 后台输入文本（WM_CHAR / SendInput / 剪贴板） |
| `win_press` | 后台按键（Enter/Tab/Ctrl+A…） |
| `win_scroll` | 后台滚动（WM_MOUSEWHEEL） |
| `win_launch` | **拉起进程**（参数 + 工作目录），不等窗口 |
| `win_window_state` | **最大化/最小化/还原/伪全屏/退出全屏** |
| `win_drag` | **拖拽**（按下 + 轨迹插值 + 抬起） |
| `win_drag_path` | **路径绘制**（0.2.0）：圆/弧/贝塞尔/折线，一次按下走完；画弧线用它 |
| `win_wait` | **条件等待**：窗口出现 / 图像匹配 / OCR 文本 |
| `win_ocr` | **Windows 原生 OCR** 读指定窗口或区域 |
| `win_find` | **一次性模板匹配**（0.1.9）：返回命中框/中心/得分（屏幕坐标） |
| `win_web_cdp` | **CDP 通道**操作浏览器（导航 / 执行 JS / 列标签页） |

---

## 2. 新增 6 个工具（0.1.6）

### 2.1 `win_launch`

| | |
| --- | --- |
| 输入 | `path`（必填，exe 绝对路径或 PATH 里的名字）；`args[]`（逐个传，不要自己拼引号）；`cwd`（必须是已存在的目录）；`no_window`（true 时加 `CREATE_NO_WINDOW`，控制台程序不弹黑框）；`check_ms`（启动后等多少毫秒复核进程是否还活着，默认 30，0=不检查） |
| 成功 | `{pid, path, cwd, still_running, warnings[]}`；`still_running=false` 表示**进程秒退**（路径/参数不对），这不是报错但等于失败 |
| 失败 | `NOT_FOUND`：裸名字在 PATH 找不到 / `cwd` 不存在 / spawn 报 2、3；`INVALID_ARGS`：参数缺失或类型错；`ENGINE_ERROR`：其它 spawn 失败（含权限） |
| 注意 | **只负责拉起，不等窗口出现**——等窗口交给 `win_wait(condition:"window")`。相对路径会给 warning |

### 2.2 `win_window_state`

| | |
| --- | --- |
| 输入 | `hwnd`（必填）；`action`（必填）：`maximize` / `minimize` / `restore` / `fullscreen` / `exit_fullscreen` |
| 成功 | `{hwnd, action, style_before, style_after, rect_before, rect_after, is_topmost, warnings[]}`（样式与 `is_topmost` 都是**回读**出来的，不是"我以为设上了"） |
| 失败 | `NOT_FOUND`：hwnd 不是有效窗口；`INVALID_ARGS`：action 不认识；`ENGINE_ERROR`：Win32 调用失败 |
| 注意 | ① `fullscreen` = 去边框 + `HWND_TOPMOST` + 铺满窗口所在显示器，**会盖住整个屏幕并抢焦点**，测试时别拿它盖用户桌面；② 原窗口样式在**第一次进全屏时**记录（重复调用不覆盖记录）；③ `exit_fullscreen` 按记录还原；引擎重启后记录丢失，只能补回标准边框 + 取消置顶，并在 `warnings` 里说明；④ `minimize` 本身就会丢前台，所以不做前台归还，`warnings` 里写明；⑤ 操作前用 `AttachThreadInput` 稳住焦点，做完把前台还给原窗口 |

### 2.3 `win_drag`

| | |
| --- | --- |
| 输入 | 终点 `to_x`/`to_y`（必填，屏幕物理像素）；起点二选一：`from_x`+`from_y` 或 `snapshot_id`+`uid`；`hwnd`（不用 snapshot 时必填）；`button`（默认 left）；`steps`（轨迹点数，默认 24，夹在 2..200）；`duration_ms`（默认 400，夹在 50..10000）；`wobble`（中间点叠 1px 法向微动，默认 true） |
| 成功 | `{from, to, steps, moved_ms, warnings[]}`；`from` 是实际注入的起点（做过 1px 落点抖动） |
| 失败 | `INVALID_ARGS`：缺起点/起点终点成对参数缺失；`STALE_SNAPSHOT`：snapshot 过期或不含该 uid；`UNSUPPORTED`：抢不到前台，或**起点/终点有一个不在目标窗口上**（落错地方比点错更糟，直接拒绝）；`NOT_FOUND`：hwnd 无效 |
| 注意 | 真 SendInput：会短暂移动真实光标并激活目标窗口（结束后还原光标、归还前台，`warnings` 里报告）。轨迹是"按下 → smoothstep 插值移动（含 1px 微动）→ 精确终点 → 抬起"；**只发"移动+按下+移动+松开"很多控件不认**（Win32 列表视图拖选、Qt 拖放手势、网页 HTML5 拖放都会当没动过） |

### 2.3b `win_drag_path`（0.2.0，**画弧线/曲线用这个，别用 `win_drag` 拼段**）

| | |
| --- | --- |
| 输入 | 三种给法（优先级 `strokes` > `points` > `shape`）：`strokes`＝多笔点列 `[[[x,y],...],...]`；`points`＝单笔点列 `[[x,y],...]`；`shape`＝`{kind:"circle",cx,cy,r,start_deg?,ccw?}` / `{kind:"arc",cx,cy,r,start_deg,end_deg,ccw?}` / `{kind:"bezier",points:[[x,y]×2~4]}`。可选：`from_x`/`from_y`（在路径前插一个落笔点）、`hwnd`/`snapshot_id`+`uid`、`button`（默认 left）、`step_px`（默认 2，夹 0.5..64）、`duration_ms`（不给就按 1.2ms/px 自动）、`wobble`（默认 **false**） |
| 成功 | `{strokes, points, planned_ms, duration_ms, path_px, max_step_px, start_screen, end_screen, timer_1ms, warnings[]}` |
| 失败 | `INVALID_ARGS`：三种给法都没给/路径为空/点数超 20000/有点落在目标窗口矩形外；`UNSUPPORTED`：抢不到前台，或**任一落笔点被别的窗口盖住**；`STALE_SNAPSHOT`/`NOT_FOUND` 同其它工具 |
| 注意 | ①**引擎内部做了三件调用方不用管的事**：等弧长重采样（步距 `step_px`）、时间按累计弧长分配（匀速，不是按参数 t）、动作期间 `timeBeginPeriod(1)`；`timer_1ms=false` 表示没拿到 1ms 精度，时长会明显偏长；②`duration_ms` 与 `planned_ms` 的差就是注入开销（实测 +4~5%）；③`max_step_px` 只统计落笔步、**不含笔间抬笔飞行**；④角度单位为度、0°=右、角度增大在屏幕上是顺时针（y 轴向下），`ccw=true` 反转；⑤`wobble` 打开会在弧线上出毛刺，画图别开；⑥多笔之间会自动抬笔-移动-落笔，笔间移动沿直线飞过去，**不会**在画布上留下墨迹 |

**真机数据（2026-10-24，mspaint 77% 缩放 + 4px 画笔，同一题三条线对比）**

| 画法 | 拟合半径 | 径向误差均值 | 最大 | 标准差 | 耗时 |
| --- | --- | --- | --- | --- | --- |
| `win_drag` 6 段直弦 | 117.82 | 1.27 | 4.18 | 1.55 | 6 次调用 |
| `win_drag` 300 点拼接 | 120.06 | 1.09 | 3.39 | 1.29 | 300 次调用 |
| `win_drag_path` 圆弧（请求 r=156） | 156.19 | **1.03** | **3.04** | **1.23** | **1 次调用 / 627ms** |

圆心实测 `(799.7, 998.6)` vs 请求 `(800, 1000)`，半径偏差 `+0.19px`。闭合圆用
`shape:{kind:"circle"}` 时起笔点＝收笔点（实测都在 `(300,910)`），不需要再补一笔。

### 2.3c 动作生效验证：`verify` 参数（0.2.1）

`win_click` / `win_press` / `win_scroll` / `win_drag` / `win_drag_path` **共用**的
可选参数。**不传 = 不验证**（零开销）；传了就在动作前抓基准、动作后轮询比对，
结果挂在返回值的 `effect` 上。

| | |
| --- | --- |
| 输入 | `mode`：`pixels`（默认）/ `window`；`region`（比对区域，默认动作点 ±160px，夹到窗口内）；`threshold`（默认 0.002，anim 类目标要调大）；`timeout_ms`（默认 1200，上限 10000）；`interval_ms`（默认 60）；`mode=window` 需要 `title_contains`/`class_name`/`pid` 之一 |
| 成功 | `effect: {checked, changed, mode, diff_ratio, polls, waited_ms, region, detail, warnings}` |
| 判读 | **`changed=false` 就是"动作没效果"**——这是本工具唯一要看的字段；`checked=false` 表示没验成（基准图抓不到 / window 模式缺过滤条件），原因在 `warnings`，此时 `changed` 无意义 |
| 注意 | ①`pixels` 用取样比对（每 3 像素取 1，三通道差和 >24 才算变），能压掉合成噪声但**压不掉动画**；②`window` 模式更稳，点开对话框/菜单/弹层优先用它；③动作前该窗口已存在时 `window` 模式固定报 `changed=false`（`detail` 说明原因）；④`win_type` 不在覆盖范围（打字是否有变化建议截图 + `win_ocr` 验） |

**真机实测（2026-10-24，mspaint）**：点「选择」工具 → `changed=true, diff 1.397%, 4ms`；
**再点一次同一个已选中的工具 → `changed=false, diff 0%, 19 次轮询 1208ms`**（这就是要抓的
静默无效）；点「文件」→ `window` 模式 1 次轮询命中 `PopupWindowSiteBridge`；点画布空白处
→ `window` 模式 `changed=false`。

### 2.3d 截图坐标：`shot_id`（0.2.2）

`win_capture` 阈值外还多返回一个 `coord` 块；把 `coord.shot_id` 传给动作工具的
`shot_id`，**该工具里所有坐标字段就按那张截图的像素解释**，引擎换算成屏幕坐标。

| | |
| --- | --- |
| 拿 shot_id | `win_capture` → 结果里的 `coord: {shot_id, hwnd, screen_rect, image_w, image_h, scale_x, scale_y, ttl_ms}` |
| 用 shot_id | `win_click{shot_id, x, y}` / `win_scroll{shot_id, x?, y?}` / `win_drag{shot_id, from_x, from_y, to_x, to_y}` / `win_drag_path{shot_id, points/strokes/shape}` |
| 换算 | `screen = screen_rect.origin + 图片像素 × scale`，x/y 两轴分别算；圆的半径按两轴平均缩放 |
| 生命周期 | 10 分钟、最多 32 张；过期或引擎重启后报 `INVALID_ARGS`，重新截图即可 |
| 注意 | 没给 `hwnd` 时用截图时的窗口当目标；不传 `shot_id` 就还是屏幕物理像素（老行为不变） |

**真机实测**：mspaint 窗口 1298×1609、截图 1291×1600（scale≈1.0054/1.0056），
`shot_id + (300,145)` 点到屏幕 `(293,146)`；`points [[350,700],[450,800]]` 画出的线
实测落点 `(343,704)`–`(443,805)`，与公式一致——**模型再也不需要自己乘 1.8286 这种系数**。

### 2.3e 图像 → 笔顺：`win_image_trace`（0.2.3）

一条命令把图里的线变成能画的笔顺。**这是"临摹/抄线稿"的正确姿势**：不要让模型看图
一个个报坐标（模型只能逐 token 吐点，又慢又歪），坐标交给程序。

| | |
| --- | --- |
| 图源 | `path`（PNG 文件）或 `hwnd`+`region`（自截，**会返回 `coord.shot_id`**，可直接喂 `win_drag_path`） |
| 处理参数 | `threshold`（0~255，不给用 Otsu）/ `invert`（黑影白线）/ `crop` / `min_area`=16 / `smooth`=1.2（DP 容差，0=不简化）/ `step_px`=2 / `max_strokes`=64 / `close_loops`=true / `min_len_px`=8 |
| 输出 | `{stroke_count, total_points, total_px, image_w/h, threshold, ink_px, skeleton_px, dropped_blobs, trace_ms, strokes[{points, length_px, closed}], warnings}` |
| 链路 | `win_image_trace{hwnd,region}` → 拿 `strokes` + `coord.shot_id` → `win_drag_path{shot_id, strokes}` 画出来 |
| 注意 | ①坐标是**图片像素**（有 `coord` 就配合 `shot_id` 用，没有就自己对应到屏幕）；②笔顺已按长度从长到短排序并限量；③`closed=true` 的末点＝首点，直接画就是闭合曲线；④输出点压到 2 位小数 |

**真机实测（2026-10-25，mspaint 1044×750 画布）**：11 笔涂鸦 → 31 条笔顺 / 1575 点 /
`trace_ms=12`；手画的圆被识别成 1 条 `closed=true, length_px=442.99`（理论 2π×70=439.8，
误差 0.7%）。把这条笔顺平移后在空白处重画：226 点、**568ms** 一笔画完、`verify` 报
`changed=true diff 2.54%`。

**踩坑记录**：`win_press "b"` **不能**选中画图里的画笔（试了两次都没反应，之前误判成
"能切工具"）。要切工具只能点工具栏图标，坐标按当前窗口换算：1298×1609 窗口下
「画笔」在窗口内 `(333,169)`（屏幕坐标 = 窗口原点 + 该偏移，本轮 `(324,169)`）。
点完必须截图回读确认高亮，别直接把"点过了"当"切过去了"。

### 2.3f 录制回放：`win_record` + `times_ms`（0.2.4）

录人手：按住左键画一遍，抬键即断笔，每点带毫秒时间戳。然后把 `points` / `times_ms`
原样贴进 `win_drag_path`，**按原速重放**——这是"看一遍人怎么画，再一模一样画一遍"
的落地方式（比让模型看图猜轨迹可靠得多）。

| | |
| --- | --- |
| 录制 | `win_record{duration_ms?=5000, hz?=120, max_points?=20000}` → `{strokes:[{points, times_ms}], total_px}` |
| 回放 | `win_drag_path{strokes, times_ms}`（可再配 `shot_id` / 绝对坐标）；返回 `timed=true`，`planned_ms` = 录制末尾时间戳 |
| 实现 | `GetCursorPos` + `GetAsyncKeyState` **轮询**，不装鼠标钩子（钩子要消息泵、回调超时会被系统摘掉） |
| 注意 | ①调用**阻塞 `duration_ms`**，期间引擎不处理别的请求（故意的，防污染）；②时间轴已平移到首点 = 0ms；③`times_ms` 存在时**跳过重采样**（时间戳要按原始点一一对应），`duration_ms` 被忽略；④只有部分笔有 `times_ms` 时整段退回匀速 |

**实测（2026-10-25）**：另一进程注入 1200ms 的圆弧，`win_record` 录到 1 笔 / 109 点 /
时间戳 0→1198ms；重放 `timed=true, planned_ms=1198, elapsed_ms=1224`（+2.2% 注入开销），
`verify` 报 `changed=true`。

### 2.4 `win_wait`（**没有纯 sleep**）

| | |
| --- | --- |
| 公共输入 | `condition`（必填）：`window` / `image` / `text`；`timeout_ms`（默认 10000，**上限 300000**，超上限会被夹并 warning）；`interval_ms`（默认 300，最小 20）；`hwnd` / `region`（公共区域，规则同 `win_capture`：region > 窗口矩形 > 虚拟桌面） |
| `window` 输入 | `title_contains` / `class_name` / `pid` **至少给一个**（否则等于等所有窗口，直接报错）；`visible_only`（默认 true） |
| `image` 输入 | `template`（必填，模板 PNG 路径）；`min_score`（默认 0.95，1.0=逐像素完全相同）；`template_scale`（模板相对屏幕的缩放系数，默认 1.0） |
| `text` 输入 | `text`（必填，非空子串）；`case_sensitive`（默认 false）；`language`（OCR 语言，如 `en-US` / `zh-Hans-CN`） |
| 成功 | `matched=true` + 该条件专属字段：`window`（`WinInfo`）、`match_point`（模板中心，屏幕坐标）+ `score`、`matched_text` + `ocr_text`（最后一次 OCR 全文，截断 2000 字） |
| 超时 | `matched=false, timed_out=true, waited_ms, polls, timeout_ms`，**RPC 层仍然是成功**——"没等到"是业务结果，不是错误，别当失败重试。另外带上 `score`（image：最像的一次得分）、`last_error`（text：这一轮到底识别到了什么）、`warnings[]`（含阈值提示/超上限说明） |
| 立即失败 | `INVALID_ARGS`：条件参数缺失、模板文件不存在（`NOT_FOUND`）、模板比等待区域还大、`wait_for_text` 但本构建没编 OCR（`UNSUPPORTED`）。**参数校验全部在开始轮询之前**，不会让你等 10 秒才说参数写错 |
| 坑 | ① 模板必须是**未经缩放**的截图裁出来的（`win_capture` 默认把长边缩到 1600，用缩过的图裁模板要填 `template_scale`）；② 匹配是灰度 SSD 两阶段（降采样粗搜挑候选 + 原分辨率精算），显示周期图案（条纹、表格）时会有**多个同分位置**，返回其中一处，`match_point` 不保证是你裁模板的那一处；③ 小字 OCR 会误读（实测 `drags=0` 读成 `drags=e`），要稳就等大字或图像；④ `wait_for_text` 每轮是一次真实截图 + OCR，间隔别低于 300ms |

### 2.5 `win_ocr`

| | |
| --- | --- |
| 输入 | `hwnd`（只读这个窗口，被遮挡也能读）；`region`（只读这块屏幕矩形，与 hwnd 同时给时以 region 为准并裁到窗口内）；`language`（不给用系统语言）；`keep_file`（顺手落一张**未缩放** PNG 供人肉核对） |
| 成功 | `{text, lines:[{text, rect}], language, available_languages[], origin, width, height, ocr_ms, capture_ms, backend, path?}` |
| 失败 | `UNSUPPORTED`：本构建没编 OCR / 系统没有可用 OCR 引擎 / 语言包缺失——**附上可用语言列表，请改用模型自己的视觉能力看图，不要重试**；`ENGINE_ERROR`：WinRT 调用失败（图过大、内存不足）；`NOT_FOUND`：hwnd 无效 |
| 注意 | 行矩形的坐标是**相对截图起点**的，`origin + line.rect` 才是屏幕坐标；识别源图用 `Auto` 后端（PrintWindow 优先，全黑回退 Bitblt），GPU 渲染窗口（Chromium/Electron/微信 4.0）用窗口 DC BitBlt 会得到纯黑图；每次识别都在独立 MTA 线程跑（避免 STA 死锁） |

### 2.6 `win_web_cdp`

| | |
| --- | --- |
| 输入 | `host`（默认 127.0.0.1）；`port`（默认 9222）；三个动作**必须且只能给一个**：`list_targets=true` / `new_target="url"` / `command="CDP 方法名"`+`command_params={...}`；`ws_url`（直接给页面级 WebSocket，跳过目标发现）；`include_events`（顺带返回同连接的 CDP 事件，最多 50 条）；`timeout_ms`（默认 10000） |
| 成功 | `list_targets` → 标签页列表；`new_target` → 新标签页信息；`command` → `{raw: <原始 CDP 结果>}` |
| 失败 | `ENGINE_ERROR`：端口连不上（会提示目标要用 `--remote-debugging-port=9222` 启动）；`INVALID_ARGS`：动作数量不为 1、参数类型错；`TIMEOUT`：超过 `timeout_ms`（连接/HTTP/WebSocket 全程共用 deadline） |
| 注意 | **不解析 DOM、不封装任何 CDP 域**，原始 JSON 原样扔回来，语义由调用方按 CDP 协议理解；这是"真交互"通道，网页上的点击/输入不要用 `win_click` 硬点像素（页面一滚动就错） |

---

### 2.7 `win_find`（0.1.9）

| | |
| --- | --- |
| 输入 | `template`（必填，模板 PNG 绝对路径）；`hwnd`；`region`；`min_score`（默认沿用 wait 的阈值）；`scale`（模板缩放，默认 1.0） |
| 成功 | `{found, score, x, y, w, h, cx, cy, origin:{x,y}, searched_region, elapsed_ms}`；`x/y/w/h` 是屏幕物理像素命中框，`cx/cy` 是中心 |
| 没找到 | `found=false` 仍返回最佳匹配位置和 `score`（**业务结果，不是错误**），先看 `found` |
| 失败 | `NOT_FOUND`：模板文件不存在；`INVALID_ARGS`：模板比搜索区域大、region 越界裁剪后为空、参数错 |
| 注意 | ①**一次性搜索，不轮询**——要等它出现用 `win_wait(condition:"image")`；②大模板全屏暴力匹配慢（1280×800 模板搜 2560×1600 约 2.2–2.5s），尽量传 `hwnd` 或 `region`；③和 `win_wait` 共用同一套匹配器/阈值语义（`score = 1 - RMSE/255`） |

同 exe 还带两个**命令行**模式（给 `zzz-daily.ps1` 这类外部脚本用；不走命名管道、不启 server、不抢单实例锁）：

```
screen-control-engine find    --template <png> [--hwnd N] [--region x,y,w,h] [--min-score 0.9] [--scale 1.0]
screen-control-engine capture --out <绝对png> [--hwnd N] [--region x,y,w,h]
```

- stdout 永远只有一行 JSON；退出码 `0`=找到 / `3`=没找到（`found=false`）/ `2`=错误（stdout `{"ok":false,...}` + stderr 人话）。
- ⚠️ release 版是 **Windows GUI 子系统**（常驻时不弹黑框），所以 **PowerShell 里 `& exe ...` 既拿不到输出也拿不到退出码**（`$LASTEXITCODE` 是空）。PowerShell 调用必须用
  `ProcessStartInfo{UseShellExecute=false; RedirectStandardOutput=true}` 自己重定向；bash / node 的管道调用不受影响。

---

## 3. 旧 8 个工具的改动点

- `win_capture`：`backend` 取值 `auto` / `printwindow` / `bitblt`（**旧的 `gdi` 已改名 `bitblt`**，
  传 `gdi` 会 `INVALID_ARGS`）；新增 `ocr=true` + `ocr_language`，在同一张图上顺带 OCR
  （在**未缩放的原始像素**上识别，结果并进 `ocr` 字段，`path` 仍是不缩放的 PNG）。
- `win_type`：`method` 取值 `auto` / `postmessage` / `clipboard`（**旧的 `message` 已改名 `postmessage`**）。
- `win_click` / `win_press` / `win_scroll`：描述已修正为真实行为（SendInput 真注入 + 抢前台失败即拒绝）。
- 密码框（`ES_PASSWORD` / UIA `IsPassword`）仍是**硬拒绝** `PASSWORD_FIELD`；判定会连同
  `AttachThreadInput` + `GetFocus` 查到的**焦点子控件**一起看，`ctx.target` 被绕过也拦得住。

---

## 4. 错误码（对外只有 7 个）

| 码 | 含义 | 上层怎么办 |
| --- | --- | --- |
| `INVALID_ARGS` | 参数缺失/类型错/自相矛盾 | 改参数重试 |
| `NOT_FOUND` | 窗口、元素、快照、文件、进程不存在 | 重新枚举/换目标 |
| `STALE_SNAPSHOT` | snapshot 过期或 uid 不属于它 | 重新 `win_observe` |
| `PASSWORD_FIELD` | 密码框硬拒绝 | 不要重试，交给人 |
| `UNSUPPORTED` | 平台/构建不支持，或抢不到前台 | **不要重试**；OCR 场景改用模型视觉能力 |
| `TIMEOUT` | 等待操作超时 | 判断是否值得重试 |
| `ENGINE_ERROR` | 其它内部失败（含 Win32/IO/CDP 连不上） | 看 message 与 `win_engine_status` |

---

## 5. 自测入口

```bash
# 1) 离线算法单元测试：不碰屏幕、不开窗口、不注入输入（21 passed / 0 failed / 1 ignored）
cd rust-engine && cargo test --release

# 2) CDP 真机自测：自动起一个独立 profile 的无头 Edge，跑完自动关（10/10）
node scripts/cdp-selftest.mjs                    # --headful 看得见窗口 / --keep 留着不关

# 3) 其它工具的一次性真机整合验证：直连命名管道，不经过 DSH
node scripts/dev-probe.mjs --requests '<json>'   # 或 --file <json>；--quiet 不转发引擎 stderr
```

离线测试覆盖：灰度权重、块平均降采样、最近邻缩放、**模板匹配相位一致性**
（模板裁在非降采样整数倍位置时自比必须 1.0，旧实现只有 0.826）、右下边界定位、
无关模板不得过阈值、参数校验 fail-fast、GDI 内存 DIB 上的真实 OCR、以及 HTTP 分帧
（`Content-Length` / chunked / 无长度 / 半截响应）与 chunked 解析边界。

`cdp-selftest.mjs` 的安全性：独立 `--user-data-dir`（**不碰你手动开着的浏览器**）、
端口已被占用就直接退出（不会连错浏览器自欺欺人）、只按「命令行里带本次临时目录」
精确关进程、原始响应落在 `logs/cdp-selftest-<时间戳>.json`。

---

## 6. 其它注意

`win_wait(condition:"image")` 对**周期性图案**有多解，返回其中一处；用 `region`
缩小范围可规避。其余遗留项见文首「遗留问题」。
