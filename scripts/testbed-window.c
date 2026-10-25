/*
 * DSH screen-control 验收靶窗。
 *
 * 为什么需要它：验收"字到底进没进编辑框""密码框到底拒没拒""按钮到底点没点"
 * 必须有一个**独立于引擎**的观察者。这个窗口把每次收到的输入原样写进状态文件，
 * 测试脚本读那个文件来判断，而不是读引擎自己回报的结果——否则就是自说自话。
 *
 * 用法:  testbed-window.exe <状态文件路径>
 *
 * 界面文字**一律用 ASCII**：CreateWindowExA 走的是系统 ANSI 代码页(本机 GBK)，
 * 直接塞 UTF-8 字节会变成乱码标题，测试脚本就再也搜不到这个窗口了。
 * 脚本按类名 DshTestbedWindow 找它，比按标题更稳。
 *
 * 状态文件每次事件整份覆盖重写，格式是一行一条 key=value：
 *   alive=1                进程活着
 *   clicks=<n>             "点我"按钮被真正点到的次数
 *   edit1=<原样文本>        普通编辑框内容(验证 WM_CHAR / 真注入真的到了)
 *   edit2_len=<n>          密码框内容长度(只报长度，内容不落盘)
 *   focus=<十进制 hwnd>     当前焦点窗口
 *   tick=<n>               定时器心跳，用来自证文件是新的
 *   drags=<n>              绑定在窗口背景上的拖拽次数
 *   drag_moves=<n>         最近一次拖拽期间收到的 WM_MOUSEMOVE 次数
 *   drag_total_moves=<n>   历次拖拽收到的 WM_MOUSEMOVE 总数
 *   drag_start=<x>,<y>     最近一次拖拽的起点(客户区坐标)
 *   drag_end=<x>,<y>       最近一次拖拽的终点(客户区坐标)
 *
 * drag_moves 是"拖拽插值真的发出去了"的硬证据：只发 down/up 而不发中间移动，
 * 这个数会是 0；引擎的轨迹插值生效时它应该接近 steps 的数量。
 *
 * 只用 Win32 API(不碰 CRT 的文件/格式化)，避免 mingw 的 __USE_MINGW_ANSI_STDIO
 * 之类编译期差异把验收工具本身变成变量。
 */

#include <windows.h>

#define ID_EDIT_NORMAL   1001
#define ID_EDIT_PASSWORD 1002
#define ID_BUTTON        1003
#define ID_STATIC        1004

#define TIMER_HEARTBEAT  1

// 客户区布局：上面是控件区，y >= 130 是自由拖拽区(顺便画一行大字给 OCR 认)。
#define CTRL_AREA_BOTTOM 130
#define CLIENT_W         620
#define CLIENT_H         420

static HWND g_edit1 = NULL;
static HWND g_edit2 = NULL;
static HWND g_button = NULL;
static HWND g_static = NULL;
static int g_clicks = 0;
static int g_tick = 0;
static char g_path[MAX_PATH];

static int g_dragging = 0;
static int g_drags = 0;
static int g_drag_moves = 0;
static int g_drag_total_moves = 0;
static POINT g_drag_start;
static POINT g_drag_end;

/* 消息循环里看到的按键总数与最后一个虚拟键码。
 * 用途：把"引擎到底有没有把按键投进来"和"投进来的是什么键"变成可读的数字——
 * 少了这一项，就只能靠猜（焦点没动到底是没收到 F2，还是收到 F2 但 SetFocus 失败）。 */
static int g_keys = 0;
static unsigned int g_last_vk = 0;

/* 把控件文本按 UTF-8 取出来。
 *
 * 为什么不用 GetWindowTextA：那一步会按系统 ANSI 代码页(本机 GBK)转码，
 * 测试脚本按 UTF-8 读文件就会把中文读成乱码——看起来像引擎打错了字，
 * 其实是靶窗自己写文件时把编码搞坏了，属于测量工具引入的假故障。 */
static void read_text_utf8(HWND hwnd, char *out, int cap)
{
    WCHAR wide[512];
    int n;

    out[0] = '\0';
    if (hwnd == NULL || cap <= 1) {
        return;
    }
    n = GetWindowTextW(hwnd, wide, 512);
    if (n <= 0) {
        return;
    }
    WideCharToMultiByte(CP_UTF8, 0, wide, n, out, cap - 1, NULL, NULL);
    out[cap - 1] = '\0';
}

