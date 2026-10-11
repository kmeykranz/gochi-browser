# Gochi Browser — 功能说明

轻量级、键盘驱动的浏览器。技术栈:Tauri + Rust + macOS WebKit(WKWebView,经 wry),无前端打包器,UI 全部是本地静态 HTML。

本文件持续维护,记录所有功能的实现逻辑。

---

## 1. 总体架构

单个原生窗口(`main`)里挂多个 webview(需要 `--features unstable`,multiwebview):

| label | 作用 | 信任级 |
|---|---|---|
| `tab-N` | 每个标签一个外部网页 webview | 不受信任(无 capability) |
| `overlay` | 按需创建的居中面板(命令面板/标签切换/地址/书签/历史),创建即置顶、关闭即销毁 | 可信(`core:default`) |

主入口 `src-tauri/src/main.rs`。状态 `BrowserState`(Mutex)管理:标签列表、当前标签、id 计数器、当前打开的 overlay 模式。

### 线程安全规则(踩过死锁坑)

- Tauri 命令默认跑在主线程,**从主线程 `add_child`/`close` webview 会死锁**(自定义协议 / IPC 互相等待)。所以凡是要创建/销毁 webview 的命令一律 `async`(跑在 worker,由 Tauri 调度回主线程);菜单事件处理器也用 `std::thread::spawn` 包一层。
- **绝不持有 `BrowserState` 锁时调用 Tauri API**:一律「锁内取快照 → 放锁 → 再 emit/操作 webview」。

### capability 划分(`src-tauri/capabilities/`)

- `default.json`:只给 `overlay` → `core:default`。
- 标签 webview 不做任何授权(网页理论上能看到 `window.__TAURI__` 全局,这是原型级已知限制)。

---

## 2. 窗口与 UI

- **无自绘 UI**:`decorations: true` + `titleBarStyle: Transparent` + 空标题。保留 macOS 原生红黄绿按钮,可拖动/双击缩放。
- **网页在标题栏下方渲染**(`Window` 的子 webview 相对内容视图定位,而内容视图天然在标题栏之下),不会被顶部盖住。
- 标题栏是透明的,显示的是窗口背景色 → 按系统深浅色设置(`TITLEBAR_LIGHT` / `TITLEBAR_DARK`)。
- **网页区域圆角**:新建标签时通过 `with_webview` + objc2 给 WKWebView 的图层设 `cornerRadius=10pt` 且 `masksToBounds`(`apply_corner_radius`),四角被裁成圆角,露出窗口背景色。
- 窗口 resize 时 `layout()` 把所有标签 webview 和 overlay 重新定位/缩放。
- 无任何按钮;所有操作靠快捷键 + `⌘P` 命令面板。

启动:读配置 → 建窗口 → 设标题栏 → 开一个标签页(打开配置的首页)。

---

## 3. 配置 `~/.config/gochi-browser/settings.json`

首次运行自动生成模板(含全部可自定义项),缺失/解析失败回退默认值。**改动重启生效**(菜单在启动时按配置构建)。

```json
{
  "search": "https://www.bing.com/search",
  "home": "https://duckduckgo.com/",
  "shortcuts": {
    "command_palette": "CmdOrCtrl+P",
    "open_address": "CmdOrCtrl+R",
    "open_switcher": "CmdOrCtrl+O",
    "open_bookmarks": "CmdOrCtrl+B",
    "open_history": "CmdOrCtrl+Y",
    "new_tab": "CmdOrCtrl+T",
    "close_tab": "CmdOrCtrl+W",
    "next_tab": "Ctrl+Tab",
    "prev_tab": "Ctrl+Shift+Tab",
    "reload": "CmdOrCtrl+Shift+R",
    "back": "Alt+Left",
    "forward": "Alt+Right"
  }
}
```

- `search`:搜索引擎。两种写法:a) 结果页 base URL,查询自动追加为 `?q=`(如 `https://www.bing.com/search`);b) 含 `{query}` 占位符的模板,占位符替换为 URL 编码后的查询词。
- `home`:起始页 / 新标签页。
- `shortcuts`:动作 id → 加速键。值设为 `null` / `""` / `"none"` 可禁用某个快捷键。

动作注册表在 `ACTIONS`(`main.rs`),每个动作有 id/标题/默认键/是否进菜单。菜单事件和命令面板都汇聚到同一个 `dispatch_action()`。

---

## 4. `⌘P` 命令面板(command palette)

- 居中 overlay(`palette.html`),列出**所有动作**(id、标题、快捷键;编辑类动作来自原生 Edit 菜单,面板里无快捷键展示)。
- 输入即过滤(按标题/id),`↑↓` 选择,`↵` 执行;`esc` 或点遮罩关闭;`⌘P` 再按一次关闭。
- 执行 = `run_action(id)` → `dispatch_action()`。

可执行的动作两类:

**浏览器/窗口动作**:New Tab、Close Tab、Next/Previous Tab、Reload、Back、Forward、Open Address、Go to Tab、Open Bookmarks、Open History、Command Palette、Minimize Window、Toggle Fullscreen、Quit、Grant Full Disk Access…

**编辑动作**(作用于当前网页,通过 `eval("document.execCommand(...)")`):Undo / Redo / Cut / Copy / Paste / Select All。键盘层面的编辑快捷键由原生 Edit 菜单负责(更可靠);面板里执行为对网页执行 `execCommand`,`paste` 在 WebKit 下可能被拦截。

---

## 5. 标签管理

