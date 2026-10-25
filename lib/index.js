/**
 * DSH 插件入口 —— 只做三件事:注册工具、校验参数、把请求转给 Rust 引擎。
 *
 * 刻意保持"薄":这里没有一行 Win32 调用、没有图像处理、没有坐标换算。
 * 任何"重"逻辑都应该出现在 rust-engine 里,否则就违背了本插件的架构约定。
 *
 * 引擎是**惰性启动**的:apply() 阶段不 spawn 进程,第一次调用工具时才拉起。
 * 这样即使引擎还没编译出来(缺二进制),插件本身也能正常加载、工具也能注册,
 * 调用时再返回一条明确的"找不到引擎"错误 —— 而不是让整个 profile 加载失败。
 */
import { defineTool } from '@deepseek-ai/dsh-tools';
import { EngineBridge } from './bridge.js';
export const name = 'screen-control';
export const inject = ['tools'];
const ENGINE_TIMEOUT_MS = 15_000;
const OBSERVE_TIMEOUT_MS = 25_000;
const CAPTURE_TIMEOUT_MS = 25_000;
/** 把模型给的参数转成协议要求的 `Record<string, JsonValue>`,丢掉未提供的字段。 */
function toParams(input) {
    const out = {};
    for (const [key, value] of Object.entries(input)) {
        if (value !== undefined)
            out[key] = value;
    }
    return out;
}
/** 统一的模型可见文本渲染。 */
function asText(value, hint) {
    const body = typeof value === 'string' ? value : JSON.stringify(value, null, 1);
    return [{ type: 'text', text: hint ? `${body}\n${hint}` : body }];
}
const RECT_SPEC = {
    type: 'object',
    additionalProperties: false,
    properties: {
        x: { type: 'integer', required: true, description: '左上角 X(屏幕物理像素)' },
        y: { type: 'integer', required: true, description: '左上角 Y(屏幕物理像素)' },
        w: { type: 'integer', required: true, description: '宽度(像素)' },
        h: { type: 'integer', required: true, description: '高度(像素)' },
    },
};
/** 动作生效验证参数(0.2.1)。不传 = 不验证,完全不影响原有行为。 */
const VERIFY_SPEC = {
    type: 'object',
    additionalProperties: true,
    description: '动作生效验证:给了就在动作前抓基准、动作后轮询比对,结果在 effect.changed。' +
        'mode=pixels(默认)比对区域画面变化比例是否 ≥ threshold;' +
        'mode=window 要求动作后出现动作前不存在的窗口(点开对话框/菜单/弹层用这个,比像素稳)。' +
        '动画/视频类目标必须把 threshold 调大(默认 0.002),否则画面本来就在动,永远算生效。',
};
export function apply(ctx) {
    const logger = ctx.logger('screen-control');
    const bridge = new EngineBridge({
        info: (message) => logger.info(message),
        warn: (message) => logger.warn(message),
    });
    // 插件卸载时释放 Rust 子进程;桥的 dispose() 可重复调用。
    ctx.effect(() => () => bridge.dispose(), 'screen-control/engine');
    const register = (definition, label) => {
        ctx.effect(() => ctx.tools.register(definition), `screen-control/tool:${label}`);
    };
    register(defineTool({
        name: 'win_engine_status',
        description: '查看屏幕操控引擎(Rust 常驻进程)的状态:是否已启动、可执行文件路径、重启次数、最近的错误。' +
            '任何 win_* 工具报错时先用它排查。',
        parameters: {},
        timeoutMs: ENGINE_TIMEOUT_MS,
        output: { schema: { type: 'json' }, render: (_args, value) => asText(value) },
        execute: async () => {
            // 状态必须在探活之后取:探活之前引擎还没被拉起来,running 永远是 false。
            try {
                const pong = await bridge.call('engine_ping', {}, { timeoutMs: ENGINE_TIMEOUT_MS });
                const merged = {};
                if (pong !== null && typeof pong === 'object' && !Array.isArray(pong)) {
                    Object.assign(merged, pong);
                }
                merged.bridge = bridge.status();
                return merged;
            }
            catch (error) {
                // 引擎没编译出来时,这里必须给出"人话"错误,而不是让调用方去猜。
                return { started: false, error: error.message, bridge: bridge.status() };
            }
        },
    }), 'win_engine_status');
    register(defineTool({
        name: 'win_list_windows',
        description: '列出当前桌面上的顶级窗口(句柄、标题、类名、进程、位置尺寸、Z-order)。' +
            '坐标一律是屏幕物理像素。这是"观察"的第一步:先拿到 hwnd,再交给 win_capture / win_observe。',
        parameters: {
            title_contains: { type: 'string', description: '只返回标题包含该子串的窗口,不区分大小写。' },
            class_name: { type: 'string', description: '只返回类名完全等于该值的窗口,例如 ApplicationFrameWindow。' },
            pid: { type: 'integer', description: '只返回属于该进程 ID 的窗口。' },
            process_name: {
                type: 'string',
                description: '只返回这个进程的窗口,按可执行文件名匹配(不区分大小写,写 QQ 或 QQ.exe 等价,不做子串匹配)。' +
                    '比 pid 少一次 tasklist 往返:一次调用就能同时回答"进程有没有窗口"和"hwnd 是多少"。',
            },
            visible_only: { type: 'boolean', description: '是否只返回可见窗口,默认 true。' },
            limit: { type: 'integer', description: '最多返回多少个窗口,默认 50,上限 500。' },
        },
        timeoutMs: ENGINE_TIMEOUT_MS,
        output: { schema: { type: 'json' }, render: (_args, value) => asText(value) },
        execute: async (args, exec) => {
            // 工具参数是平铺的,协议里 filter 是嵌套对象 —— 在这里转换一次,
            // 别让引擎去猜两种形状(猜错就是"过滤没生效"这种最难查的 bug)。
            const filter = {};
            if (args.title_contains !== undefined)
                filter.title_contains = args.title_contains;
            if (args.class_name !== undefined)
                filter.class_name = args.class_name;
            if (args.pid !== undefined)
                filter.pid = args.pid;
            if (args.process_name !== undefined)
                filter.process_name = args.process_name;
            if (args.visible_only !== undefined)
                filter.visible_only = args.visible_only;
            return await bridge.call('win_list_windows', toParams({ filter, limit: args.limit }), {
                timeoutMs: ENGINE_TIMEOUT_MS,
                signal: exec.signal,
            });
        },
    }), 'win_list_windows');
    register(defineTool({
        name: 'win_capture',
        description: '截取指定窗口或屏幕区域的图像,压缩后返回一个 PNG 文件路径(不走 Base64)。' +
            '后台窗口也能截:引擎优先用 PrintWindow,失败再回退 GDI BitBlt。' +
            '默认把长边缩到 1600 像素以节省 Token。拿到 path 后用 read_image 工具查看图片。' +
            'ocr=true 时同时返回 Windows 原生 OCR 的识别文本(在未缩放的原始像素上识别);' +
            '本机没装 OCR 语言包会直接报 UNSUPPORTED,这时改用 read_image 自己看图。' +
            '返回值里的 coord 是坐标换算信息:coord.shot_id 交给动作工具的 shot_id,' +
            '你在图片上量到的坐标就能直接当参数用,不用自己乘缩放系数。',
        parameters: {
            hwnd: { type: 'integer', description: '目标窗口句柄;与 region 二选一,都不给则截主屏。' },
            region: { ...RECT_SPEC, description: '屏幕物理像素矩形;给了就截这块区域。' },
            max_dim: { type: 'integer', description: '长边缩放上限(像素),默认 1600;传 0 表示不缩放(做模板时要传 0)。' },
            format: { type: 'string', enum: ['png'], description: '编码格式,目前只支持 png。' },
            backend: {
                type: 'string',
                enum: ['auto', 'printwindow', 'bitblt'],
                description: '捕获后端,默认 auto(先 PrintWindow,全黑才回退 Bitblt)。做模板匹配时用 auto,' +
                    '和 wait_for_image 的后端保持一致;bitblt 对 GPU 渲染的窗口(Chromium/Electron)会得到纯黑图。',
            },
            ocr: { type: 'boolean', description: 'true 时附带 OCR 文本,结果在返回值的 ocr 字段里。默认 false。' },
            ocr_language: { type: 'string', description: 'OCR 语言,例如 zh-Hans-CN / en-US;不给用系统语言。' },
        },
        timeoutMs: CAPTURE_TIMEOUT_MS,
        output: { schema: { type: 'json' }, render: (_args, value) => asText(value, '提示:用 read_image 读取上面 path 指向的 PNG 就能看到画面;返回值 coord.shot_id 可交给动作工具做坐标换算。') },
        execute: async (args, exec) => await bridge.call('win_capture', toParams(args), {
            timeoutMs: CAPTURE_TIMEOUT_MS,
            signal: exec.signal,
        }),
    }), 'win_capture');
    register(defineTool({
        name: 'win_observe',
        description: '观察目标窗口:返回结构化 UI 元素树(每个可交互元素带 uid、name、control_type、rect)' +
            '以及一张高亮缩略图,并生成 snapshot_id。元素树比截图省 Token,优先用它。' +
            'snapshot_id 默认 5 秒过期;带 snapshot_id + uid 的动作才是安全的,过期会被引擎拒绝。',
        parameters: {
            hwnd: { type: 'integer', required: true, description: '目标窗口句柄,从 win_list_windows 获得。' },
            max_depth: { type: 'integer', description: '元素树最大深度,默认 4。' },
            max_elements: { type: 'integer', description: '元素数量上限,默认 400,超出则 truncated=true。' },
            with_screenshot: { type: 'boolean', description: '是否附带高亮缩略图,默认 true。' },
            snapshot_ttl_ms: { type: 'integer', description: '快照有效期(毫秒),默认 5000。' },
        },
        timeoutMs: OBSERVE_TIMEOUT_MS,
        output: { schema: { type: 'json' }, render: (_args, value) => asText(value) },
        execute: async (args, exec) => await bridge.call('win_observe', toParams(args), {
            timeoutMs: OBSERVE_TIMEOUT_MS,
            signal: exec.signal,
        }),
    }), 'win_observe');
    register(defineTool({
        name: 'win_click',
        description: '点击。走 SendInput 真注入:会短暂移动真实光标(结束后还原到原位)、会激活被点的窗口' +
            '(这是 Windows 的语义,点完不还前台)。坐标是屏幕物理像素。' +
            '进度:先落点前 1px 抖动再回目标点(自绘控件靠 hover 判定),按下保持 12ms 再抬起。' +
            '推荐用法:先 win_observe 拿 snapshot_id,再用 snapshot_id + uid 点击。' +
            '返回 warnings 里会写清光标有没有还原成功。',
        parameters: {
            snapshot_id: { type: 'integer', description: 'win_observe 返回的快照 id;配合 uid 使用,过期会被拒绝。' },
            uid: { type: 'integer', description: '元素 uid;与 snapshot_id 一起用,由引擎换算成精确坐标。' },
            hwnd: { type: 'integer', description: '目标窗口句柄;不用 snapshot_id 时必填。' },
            x: { type: 'integer', description: '屏幕物理像素 X;不用 uid 时必填。' },
            y: { type: 'integer', description: '屏幕物理像素 Y;不用 uid 时必填。' },
            button: { type: 'string', enum: ['left', 'right', 'middle'], description: '鼠标键,默认 left。' },
            clicks: { type: 'integer', description: '点击次数,默认 1;2 表示双击。' },
            force_sendinput: { type: 'boolean', description: '强制走 SendInput 降级路径(调试用)。' },
            shot_id: {
                type: 'integer',
                description: '截图坐标:win_capture 返回的 coord.shot_id。给了它,x/y 按那张截图的像素解释,引擎换算成屏幕坐标。',
            },
            verify: VERIFY_SPEC,
        },
        timeoutMs: ENGINE_TIMEOUT_MS,
        output: { schema: { type: 'json' }, render: (_args, value) => asText(value, '提示:点击后建议再 win_observe 一次确认效果,不要凭猜测继续操作。') },
        execute: async (args, exec) => await bridge.call('win_click', toParams(args), {
            timeoutMs: ENGINE_TIMEOUT_MS,
            signal: exec.signal,
        }),
    }), 'win_click');
    register(defineTool({
        name: 'win_type',
        description: '输入文本(中文/emoji 都支持)。默认 auto:纯 ASCII 走 KEYEVENTF_UNICODE 真注入;' +
            '文本含非 ASCII(中文/emoji/全角标点)时**自动改走剪贴板**——引擎自己把文本写进' +
            'CF_UNICODETEXT 再按 Ctrl+V,返回值 warnings 里会明说"剪贴板内容已被覆盖";' +
            '写剪贴板失败会自动退回逐字符注入并给 warning(这时长中文串可能串位,要截图回读)。' +
            '默认方式会临时把目标窗口切到前台(抢不到前台就直接报 UNSUPPORTED,不会让按键落进别的窗口),' +
            '输入完把前台还给原窗口。检测到密码框(PasswordBox / ES_PASSWORD)时**硬性拒绝**,不会输入任何字符。' +
            'method="sendinput" 强制逐字符 Unicode 注入(不碰剪贴板);' +
            'method="postmessage" 走 WM_CHAR(不碰前台、不碰剪贴板,对普通 Win32 编辑控件有效);' +
            'method="clipboard" 只发 Ctrl+V,要求调用方自己先把文本放进剪贴板。',
        parameters: {
            snapshot_id: { type: 'integer', description: 'win_observe 的快照 id;配合 uid 定位输入框。' },
            uid: { type: 'integer', description: '输入框元素的 uid,由引擎换算成屏幕坐标并定位到该控件。' },
            hwnd: { type: 'integer', description: '目标窗口句柄;不用 snapshot_id 时必填。' },
            text: { type: 'string', required: true, description: '要输入的文本。' },
            method: {
                type: 'string',
                enum: ['auto', 'postmessage', 'sendinput', 'clipboard'],
                description: '输入方式,默认 auto(非 ASCII 自动走剪贴板,ASCII 走真注入)。' +
                    'postmessage=WM_CHAR 不碰前台;sendinput=强制逐字符 Unicode 注入;clipboard=只发 Ctrl+V(剪贴板由调用方自己写)。',
            },
        },
        timeoutMs: ENGINE_TIMEOUT_MS,
        output: { schema: { type: 'json' }, render: (_args, value) => asText(value) },
        execute: async (args, exec) => await bridge.call('win_type', toParams(args), {
            timeoutMs: ENGINE_TIMEOUT_MS,
            signal: exec.signal,
        }),
    }), 'win_type');
    register(defineTool({
        name: 'win_press',
        description: '发送一个按键(可带修饰键),例如 Enter、Tab、Escape、Ctrl+A。走 SendInput 真注入:' +
            '会临时把目标窗口切到前台(抢不到就报 UNSUPPORTED,不会让按键落进别的窗口),' +
            '完成后把前台还给原窗口。key 用 Windows 虚拟键名或单个字符。',
        parameters: {
            snapshot_id: { type: 'integer', description: 'win_observe 的快照 id。' },
            uid: { type: 'integer', description: '聚焦目标元素的 uid;不给则发给整个窗口。' },
            hwnd: { type: 'integer', description: '目标窗口句柄;不用 snapshot_id 时必填。' },
            key: {
                type: 'string',
                required: true,
                description: '可用键名:Enter/Return、Tab、Esc/Escape、Space、Backspace、Delete、Insert、Home、End、' +
                    'PageUp/PageDown、Left/Right/Up/Down、F1..F12、单个字母或数字(如 a、7)、' +
                    'numpad0..numpad9、plus/minus/comma/period/slash 等;也接受 ctrl/alt/shift/win 本身。',
            },
            modifiers: {
                type: 'array',
                items: { type: 'string', enum: ['ctrl', 'alt', 'shift', 'win'] },
                description: '修饰键列表,取值只能是 ctrl / alt / shift / win(小写);例如 ["ctrl","shift"] 表示 Ctrl+Shift+key。',
            },
            repeat: { type: 'integer', description: '重复次数,默认 1。' },
            verify: VERIFY_SPEC,
        },
        timeoutMs: ENGINE_TIMEOUT_MS,
        output: { schema: { type: 'json' }, render: (_args, value) => asText(value) },
        execute: async (args, exec) => await bridge.call('win_press', toParams(args), {
            timeoutMs: ENGINE_TIMEOUT_MS,
            signal: exec.signal,
        }),
    }), 'win_press');
    register(defineTool({
        name: 'win_scroll',
        description: '在指定位置滚轮滚动。走 SendInput 真注入:会短暂移动真实光标(结束后还原)、' +
            '并激活滚动位置的窗口(Windows 的语义:滚轮事件发给光标下的窗口)。' +
            'delta 为正向上滚、为负向下滚,一个刻度是 120(例如 -360 表示向下滚 3 格)。',
        parameters: {
            snapshot_id: { type: 'integer', description: 'win_observe 的快照 id。' },
            uid: { type: 'integer', description: '滚动目标元素的 uid;不给则用窗口中心。' },
            hwnd: { type: 'integer', description: '目标窗口句柄;不用 snapshot_id 时必填。' },
            delta: { type: 'integer', required: true, description: '滚轮增量,一格 120;负数向下滚。' },
            x: { type: 'integer', description: '滚动位置 X(屏幕物理像素);不给则用窗口中心。' },
            y: { type: 'integer', description: '滚动位置 Y(屏幕物理像素);不给则用窗口中心。' },
            shot_id: { type: 'integer', description: '截图坐标:x/y 按该截图的像素解释。' },
            verify: VERIFY_SPEC,
        },
        timeoutMs: ENGINE_TIMEOUT_MS,
        output: { schema: { type: 'json' }, render: (_args, value) => asText(value) },
        execute: async (args, exec) => await bridge.call('win_scroll', toParams(args), {
            timeoutMs: ENGINE_TIMEOUT_MS,
            signal: exec.signal,
        }),
    }), 'win_scroll');
    register(defineTool({
        name: 'win_launch',
        description: '拉起一个进程(可带启动参数和工作目录)。只负责启动,**不等窗口出现**——' +
            '等窗口交给 win_wait(condition="window")。"起没起来"用 GetExitCodeProcess 复核:' +
            '返回 still_running=false 说明进程秒退(路径或参数不对),别再傻等窗口。' +
            '不接受 cd/dir/start 这类 shell 内建命令;要 shell 语义就自己启动 cmd.exe /C。',
        parameters: {
            path: { type: 'string', required: true, description: '可执行文件路径,或 PATH 里的名字(notepad.exe / notepad)。' },
            args: { type: 'array', items: { type: 'string' }, description: '启动参数,逐个传,不要自己拼引号。' },
            cwd: { type: 'string', description: '子进程工作目录;必须是已存在的目录,否则 NOT_FOUND。' },
            no_window: { type: 'boolean', description: 'true 时加 CREATE_NO_WINDOW,控制台程序不弹黑框。默认 false。' },
            check_ms: { type: 'integer', description: '启动后等多少毫秒再确认进程还活着,默认 30;传 0 表示不检查。' },
        },
        timeoutMs: ENGINE_TIMEOUT_MS,
        output: {
            schema: { type: 'json' },
            render: (_args, value) => asText(value, '提示:启动成功 ≠ 窗口已出现。要等界面就接着调 win_wait(condition="window")。'),
        },
        execute: async (args, exec) => await bridge.call('win_launch', toParams(args), {
            timeoutMs: ENGINE_TIMEOUT_MS,
            signal: exec.signal,
        }),
    }), 'win_launch');
    register(defineTool({
        name: 'win_window_state',
        description: '窗口状态控制。action 五选一:maximize(最大化)/ minimize(最小化)/ restore(还原)/ ' +
            'fullscreen(无边框+置顶的伪全屏,铺满窗口所在显示器)/ exit_fullscreen(按进入全屏前' +
            '记录的样式还原)。全屏前的原始样式由引擎记着,反复调 fullscreen 不会覆盖记录;' +
            '引擎重启后记录丢失,那时 exit_fullscreen 只能补回标准边框并取消置顶,会在 warnings 里说明。' +
            '操作前用 AttachThreadInput 稳住焦点,做完把前台还给原窗口(最小化例外,那本身就会丢前台)。',
        parameters: {
            hwnd: { type: 'integer', required: true, description: '目标窗口句柄,从 win_list_windows 获得。' },
            action: {
                type: 'string',
                required: true,
                enum: ['maximize', 'minimize', 'restore', 'fullscreen', 'exit_fullscreen'],
                description: '要做的状态变更。',
            },
        },
        timeoutMs: ENGINE_TIMEOUT_MS,
        output: {
            schema: { type: 'json' },
            render: (_args, value) => asText(value, '返回值里 is_fullscreen/is_maximized/is_minimized 都是操作后回读的真实状态。'),
        },
        execute: async (args, exec) => await bridge.call('win_window_state', toParams(args), {
            timeoutMs: ENGINE_TIMEOUT_MS,
            signal: exec.signal,
        }),
    }), 'win_window_state');
    register(defineTool({
        name: 'win_drag',
        description: '拖拽:按下 → 轨迹插值 → 抬起,走 SendInput 真注入。按下后按 steps 逐步插值移动' +
            '(smoothstep 缓入缓出,中间点叠 1px 法向微动),这样控件才认得出"这是一次拖拽"——' +
            '只发"移动+按下+移动+松开"的话,Win32 列表视图拖选、Qt 拖放手势、网页 HTML5 拖放' +
            '很多都当成没动过。起点和终点**都必须落在目标窗口上**,否则直接拒绝' +
            '(落错地方的拖拽可能把东西拖进别的程序,比点错更糟)。' +
            '典型用途:拖标题栏移窗口、拖选文本、拖滑块。',
        parameters: {
            from_x: { type: 'integer', description: '起点 X(屏幕物理像素);给了 snapshot_id + uid 时可省略。' },
            from_y: { type: 'integer', description: '起点 Y(屏幕物理像素);与 from_x 必须成对给。' },
            to_x: { type: 'integer', required: true, description: '终点 X(屏幕物理像素)。' },
            to_y: { type: 'integer', required: true, description: '终点 Y(屏幕物理像素)。' },
            snapshot_id: { type: 'integer', description: 'win_observe 的快照 id;配 uid 时用元素中心当起点。' },
            uid: { type: 'integer', description: '起点元素的 uid。' },
            hwnd: { type: 'integer', description: '目标窗口句柄;不用 snapshot_id 时必填。' },
            button: { type: 'string', enum: ['left', 'right', 'middle'], description: '鼠标键,默认 left。' },
            steps: { type: 'integer', description: '轨迹点数(含终点),默认 24,夹在 2..200。' },
            duration_ms: { type: 'integer', description: '从按下到抬起的计划时长,默认 400,夹在 50..10000。' },
            wobble: { type: 'boolean', description: '轨迹中间点是否叠 1px 微动,默认 true。' },
            shot_id: { type: 'integer', description: '截图坐标:from_x/from_y/to_x/to_y 全按该截图的像素解释。' },
            verify: VERIFY_SPEC,
        },
        timeoutMs: 30_000,
        output: { schema: { type: 'json' }, render: (_args, value) => asText(value, '拖完建议截图回读确认结果(拖动位置对不对、有没有真的拖起来)。') },
        execute: async (args, exec) => await bridge.call('win_drag', toParams(args), {
            timeoutMs: 30_000,
            signal: exec.signal,
        }),
    }), 'win_drag');
    register(defineTool({
        name: 'win_drag_path',
        description: '沿整条路径连续画:一次按下 → 按弧长匀速走完所有点 → 抬起;多笔之间自动抬笔再落笔。' +
            '画弧线/圆/曲线/签名/摇杆轨迹用这个,不要用 win_drag 拼段——' +
            'win_drag 只走一段直线,拼多段会在每段之间断笔,且每段独立加减速(折角 + 速度突变)。' +
            '三种给法(优先级从高到低):strokes(多笔点列)、points(单笔点列)、' +
            'shape(形状快捷方式,给圆心半径就行,不用自己算几百个点)。' +
            '引擎内部:按累计弧长重采样成等距点、时间按弧长等分(匀速)、系统定时器提到 1ms' +
            '(否则步与步之间是 15.6ms 的台阶,画出来一顿一顿)。' +
            '所有点必须落在目标窗口矩形内,否则整条路径拒绝执行(画出界=画到别的程序上)。',
        parameters: {
            hwnd: { type: 'integer', description: '目标窗口句柄;不给就用首点做命中测试找窗口。' },
            snapshot_id: { type: 'integer', description: 'win_observe 的快照 id;配 uid 时用元素中心当起点。' },
            uid: { type: 'integer', description: '起点元素 uid。' },
            from_x: { type: 'integer', description: '起点 X(屏幕物理像素);给了会在路径最前面插一个落笔点。' },
            from_y: { type: 'integer', description: '起点 Y;与 from_x 必须成对给。' },
            points: {
                type: 'array',
                items: { type: 'array', items: { type: 'number' } },
                description: '单笔点列 [[x,y],...],屏幕物理像素。',
            },
            strokes: {
                type: 'array',
                items: { type: 'array', items: { type: 'array', items: { type: 'number' } } },
                description: '多笔 [[[x,y],...],...];笔之间自动抬笔再落笔。',
            },
            shape: {
                type: 'object',
                additionalProperties: true,
                description: '形状快捷方式:{"kind":"circle","cx","cy","r","start_deg"?,"ccw"?} 或 ' +
                    '{"kind":"arc","cx","cy","r","start_deg","end_deg","ccw"?} 或 ' +
                    '{"kind":"bezier","points":[[x,y],...]}(2~4 个控制点)。',
            },
            step_px: { type: 'number', description: '相邻落点最大间距(像素),默认 2;越小越圆、点数与耗时越高。' },
            duration_ms: {
                type: 'integer',
                description: '总时长(含笔间移动),不给就按路径长度自动算(约 1.2ms/像素)。',
            },
            wobble: { type: 'boolean', description: '轨迹微抖,默认 false。画弧线时开了会出毛刺,别开。' },
            button: { type: 'string', enum: ['left', 'right', 'middle'], description: '鼠标键,默认 left。' },
            shot_id: {
                type: 'integer',
                description: '截图坐标:points/strokes/shape 里的坐标全按该截图的像素解释(半径按两轴平均缩放)。',
            },
            times_ms: {
                type: 'array',
                items: { type: 'array', items: { type: 'number' } },
                description: '录制回放:每笔每点的时间戳(毫秒),与点数一一对应;给了就跳过重采样、按人手快慢重放,' +
                    '忽略 duration_ms。直接把 win_record 返回的 strokes[].times_ms 原样贴进来。',
            },
            verify: VERIFY_SPEC,
        },
        timeoutMs: 120_000,
        output: {
            schema: { type: 'json' },
            render: (_args, value) => asText(value, '画完建议截图回读确认线形;duration_ms 与 planned_ms 的差就是时间控制精度。'),
        },
        execute: async (args, exec) => await bridge.call('win_drag_path', toParams(args), {
            timeoutMs: 120_000,
            signal: exec.signal,
        }),
    }), 'win_drag_path');
    register(defineTool({
        name: 'win_image_trace',
        description: '把图里的线条变成可直接画的笔顺:二值化(Otsu) → Zhang-Suen 细化成 1px 骨架 → ' +
            '沿骨架提笔顺(贪心+惯性,断线自动接回、重复像素自动丢弃) → Douglas-Peucker 简化 → ' +
            '等距重采样。返回 strokes[].points,**图片像素坐标**。' +
            '图源二选一:path(图片文件)或 hwnd/region(自截,会顺带返回 coord.shot_id)。' +
            '标准用法:win_capture 或本工具自截 → 拿 strokes + shot_id → 直接喂 win_drag_path,' +
            '坐标由引擎换算。**不要让模型自己看图报坐标**——模型只能一个个吐 token,又慢又歪。',
        parameters: {
            path: { type: 'string', description: '图片文件路径(PNG);不给就自截。' },
            hwnd: { type: 'integer', description: '自截的目标窗口句柄;与 region 二选一。' },
            region: { ...RECT_SPEC, description: '自截的屏幕区域(物理像素);不给就整窗/整屏。' },
            max_dim: { type: 'integer', description: '自截时长边上限,默认 1600;0=不缩放。' },
            threshold: { type: 'integer', description: '二值化阈值 0~255;不给用 Otsu 自动算。' },
            invert: { type: 'boolean', description: 'true=亮色是墨(黑影上的白线),默认 false。' },
            crop: { type: 'array', items: { type: 'integer' }, description: '只处理图片内这块 [x,y,w,h](图片像素)。' },
            min_area: { type: 'integer', description: '丢掉面积小于它的连通块,默认 16。' },
            smooth: { type: 'number', description: 'Douglas-Peucker 容差(图片像素),默认 1.2;0=不简化。' },
            step_px: { type: 'number', description: '输出笔顺的重采样间距(图片像素),默认 2.0。' },
            max_strokes: { type: 'integer', description: '最多输出几笔(按长度从长到短取),默认 64。' },
            close_loops: { type: 'boolean', description: '首尾距离 < step*1.5 时闭合,默认 true。' },
            min_len_px: { type: 'number', description: '丢掉长度小于它的笔顺(像素),默认 8;清骨架残渣用。' },
        },
        timeoutMs: 60_000,
        output: {
            schema: { type: 'json' },
            render: (_args, value) => asText(value, '拿到 strokes 后:有 coord 就把 coord.shot_id 和 strokes 一起传给 win_drag_path;没有 coord(从文件追踪)则坐标是图片像素,需要自己对应到屏幕。'),
        },
        execute: async (args, exec) => await bridge.call('win_image_trace', toParams(args), {
            timeoutMs: 60_000,
            signal: exec.signal,
        }),
    }), 'win_image_trace');
    register(defineTool({
        name: 'win_record',
        description: '录一段人手的鼠标轨迹:按住左键期间的移动会被采样成笔顺(抬键即断笔),' +
            '每个点带毫秒时间戳。返回 strokes[].points + strokes[].times_ms,' +
            '原样贴进 win_drag_path(配 shot_id 或直接绝对坐标)就能**按原速重放**。' +
            '实现是 GetCursorPos 轮询 + GetAsyncKeyState 判按键(不装钩子,不改系统状态),' +
            '默认 120Hz。注意:调用会**阻塞 duration_ms 那么久**,期间引擎不处理别的请求——' +
            '这是故意的,录的时候不该有别的注入污染轨迹。',
        parameters: {
            duration_ms: { type: 'integer', description: '录制时长(毫秒),默认 5000,夹 100..60000。' },
            hz: { type: 'integer', description: '采样频率,默认 120,夹 20..500。' },
            max_points: { type: 'integer', description: '采样点数上限,默认 20000。' },
        },
        timeoutMs: 90_000,
        output: {
            schema: { type: 'json' },
            render: (_args, value) => asText(value, '重放:把 strokes[].points 和 strokes[].times_ms 分别传给 win_drag_path 的 strokes / times_ms。'),
        },
        execute: async (args, exec) => await bridge.call('win_record', toParams(args), {
            timeoutMs: 90_000,
            signal: exec.signal,
        }),
    }), 'win_record');
    register(defineTool({
        name: 'win_wait',
        description: '条件等待(**不是** sleep)。condition 三选一:' +
            'window=轮询等窗口出现(title_contains / class_name / pid 至少给一个);' +
            'image=在指定区域轮询做模板匹配(template 是模板 PNG 路径,必须**未经缩放**,建议先用' +
            'win_capture(max_dim=0, backend="gdi") 截原图再裁一块出来);' +
            'text=轮询截图 + Windows 原生 OCR,在识别文本里找子串。' +
            '超时**不算报错**:返回 matched=false、timed_out=true、waited_ms、polls,' +
            'image 超时还会带上"最像的一次得分"score,warnings 里写明差多少、模板可能是哪里不对。',
        parameters: {
            condition: {
                type: 'string',
                required: true,
                enum: ['window', 'image', 'text'],
                description: '等待条件类型:window / image / text。',
            },
            timeout_ms: { type: 'integer', description: '超时毫秒,默认 10000,上限 300000(5 分钟);到点就返回,绝不死等。' },
            interval_ms: { type: 'integer', description: '轮询间隔毫秒,默认 300,下限 20。' },
            title_contains: { type: 'string', description: 'window:标题包含该子串(不区分大小写)。' },
            class_name: { type: 'string', description: 'window:类名精确匹配。' },
            pid: { type: 'integer', description: 'window:属于该进程 ID。' },
            visible_only: { type: 'boolean', description: 'window:是否只认可见窗口,默认 true。' },
            template: { type: 'string', description: 'image:模板 PNG 的绝对路径。' },
            min_score: { type: 'number', description: 'image:匹配阈值 0..1,默认 0.95;1.0 表示逐像素完全相同。' },
            template_scale: { type: 'number', description: 'image:模板相对屏幕的缩放系数,默认 1.0(模板若来自缩过图的截图,这里填回去)。' },
            text: { type: 'string', description: 'text:要等到的子串(在 OCR 结果里找)。' },
            case_sensitive: { type: 'boolean', description: 'text:是否区分大小写,默认 false。' },
            language: { type: 'string', description: 'text:OCR 语言(如 zh-Hans-CN),不给用系统语言。' },
            hwnd: { type: 'integer', description: 'image/text:在哪个窗口里等;给了就用窗口 DC 截图(被遮挡也能读)。' },
            region: { ...RECT_SPEC, description: 'image/text:在屏幕这块矩形里等;给了就以它为准。' },
        },
        timeoutMs: 330_000,
        output: {
            schema: { type: 'json' },
            render: (_args, value) => asText(value, '超时请读 matched / timed_out / warnings:matched=false 就是没等到,不是工具坏了。'),
        },
        execute: async (args, exec) => await bridge.call('win_wait', toParams(args), {
            timeoutMs: 330_000,
            signal: exec.signal,
        }),
    }), 'win_wait');
    register(defineTool({
        name: 'win_find',
        description: '一次性屏幕定位:在窗口或屏幕区域里做模板匹配,返回模板现在的位置和相似度。' +
            '**只搜一次、不轮询**;要等它出现请用 win_wait(condition="image")。' +
            'template 是模板 PNG 绝对路径,必须从**未经缩放**的截图里裁出来' +
            '(win_capture 时传 max_dim=0),否则用 scale 把缩放系数补回去。' +
            '搜索区域规则与 win_capture 一致:region > hwnd 的窗口矩形 > 整个屏幕(region 越界会裁剪并在 warnings 说明)。' +
            '返回 found/score/x/y/w/h/cx/cy/origin/searched_region/elapsed_ms/warnings;' +
            'found=false 是正常结果而不是报错,这时 score 与 x/y 仍是最佳候选,必须先看 found 再用坐标。',
        parameters: {
            template: {
                type: 'string',
                required: true,
                description: '模板 PNG 绝对路径;必须未经缩放,或把缩放系数填到 scale。',
            },
            hwnd: { type: 'integer', description: '目标窗口句柄;给了就用它的窗口矩形当默认搜索区域。' },
            region: { ...RECT_SPEC, description: '搜索区域(屏幕物理像素);越界会与捕获区域求交并在 warnings 里说明。' },
            min_score: {
                type: 'number',
                description: '匹配阈值 0..1,默认沿用 wait 的 0.95;1.0 = 逐像素完全相同。',
            },
            scale: {
                type: 'number',
                description: '模板相对屏幕的缩放系数,默认 1.0(模板若来自缩过图的截图,这里填回去)。',
            },
        },
        timeoutMs: CAPTURE_TIMEOUT_MS,
        output: {
            schema: { type: 'json' },
            render: (_args, value) => asText(value, '提示:found=false 不是工具坏了,读 score/warnings 判断差多少;要"等出现"请用 win_wait(condition="image")。'),
        },
        execute: async (args, exec) => {
            const result = await bridge.find({
                template: args.template,
                hwnd: args.hwnd,
                region: args.region,
                min_score: args.min_score,
                scale: args.scale,
            }, { timeoutMs: CAPTURE_TIMEOUT_MS, signal: exec.signal });
            // FindResult 是纯 JSON 形状的结构体(没有函数/类实例),工具层要求 JsonValue,
            // 这里做一次显式转换而不是把 bridge 的类型化返回值降级成 any。
            return result;
        },
    }), 'win_find');
    register(defineTool({
        name: 'win_ocr',
        description: '调用 Windows 原生 OCR(Windows.Media.Ocr)识别指定窗口或区域的文字,' +
            '返回全文 text、逐行 lines(每行带文本和像素矩形)。不自己搓 OCR 引擎。' +
            '本机没装可用 OCR 语言包、或构建没编译 OCR 时直接返回 UNSUPPORTED(不会硬编一个烂结果),' +
            '这时改用 win_capture + read_image 让模型自己看图。' +
            'lines[].rect 是相对截图左上角的矩形,加上 origin 才是屏幕坐标。' +
            '识别源图会落一张不缩放的 PNG,路径在 path 字段,可人肉核对。',
        parameters: {
            hwnd: { type: 'integer', description: '只识别这个窗口;给了就用窗口 DC(窗口被遮挡也读得到)。' },
            region: { ...RECT_SPEC, description: '只识别屏幕这块矩形;与 hwnd 同时给时以 region 为准(裁到窗口内)。' },
            language: { type: 'string', description: 'OCR 语言,例如 zh-Hans-CN / en-US;不给用系统语言。' },
        },
        timeoutMs: CAPTURE_TIMEOUT_MS,
        output: { schema: { type: 'json' }, render: (_args, value) => asText(value, '识别失败请先看 code:UNSUPPORTED = 本机 OCR 不可用,换 read_image 走视觉。') },
        execute: async (args, exec) => await bridge.call('win_ocr', toParams(args), {
            timeoutMs: CAPTURE_TIMEOUT_MS,
            signal: exec.signal,
        }),
    }), 'win_ocr');
    register(defineTool({
        name: 'win_web_cdp',
        description: '走 Chrome DevTools Protocol 做网页交互(**不是**模拟点击:点像素那种做法页面一滚动就错)。' +
            '内部用阻塞式 WebSocket 连到 host:port(默认 127.0.0.1:9222),三个动作必须且只能给一个:' +
            'command=发一条 CDP 命令(Runtime.evaluate / Page.navigate / DOM.getDocument 等),' +
            '原始 JSON 原样返回在 raw/result 里;list_targets=true 返回 /json/list;' +
            'new_target="url" 新建标签页。引擎不解析 DOM、不封装任何 CDP 域,' +
            '返回值的语义由调用方自己按 CDP 协议理解。' +
            '前提:目标浏览器/Electron 用 --remote-debugging-port=9222 启动;端口没监听会明确报连不上。',
        parameters: {
            host: { type: 'string', description: '调试主机,默认 127.0.0.1。' },
            port: { type: 'integer', description: '调试端口,默认 9222。' },
            ws_url: { type: 'string', description: '直接给页面级 WebSocket 地址(ws://...);给了就跳过目标发现。' },
            command: { type: 'string', description: 'CDP 方法名,例如 Page.navigate、Runtime.evaluate。' },
            command_params: {
                type: 'object',
                additionalProperties: true,
                description: 'CDP 命令参数对象,原样发给浏览器,例如 {expression:"document.title", returnByValue:true}。',
            },
            list_targets: { type: 'boolean', description: 'true 时只返回 /json/list 的目标列表。' },
            new_target: { type: 'string', description: '给一个 URL,新建标签页并返回它的目标信息。' },
            timeout_ms: { type: 'integer', description: '单次调用超时毫秒,默认 10000,夹在 100..120000。' },
            include_events: { type: 'boolean', description: 'true 时把同一次连接里收到的 CDP 事件也返回(最多 50 条)。' },
        },
        timeoutMs: 20_000,
        output: {
            schema: { type: 'json' },
            render: (_args, value) => asText(value, '用法示例:Runtime.evaluate + {expression:"document.title", returnByValue:true} 取标题。'),
        },
        execute: async (args, exec) => await bridge.call('win_web_cdp', toParams(args), {
            timeoutMs: 20_000,
            signal: exec.signal,
        }),
    }), 'win_web_cdp');
}
//# sourceMappingURL=index.js.map