static void dump_state(void)
{
    char e1[1024];
    char buf[4096];
    char tmp[MAX_PATH];
    HANDLE h;
    DWORD written = 0;
    int pwd_len;

    if (g_path[0] == '\0') {
        return;
    }
    if (lstrlenA(g_path) + 5 >= (int)sizeof(tmp)) {
        return;
    }

    read_text_utf8(g_edit1, e1, (int)sizeof(e1));
    /* 密码框只报长度，内容不落盘；长度按字符数算，和编码无关。 */
    pwd_len = (g_edit2 != NULL) ? GetWindowTextLengthW(g_edit2) : 0;

    wsprintfA(buf,
              "alive=1\r\nclicks=%d\r\ntick=%d\r\nedit1=%s\r\nedit2_len=%d\r\nfocus=%lu\r\n"
              "drags=%d\r\ndrag_moves=%d\r\ndrag_total_moves=%d\r\n"
              "drag_start=%ld,%ld\r\ndrag_end=%ld,%ld\r\n"
              "keys=%d\r\nlast_vk=%u\r\n",
              g_clicks, g_tick, e1, pwd_len,
              (unsigned long)(ULONG_PTR)GetFocus(),
              g_drags, g_drag_moves, g_drag_total_moves,
              (long)g_drag_start.x, (long)g_drag_start.y,
              (long)g_drag_end.x, (long)g_drag_end.y,
              g_keys, g_last_vk);

    /* 先写临时文件，再整体替换。
     *
     * 直接 CREATE_ALWAYS 覆盖原文件会留下一个"已截断、还没写完"的窗口，
     * 读的人正好撞上就读到空文件——测试脚本会把它当成"状态里没这个字段"，
     * 于是报出一个跟被测代码毫无关系的假故障。MoveFileEx 替换是原子的。 */
    lstrcpynA(tmp, g_path, (int)sizeof(tmp) - 4);
    lstrcatA(tmp, ".tmp");

    h = CreateFileA(tmp, GENERIC_WRITE, 0, NULL,
                    CREATE_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (h == INVALID_HANDLE_VALUE) {
        return;
    }
    WriteFile(h, buf, (DWORD)lstrlenA(buf), &written, NULL);
    CloseHandle(h);
    MoveFileExA(tmp, g_path, MOVEFILE_REPLACE_EXISTING);
}

/* 画一行大字 + 拖拽状态。大字是给 Windows OCR 认的：字号够大，
 * 识别成功率才不依赖运气；拖拽状态画出来是为了人肉核对。 */
static void paint_background(HWND hwnd)
{
    PAINTSTRUCT ps;
    HDC dc = BeginPaint(hwnd, &ps);
    RECT rc;
    HFONT big, small, old;

    GetClientRect(hwnd, &rc);
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, RGB(0, 0, 0));

    big = CreateFontA(-64, 0, 0, 0, FW_BOLD, FALSE, FALSE, FALSE,
                      DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
                      CLEARTYPE_QUALITY, DEFAULT_PITCH, "Arial");
    old = (HFONT)SelectObject(dc, big);
    TextOutA(dc, 20, 145, "OCR TEST 12345", 14);
    SelectObject(dc, old);
    DeleteObject(big);

    small = CreateFontA(-24, 0, 0, 0, FW_NORMAL, FALSE, FALSE, FALSE,
                        DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS,
                        CLEARTYPE_QUALITY, DEFAULT_PITCH, "Consolas");
    old = (HFONT)SelectObject(dc, small);
    {
        char line[160];
        wsprintfA(line, "drags=%d moves=%d total=%d", g_drags, g_drag_moves, g_drag_total_moves);
        TextOutA(dc, 20, 230, line, lstrlenA(line));
        wsprintfA(line, "drag end=%ld,%ld", (long)g_drag_end.x, (long)g_drag_end.y);
        TextOutA(dc, 20, 265, line, lstrlenA(line));
    }
    TextOutA(dc, 20, 310, "DRAG AREA BELOW THIS LINE", 25);
    SelectObject(dc, old);
    DeleteObject(small);

    EndPaint(hwnd, &ps);
}

