# Gochi Browser — 功能说明

轻量级、键盘驱动的浏览器。技术栈:Tauri + Rust + macOS WebKit(WKWebView,经 wry),无前端打包器,UI 全部是本地静态 HTML。

本文件持续维护,记录所有功能的实现逻辑。

---

## 1. 总体架构

单个原生窗口(`main`)里挂多个 webview(需要 `--features unstable`,multiwebview):

| label | 作用 | 信任级 |
|---|---|---|
| `chrome` | 顶部标签栏 + 地址栏 + 查找栏(自绘),与 macOS 红绿灯同行 | 可信(`core:default` + `start_dragging`) |
| `tab-N` | 每个标签一个外部网页 webview | 不受信任(无 capability) |
| `overlay` | 按需创建的居中面板(命令面板/标签切换/地址/书签/历史),创建即置顶、关闭即销毁 | 可信(`core:default`) |

主入口 `src-tauri/src/main.rs`。状态 `BrowserState`(Mutex)管理:标签列表、当前标签、id 计数器、当前 overlay 模式、已关闭标签栈、每标签缩放、加载代次、devtools 标记。

数据目录 `~/.config/gochi-browser/`:

| 文件 | 内容 |
|---|---|
| `settings.json` | 用户配置(见 §3) |
| `session.json` | 上次退出时打开的标签 URL + 当前下标(会话恢复) |
| `bookmarks.json` | 本浏览器自己的书签 |
| `history.json` | 本浏览器自己的访问记录(去重,按时间倒序,上限 5000) |

### 线程安全规则(踩过死锁坑)

- Tauri 命令默认跑在主线程,**从主线程 `add_child`/`close` webview 会死锁**(自定义协议 / IPC 互相等待)。所以凡是要创建/销毁 webview 的命令一律 `async`(跑在 worker,由 Tauri 调度回主线程);菜单事件处理器也用 `std::thread::spawn` 包一层。例外:`setup` 阶段(事件循环尚未启动)可以同步建 webview。
- **绝不持有 `BrowserState` 锁时调用 Tauri API**:一律「锁内取快照 → 放锁 → 再 emit/操作 webview」。
- 文件写入(history/session)在独立线程完成,history 写入用 `HISTORY_LOCK` 串行化。

### capability 划分(`src-tauri/capabilities/`)

- `default.json`:给 `chrome` 和 `overlay` → `core:default` + `core:window:allow-start-dragging`(标签栏空白处拖动窗口;双击缩放由 `core:default` 里的 `internal_toggle_maximize` 负责)。
- 标签 webview 不做任何授权(网页理论上能看到 `window.__TAURI__` 全局,这是原型级已知限制)。

---

## 2. 窗口与 UI

- **自绘标签栏**:`decorations: true` + `titleBarStyle: Overlay`。Overlay 让 content view 铺满整个窗口(fullSizeContentView),于是自绘的 `chrome` webview(高 38pt)画在窗口最顶端,与原生红绿灯**同一行并列**。
- **红绿灯浮在标签栏之上**:红绿灯始终画在 content view 之上;标签栏左侧预留 80pt 让位。
- **隐藏原生标题文字**:创建标签栏后用 objc2 对 NSWindow 设 `titleVisibility = hidden`(红绿灯保留);窗口标题仍同步为当前标签标题(供菜单/调度中心用)。
- **网页在标签栏下方渲染**:标签 webview 定位在 `y = 38` 起。
- 标签栏空白处可拖动窗口;双击空白处缩放窗口(`data-tauri-drag-region`)。
- **网页区域圆角**:新建标签时通过 `with_webview` + objc2 给 WKWebView 图层设 `cornerRadius=15pt` + `masksToBounds`。
- 窗口 resize 时 `layout()` 把标签栏、所有标签 webview 和 overlay 重新定位/缩放。

启动:读配置 → 建窗口 → 设 Overlay 标题栏 → 建标签栏 → **恢复上次会话**(或打开首页)。

### 标签栏(`ui/chrome.html`)

一行内有三种模式,靠 body class 切换:

| 模式 | 触发 | 内容 |
|---|---|---|
| `tabs`(默认) | 启动 / `esc` / 提交后 | 标签列表 + `+` 新建 + 拖动空白区 |
| `omnibox` | `⌘L`、命令面板 | 内联地址栏输入框 |
| `find` | `⌘F`、命令面板 | 内联查找输入框 |

- 标签:左边方块首字母(站点 host 首字母)、标题、hover 出 `×`;**加载中**首字母变成旋转圈,点它停止加载;中键关闭;`+` 新建。
- 顶部条底部有一条 2px 进度条,来自 `load` 事件的 `estimatedProgress`。
- 书签增删会弹一个 1.4s 的 toast(`ui` 事件)。

### 数据流