- 每个标签一个独立 webview(不是 iframe),label 递增 `tab-N`,能加载任意站点且互不影响。
- 切换 = `show()` 当前 / `hide()` 其余,**不销毁** → 保留每个标签的滚动位置、登录态、播放中的视频等。
- 新建(`⌘T` / 面板):建 webview → 加入状态、设为当前 → 隐藏其它。关闭已有 overlay。
- 关闭(`⌘W`):移除状态、`close()` 销毁 webview,自动切到相邻标签;关掉**最后一个标签会关掉窗口**。
- 切换标签/标题变化时把窗口标题同步为当前标签标题。
- 打开 URL:地址面板 `↵` 当前标签打开;书签/历史面板 `↵` 当前标签、`⌘↵` 新标签打开(`open_url`)。

---

## 6. 地址栏 / 搜索逻辑(`normalize`)

`⌘R` 弹出居中地址面板(`address.html`),打开时预填当前 URL 并全选。回车后按以下规则解析(`normalize(input, search, home)`):

1. 空输入 → 返回 `home`。
2. 本身是合法 URL 且 scheme 为 `http/https/tauri/about/file` → 原样用。
3. 像域名(无空白、含 `.` 或 `localhost/127.0.0.1`)→ 前面补 `https://`。
4. 都不是 → 走搜索引擎:若 `search` 含 `{query}` 则替换(URL 编码);否则当作结果页 base,追加 `?q=`。

例:`rust lang` → `https://www.bing.com/search?q=rust+lang`。

---

## 7. 标签切换器 `⌘O`

- 居中 overlay(`switcher.html`),列出全部标签(标题/URL),可输入过滤。
- 当前标签带绿点标记,打开时默认选中当前标签。
- `↑↓` 选择,`↵` 切换(`switch_to`),`esc`/点遮罩关闭,`⌘O` 再按关闭。

---

## 8. Safari 书签浏览器 `⌘B`

- 数据源:`~/Library/Safari/Bookmarks.plist`(二进制 plist),只读。
- 用 `plist` crate 解析树:文件夹(`WebBookmarkTypeList`,含 `Title`/`Children`)与书签(`WebBookmarkTypeLeaf`,含 `URLString` 和 `URIDictionary.title`),拍平成列表,每条标注所属文件夹;`com.apple.ReadingList` 显示为「Reading List」。
- 面板取数据命令 `picker_items`;交互同切换器,`↵` 当前标签打开、`⌘↵` 新标签打开(`open_url`)。
- **前提:完全磁盘访问权限(TCC)**。未授权时读文件报错,面板显示提示 + 按钮「开启完全磁盘访问设置」(Rust `open x-apple.systempreferences:com.apple.preference.security?Privacy_AllFiles`)。

---

## 9. Safari 历史记录 `⌘Y`

- 数据源:`~/Library/Safari/History.db`(SQLite),只读。
- **先拷贝**(连同 `-wal`/`-shm`)到临时目录再查询,避免与 Safari 抢 WAL 锁,也避免只读打开 WAL 失败。
- SQL:联合 `history_visits` × `history_items`,按 `visit_time` 倒序,取最近 2000 条;按 URL 去重(保留最新)。
- 时间:`visit_time` 是 Core Foundation 绝对时间(2001-01-01 起秒),`+978_307_200` 转 Unix 时间戳,由前端 `new Date(ts*1000)` 格式化。
- 交互同书签面板,同样需要等那个「完全磁盘访问」授权。

---

## 10. overlay 机制(统一)

命令面板 / 标签切换 / 地址 / 书签 / 历史共用同一个 overlay webview:

- `open_overlay(mode)`:若已开同款 → 关闭(即 toggle);已开别款 → 先关再开;未开 → 建一个**全窗口透明** webview(`transparent(true)`,label `overlay`)加载对应页面 → 记录 `overlay` 模式 → focus。
- 因为**按需创建在全部标签之后**,它永远在最上层;关闭即 `close()` 销毁。
- 面板列表数据或来自 `get_tabs` / `list_actions` / `picker_items`,取决于页面。
- 打开「地址/书签/历史/切换器」类面板时若已有 overlay 会替换;执行非 overlay 动作前先关闭 overlay。

---

## 11. 原生菜单

macOS 菜单栏(快捷键走这里,能在网页聚焦时生效):

- 应用菜单:About / Hide / Quit
- Edit 菜单:原生 Undo/Redo/Cut/Copy/Paste/Select All(文本编辑可靠来源)
- Commands 菜单:所有「进菜单」的动作,加速键由配置决定
- Window 菜单:Minimize / Maximize / Fullscreen

用户自定义的快捷键最终都会反映到菜单(启动时构建),菜单事件按动作 id 派发到 `dispatch_action`。

---

## 12. 运行与打包

```bash
cargo tauri dev           # 开发
cargo tauri build         # 打包 .app
```

- 依赖:`tauri`(带 `unstable`)、`serde`、`serde_json`、`plist`、`rusqlite`(bundled)。
- 打包后使用 Safari 书签/历史功能,需到 系统设置 → 隐私与安全性 → 完全磁盘访问权限 勾选本 app;dev 版换构建签名会变,FDA 授权可能需重新勾。

---

## 已知限制

- 标签/内容 webview 会注入 `window.__TAURI__`,外部页面理论上能调用自定义命令(原型级安全取舍;目前这些命令只影响导航等低风险操作)。
- 命令面板里的 `paste`(经 `execCommand`)在 WebKit 下可能无效;键盘复制粘贴请用原生 `⌘C/⌘V`。
- 编辑动作(undo/copy/paste…)的键位由 macOS 原生 Edit 菜单决定,不在 `settings.json` 的可自定义范围内。
- iCloud 标签(macOS 15 起不再有本地 `CloudTabs.db`)、Safari WebExtensions(`WKWebExtension` 需 fork wry 才可挂到 configuration)暂不支持。