static LRESULT CALLBACK WndProc(HWND hwnd, UINT msg, WPARAM wp, LPARAM lp)
{
    switch (msg) {
    case WM_CREATE: {
        HINSTANCE hi = ((LPCREATESTRUCT)lp)->hInstance;

        g_edit1 = CreateWindowExA(
            WS_EX_CLIENTEDGE, "EDIT", "",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL,
            10, 10, 280, 24, hwnd, (HMENU)ID_EDIT_NORMAL, hi, NULL);

        g_edit2 = CreateWindowExA(
            WS_EX_CLIENTEDGE, "EDIT", "",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_PASSWORD | ES_AUTOHSCROLL,
            10, 44, 280, 24, hwnd, (HMENU)ID_EDIT_PASSWORD, hi, NULL);

        g_button = CreateWindowExA(
            0, "BUTTON", "ClickMe",
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_PUSHBUTTON,
            10, 80, 90, 26, hwnd, (HMENU)ID_BUTTON, hi, NULL);

        g_static = CreateWindowExA(
            0, "STATIC", "Idle",
            WS_CHILD | WS_VISIBLE,
            110, 86, 180, 20, hwnd, (HMENU)ID_STATIC, hi, NULL);

        SetTimer(hwnd, TIMER_HEARTBEAT, 150, NULL);
        dump_state();
        return 0;
    }
    case WM_KEYDOWN:
        /* F2：把焦点移到密码框上。用来测"焦点在密码框里时，引擎必须拒绝输入"。
         * 不用 Tab 是因为 Tab 导航要靠对话框管理器，普通窗口上不一定生效——
         * 测试工具的导航方式不该成为被测结论的变量。 */
        if (wp == VK_F2 && g_edit2 != NULL) {
            SetFocus(g_edit2);
            dump_state();
            return 0;
        }
        break;
    case WM_COMMAND:
        if (LOWORD(wp) == ID_BUTTON && HIWORD(wp) == BN_CLICKED) {
            g_clicks++;
            SetWindowTextA(g_static, "Clicked");
            SetWindowTextA(hwnd, "DSH Screen-Control Testbed [clicked]");
            dump_state();
        }
        return 0;
    case WM_RBUTTONDOWN:
        /* 右键点客户区背景 = 把焦点移到密码框。用来测"焦点在密码框里时引擎必须拒绝输入"。
         * 不用按键触发是因为本机开着中文输入法：F2 之类的键会被 IME 变成
         * VK_PROCESSKEY(0xE5) 交给输入法，靶窗根本收不到，测试就变成变量了
         * （2026-10-16 实测：发了 3 个按键，收上来的 last_vk 是 229 = VK_PROCESSKEY）。 */
        if (g_edit2 != NULL) {
            SetFocus(g_edit2);
            dump_state();
        }
        return 0;
    case WM_LBUTTONDOWN:
        /* 只统计落在窗口背景上的按下：点在子控件上的会发给子控件，不会到这里。 */
        g_dragging = 1;
        g_drag_moves = 0;
        g_drag_start.x = (short)LOWORD(lp);
        g_drag_start.y = (short)HIWORD(lp);
        g_drag_end = g_drag_start;
        SetCapture(hwnd);
        return 0;
    case WM_MOUSEMOVE:
        if (g_dragging) {
            g_drag_moves++;
            g_drag_total_moves++;
            g_drag_end.x = (short)LOWORD(lp);
            g_drag_end.y = (short)HIWORD(lp);
            InvalidateRect(hwnd, NULL, FALSE);
        }
        return 0;
    case WM_LBUTTONUP:
        if (g_dragging) {
            g_dragging = 0;
            g_drag_end.x = (short)LOWORD(lp);
            g_drag_end.y = (short)HIWORD(lp);
            g_drags++;
            ReleaseCapture();
            InvalidateRect(hwnd, NULL, TRUE);
            dump_state();
        }
        return 0;
    case WM_PAINT:
        paint_background(hwnd);
        return 0;
    case WM_TIMER:
        g_tick++;
        dump_state();
        return 0;
    case WM_CLOSE:
        DestroyWindow(hwnd);
        return 0;
    case WM_DESTROY:
        KillTimer(hwnd, TIMER_HEARTBEAT);
        PostQuitMessage(0);
        return 0;
    default:
        break;
    }
    return DefWindowProcA(hwnd, msg, wp, lp);
}