- Rust 侧任何标签变化都会 `emit_tabs()` → `tabs` 事件 → 标签栏重绘。
- **加载状态由 `start_progress()` 轮询线程独占管理**:每 120ms 用 objc 直接读 WKWebView 的 `isLoading` / `estimatedProgress` / 主框架 `URL`,据此更新标签的 `loading`/`url` 并发 `load` 事件;加载结束后记录历史、存会话。代次 `load_gen` 保证同一标签只有一个轮询线程。
  - 刻意**不注册 `on_navigation`**:WebKit 的导航策略回调对**每一帧**(iframe/广告/被拦截的请求)都会触发,在那里改 URL/`loading` 会把标签 URL 污染成 iframe 地址、并让转圈图标永远停不下来。主框架真实状态一律以 WKWebView 属性为准。
  - 轮询线程读不到 webview 时最多重试 8 次后也会收尾,保证不会无限转圈。
- 地址栏/查找栏由 Rust 主动激活:`tell_chrome(app, "omnibox"|"find")` 发 `ui` 事件并 focus 标签栏(书签 toast 用 `notify_chrome`,不抢焦点)。

---

## 3. 配置 `~/.config/gochi-browser/settings.json`

首次运行自动生成模板,缺失/解析失败回退默认值。**改动重启生效**(菜单在启动时按配置构建)。

```json
{
  "search": "https://www.bing.com/search",
  "home": "https://duckduckgo.com/",
  "restore_session": true,
  "shortcuts": {
    "command_palette": "CmdOrCtrl+P",
    "open_omnibox": "CmdOrCtrl+L",
    "open_address": "CmdOrCtrl+R",
    "open_switcher": "CmdOrCtrl+O",
    "open_bookmarks": "CmdOrCtrl+B",
    "open_history": "CmdOrCtrl+Y",
    "toggle_bookmark": "CmdOrCtrl+D",
    "find_in_page": "CmdOrCtrl+F",
    "new_tab": "CmdOrCtrl+T",
    "close_tab": "CmdOrCtrl+W",
    "reopen_closed_tab": "CmdOrCtrl+Shift+T",
    "next_tab": "Ctrl+Tab",
    "prev_tab": "Ctrl+Shift+Tab",
    "tab_1": "CmdOrCtrl+1",
    "tab_9": "CmdOrCtrl+9",
    "reload": "CmdOrCtrl+Shift+R",
    "force_reload": "CmdOrCtrl+Alt+R",
    "back": "Alt+Left",
    "forward": "Alt+Right",
    "home": "CmdOrCtrl+Shift+H",
    "zoom_in": "CmdOrCtrl+=",
    "zoom_out": "CmdOrCtrl+-",
    "zoom_reset": "CmdOrCtrl+0",
    "toggle_devtools": "CmdOrCtrl+Alt+I"
  }
}
```

- `search`:搜索引擎。a) 结果页 base URL,查询自动追加 `?q=`;b) 含 `{query}` 占位符的模板。
- `home`:起始页 / 新标签页。
- `restore_session`:启动时恢复上次打开的标签(默认 true)。
- `shortcuts`:动作 id → 加速键。`null` / `""` / `"none"` 禁用某个快捷键。

动作注册表在 `ACTIONS`(`main.rs`),每个动作有 id/标题/默认键/菜单归属(`None` / `Commands` / `Tabs`)。**菜单事件和命令面板都汇聚到同一个 `dispatch_action()`**。

### 快捷键总览

| 功能 | 默认键 |
|---|---|
| 命令面板 | ⌘P |
| 内联地址栏 | ⌘L |
| 地址面板(带建议) | ⌘R |
| 标签切换器 | ⌘O |
| 书签 / 历史 | ⌘B / ⌘Y |
| 收藏当前页(切换) | ⌘D |
| 页内查找 | ⌘F |
| 新建 / 关闭 / 重开标签 | ⌘T / ⌘W / ⌘⇧T |
| 下一个 / 上一个标签 | ⌃⇥ / ⌃⇧⇥ |
| 跳到第 N / 最后一个标签 | ⌘1..⌘8 / ⌘9 |
| 刷新 / 强制刷新 | ⌘⇧R / ⌘⌥R |
| 后退 / 前进 | ⌥← / ⌥→ |
| 回首页 | ⌘⇧H |
| 放大 / 缩小 / 实际大小 | ⌘= / ⌘- / ⌘0 |
| Web Inspector(debug) | ⌘⌥I |

---

## 4. `⌘P` 命令面板(command palette)

- 居中 overlay(`palette.html`),列出**所有动作**(id、标题、快捷键)。
- 输入即过滤(按标题/id),`↑↓` 选择,`↵` 执行;`esc` 或点遮罩关闭;`⌘P` 再按一次关闭。
- 执行 = `run_action(id)` → `dispatch_action()`。

动作分三类:**导航/页面**(地址栏、查找、缩放、刷新、后退前进、打印、Web Inspector、清空历史)、**标签**(新建/关闭/重开/复制/关闭其他/切换/跳转)、**编辑与窗口**(Undo/Redo/Cut/Copy/Paste/Select All、最小化、全屏、退出、完全磁盘访问设置)。

---

## 5. 标签管理

- 每个标签一个独立 webview(不是 iframe),label 递增 `tab-N`,能加载任意站点且互不影响。
- **标签栏**见 §2;点击标签 `switch_to`,点 `×`/中键 `close_tab`,点 `+` `new_tab`。
- 切换 = `show()` 当前 / `hide()` 其余,**不销毁** → 保留滚动位置、登录态、播放中的视频等。
- **已关闭标签栈**:关闭时压栈(最多 25),`⌘⇧T` 重开(会先显示记忆的标题)。
- **`target="_blank"` / `window.open`** 通过 `on_new_window` 转成**本窗口的新标签**(`NewWindowResponse::Deny` + `create_tab`),否则 WebKit 会静默丢弃这些点击。
- **复制标签 / 关闭其他标签**:命令面板或 Tabs 菜单。
- 关闭**最后一个标签会关掉窗口**。
- **会话恢复**:标签增删/切换/加载完成时把 URL 列表 + 当前下标写入 `session.json`;启动时若 `restore_session` 且文件非空则逐个恢复并切回原当前标签。
- 切换标签/标题变化时把窗口标题同步为当前标签标题。

---

## 6. 地址栏 / 搜索逻辑

两种入口:

- **内联地址栏 `⌘L`**:在标签栏内直接输入,`↵` 当前标签打开,`⌘↵` 新标签打开,`esc` 取消并回网页。
- **地址面板 `⌘R`**(`address.html`):居中面板,输入时调用 `suggest(query)` 拿**实时建议**(自己的书签 + 自己的历史,最多 12 条),`↑↓` 选择、`↵` 当前标签、`⌘↵` 新标签;无选中时按输入内容提交。

两者最终都走 `normalize(input, search, home)`:

1. 空输入 → `home`。
2. 本身是合法 URL 且 scheme 为 `http/https/tauri/about/file` → 原样用。
3. 像域名(无空白、含 `.` 或 `localhost/127.0.0.1`)→ 前面补 `https://`。
4. 都不是 → 走搜索引擎(含 `{query}` 则替换,否则当结果页 base 追加 `?q=`)。

Rust 侧对应命令:`submit_url`(内联框,归一化后打开)、`suggest`(建议)、`open_url`(建议项/书签/历史项,严格 URL)。

---

## 7. 页内查找 `⌘F`

- `⌘F`(或命令面板)让标签栏切到 `find` 模式,输入框获得焦点。
- 边输入边调用 `find_query(query, forward, reset=true)`;`↵` 下一个、`⇧↵` 上一个(此时 `reset=false`,利用当前选区往前/后找)。
- 实现:对当前标签 `eval` 一段 JS → `window.find(query, false, backwards, true, false, true, false)`;`reset` 时先 `getSelection().removeAllRanges()`。
- 特意**不给网页 focus**,这样查找栏能持续接收键盘输入;`esc` 退出并 `focus_page`。

---

## 8. 缩放 / 打印 / Web Inspector

- 缩放:每标签独立,档位 `0.5 0.67 0.8 0.9 1 1.1 1.25 1.5 1.75 2 2.5 3`,`set_zoom` 作用于 WKWebView,状态存在 `BrowserState.zoom`。
- 打印:`print_page` → WKWebView `print()`(命令面板)。
- Web Inspector:`toggle_devtools` → `open_devtools` / `close_devtools`(debug 构建或 `--features devtools`),打开状态存在 `BrowserState.devtools`。

---

## 9. 书签(Safari 书签 + 自建书签)

- 数据源两块:
  - **自己的书签** `bookmarks.json`,`⌘D` 切换当前页收藏状态(新增/移除);面板里可点 `×` 删除(`remove_bookmark`)。
  - **Safari 书签** `~/Library/Safari/Bookmarks.plist`(二进制 plist,只读)。用 `plist` 解析树,`WebBookmarkTypeLeaf` 取 `URLString` + `URIDictionary.title`,`WebBookmarkTypeList` 递归;`com.apple.ReadingList` 显示为「Reading List」。
- 面板 `⌘B`(`picker.html`):自己书签(副标题「Bookmarks」,可删)在前,Safari 书签(副标题为文件夹名)在后。交互同切换器,`↵` 当前标签、`⌘↵` 新标签。
- **前提**:读 Safari 书签需要完全磁盘访问权限(TCC)。未授权时若自己没有书签,面板显示提示 + 按钮「开启完全磁盘访问设置」(`open x-apple.systempreferences:...Privacy_AllFiles`)。