int WINAPI WinMain(HINSTANCE hi, HINSTANCE prev, LPSTR cmdline, int show)
{
    WNDCLASSA wc;
    HWND hwnd;
    MSG msg;
    RECT rc;

    (void)prev;

    /* DPI 感知：不声明的话窗口会被系统按 150% 缩放，大字被插值糊掉，
     * OCR 的识别率就变成一个说不清的变量了。 */
    SetProcessDPIAware();

    g_path[0] = '\0';
    if (cmdline != NULL && cmdline[0] != '\0') {
        if (cmdline[0] == '"') {
            char *end = cmdline + 1;
            while (*end != '\0' && *end != '"') {
                end++;
            }
            if ((size_t)(end - cmdline - 1) < sizeof(g_path)) {
                lstrcpynA(g_path, cmdline + 1, (int)(end - cmdline));
            }
        } else {
            lstrcpynA(g_path, cmdline, (int)sizeof(g_path));
        }
    }

    ZeroMemory(&wc, sizeof(wc));
    wc.lpfnWndProc = WndProc;
    wc.hInstance = hi;
    wc.hCursor = LoadCursorA(NULL, IDC_ARROW);
    wc.hbrBackground = (HBRUSH)(COLOR_BTNFACE + 1);
    wc.lpszClassName = "DshTestbedWindow";
    if (RegisterClassA(&wc) == 0) {
        return 1;
    }

    /* 窗口尺寸按**客户区**算：拖拽区要够大，不然插值轨迹会撞到窗口边界。 */
    rc.left = 0;
    rc.top = 0;
    rc.right = CLIENT_W;
    rc.bottom = CLIENT_H;
    AdjustWindowRect(&rc, WS_OVERLAPPEDWINDOW, FALSE);

    /* WS_EX_CONTROLPARENT：IsDialogMessage 只对"对话框或带这个样式的窗口"处理
     * Tab/方向键导航。少了它，Tab 不会在控件之间移动焦点。 */
    hwnd = CreateWindowExA(WS_EX_CONTROLPARENT, "DshTestbedWindow",
                           "DSH Screen-Control Testbed",
                           WS_OVERLAPPEDWINDOW,
                           CW_USEDEFAULT, CW_USEDEFAULT,
                           rc.right - rc.left, rc.bottom - rc.top,
                           NULL, NULL, hi, NULL);
    if (hwnd == NULL) {
        return 2;
    }

    /* 用 SW_SHOWNOACTIVATE 而不是 nCmdShow：ShowWindow(SW_SHOWNORMAL) 会把靶窗
     * 顶成前台窗口，而"不抢用户焦点"正是这个插件的前提——测试工具自己先把前提
     * 破坏了，测出来的结论就不算数。这样也让键盘 SendInput 闸门能被真正验到。 */
    (void)show;
    ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    UpdateWindow(hwnd);

    while (GetMessageA(&msg, NULL, 0, 0) > 0) {
        /* 统计所有按键消息：这是"引擎的按键到底有没有进来"的唯一直接证据。 */
        if (msg.message == WM_KEYDOWN || msg.message == WM_SYSKEYDOWN) {
            g_keys++;
            g_last_vk = (unsigned int)msg.wParam;
        }
        /* F2 必须在**消息循环**里拦，而不是在 WndProc 里：键盘消息发给的是焦点控件
         * (编辑框)，顶层窗口收不到 WM_KEYDOWN(VK_F2)，所以放在 WndProc 里那次
         * 实测根本没触发。循环里能看到本线程的所有消息，这里拦才可靠。 */
        if (msg.message == WM_KEYDOWN && msg.wParam == VK_F2 && g_edit2 != NULL) {
            SetFocus(g_edit2);
            dump_state();
            continue;
        }
        /* IsDialogMessage 让 Tab / 方向键在控件之间移动焦点。 */
        if (IsDialogMessageA(hwnd, &msg)) {
            continue;
        }
        TranslateMessage(&msg);
        DispatchMessageA(&msg);
    }
    return 0;
}