---

## 10. 历史记录(自建记录 + Safari 历史)

- **自己的记录** `history.json`:每次页面加载完成(`PageLoadEvent::Finished`)取当前 URL + 标题,异步落盘(线程内 `HISTORY_LOCK` 串行)。按 URL 去重、更新访问次数与时间、按时间倒序、上限 5000。只记 `http/https`。
- **面板 `⌘Y`**(`picker.html`):自己的历史在前,再并入 Safari 历史(去重),按时间倒序,最多 2000 条。面板底部有 `Clear History` 按钮(`clear_history`,也可从命令面板调用)。
- **Safari 历史**(只读补充):`~/Library/Safari/History.db`(SQLite)。先连同 `-wal`/`-shm` **拷到临时目录**再查,避免抢 WAL 锁。SQL 联合 `history_visits × history_items` 取最近记录;`visit_time` 是 Core Foundation 绝对时间(2001 起秒),`+978307200` 转 Unix。同样需要完全磁盘访问权限。

---

## 11. 标签切换器 `⌘O`

- 居中 overlay(`switcher.html`),列出全部标签(标题/URL),可输入过滤。
- 当前标签带绿点标记,打开时默认选中当前标签。
- `↑↓` 选择,`↵` 切换(`switch_to`),`esc`/点遮罩关闭,`⌘O` 再按关闭。

---

## 12. overlay 机制(统一)

命令面板 / 标签切换 / 地址 / 书签 / 历史共用同一个 overlay webview:

- `open_overlay(mode)`:若已开同款 → 关闭(toggle);已开别款 → 先关再开;未开 → 建一个**全窗口透明** webview(`transparent(true)`,label `overlay`)加载对应页面 → 记录模式 → focus。
- 因为**按需创建在全部标签之后**,它永远在最上层(也会盖住标签栏);关闭即 `close()` 销毁。
- 面板列表数据或来自 `get_tabs` / `list_actions` / `picker_items` / `suggest`,取决于页面。
- 执行非 overlay 动作前先关闭 overlay。

---

## 13. 原生菜单

macOS 菜单栏(快捷键走这里,能在网页聚焦时生效):

- **Gochi Browser**:About / Hide / Hide Others / Show All / Quit
- **Edit**:原生 Undo/Redo/Cut/Copy/Paste/Select All(文本编辑可靠来源)
- **Commands**:导航、面板、页面、缩放、打印等动作
- **Tabs**:新建/关闭/重开/复制/关闭其他/上下切换/`Tab 1..9`
- **Window**:Minimize / Maximize / Fullscreen

用户自定义的快捷键最终都会反映到菜单(启动时构建),菜单事件按动作 id 派发到 `dispatch_action`。

---

## 14. 运行与打包

```bash
cargo tauri dev              # 开发
cargo tauri build            # 打包 .app
cargo tauri build --features devtools   # 打包并保留 Web Inspector
```

- 依赖:`tauri`(带 `unstable`)、`serde`、`serde_json`、`plist`、`rusqlite`(bundled);macOS 额外 `objc2`。
- 可选 feature `devtools`(透传 `tauri/devtools`)。
- 打包后使用 Safari 书签/历史功能,需到 系统设置 → 隐私与安全性 → 完全磁盘访问权限 勾选本 app;dev 版换构建签名会变,FDA 授权可能需重新勾。

---

## 已知限制

- 标签/内容 webview 会注入 `window.__TAURI__`,外部页面理论上能调用自定义命令(原型级安全取舍;目前这些命令只影响导航等低风险操作)。
- 命令面板里的 `paste`(经 `execCommand`)在 WebKit 下可能无效;键盘复制粘贴请用原生 `⌘C/⌘V`。
- 编辑动作(undo/copy/paste…)的键位由 macOS 原生 Edit 菜单决定,不在 `settings.json` 的可自定义范围内。
- 内联地址栏没有下拉建议(标签栏高度固定 38pt,放不下);需要建议请用 `⌘R` 地址面板。
- 原生菜单快捷键是全局的,所以在地址栏/查找栏里按 `⌥←/⌥→` 会触发后退/前进,而不是按词移动光标。
- 页内查找用 `window.find`,不显示「第 n / 共 m 个」计数,也不会高亮全部匹配。
- 标签栏没有右键菜单(纯键盘 + 命令面板/Tabs 菜单替代)。
- iCloud 标签(macOS 15 起不再有本地 `CloudTabs.db`)、Safari WebExtensions(`WKWebExtension` 需 fork wry 才可挂到 configuration)暂不支持。
- 下载管理器、页面内右键菜单定制、多窗口、扩展暂未实现。
