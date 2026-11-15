# read_wechat — 微信聊天记录提取软件

把本机 **微信 4.x（Windows 版）** 里指定联系人的聊天记录提取出来，存成表格或数据文件：
支持多选联系人/群聊、群内按成员筛选、历史导出与每分钟实时追加，导出文件用 Excel 直接打开即可阅读。

> 隐私边界：所有数据只在本机流转（导出文件在 `output/`、密钥在 `cache/`，均已 gitignore）。
> 全程只读微信数据，绝不写微信数据目录；密钥绝不入日志、不入仓库。

---

## 目录

1. [功能总览](#功能总览)
2. [环境要求](#环境要求)
3. [首次使用（5 步）](#首次使用5-步)
4. [日常使用（面板操作）](#日常使用面板操作)
5. [导出文件说明](#导出文件说明)
6. [实时监听说明](#实时监听说明)
7. [换微信账号怎么办](#换微信账号怎么办)
8. [BUG 诊断手册](#bug-诊断手册)
9. [诊断命令一览](#诊断命令一览)
10. [每个文件的用途](#每个文件的用途)
11. [架构与数据流](#架构与数据流)
12. [开发与测试](#开发与测试)
13. [已知边界](#已知边界)

---

## 功能总览

| 功能 | 说明 |
|---|---|
| 会话多选 | 搜索后添加，私聊/群聊两个列表各最多 20 个，支持删除 |
| 群成员筛选 | 搜群名可展开全部成员；选「群」= 全部成员，选「群·成员」= 只导该成员 |
| 历史导出 | 全部历史 / 时间范围 / 最近 N 条，一次性任务 |
| 实时监听 | 服务端后台每分钟增量提取（关浏览器不影响），JSON 自动落 JSONL 追加 |
| 语音转文字 | 语音消息自动离线 ASR（复用 Export_from_WeChat_v1 的模型），失败回退 `[语音]` |
| 换号自愈 | 检测到换号自动重扫密钥并重试；面板另有手动「重扫密钥」按钮 |
| 导出格式 | CSV（utf-8-sig，Excel 直接开）/ JSON / JSONL（实时专用） |
| 文件组织 | 合并一个文件（默认）或每会话一个文件 |

**导出 6 列**：`会话类型(0群/1私)`、`群聊名/私聊联系人名`、`群内发送者(私聊为空)`、`时间戳(Unix秒)`、`可读时间`、`消息内容`。
名称一律用组合格式：**备注 昵称 [群内昵称] 微信号**（空组件跳过；全空用内部 id）——例如 `张三 三哥 萧萧 zs_wx`。

## 环境要求

| 项 | 要求 |
|---|---|
| 操作系统 | Windows（密钥扫描与微信 4.x 数据布局均为 Windows 口径） |
| 微信 | **4.x** 且**登录运行中**（密钥只存在于运行中的微信进程内存里；3.x 不支持） |
| Python | 3.10+，安装时勾选 "Add python.exe to PATH" |
| Rust + cargo | 仅编译导出器时需要（仓库已带编译好的 `bin/wxmsg-exporter.exe`，不重编可跳过） |
| 语音 ASR（可选） | 本机存在 Export_from_WeChat_v1 项目（默认自动探测 `../Export_from_WeChat_v1`）；没有则语音显示 `[语音]` |

## 首次使用（5 步）

1. **双击 `start_panel.bat`** —— 黑色控制台窗口出现（它就是服务本体，全程保持开着，可最小化），浏览器自动打开面板
2. **首次配置**：在页面上填微信数据根（微信「设置 → 文件管理」里显示的路径，如 `<你的微信数据根>`）→ 保存，自动生成 `.env`，之后不再询问
3. **点「开始体检」**：确认导出器就绪、账号密钥可用（首次会自动只读扫描微信进程内存取密钥，约几十秒）
4. **选择会话**：在②区搜索名字 → 结果列表 Ctrl 多选 → 确认添加（搜索词会进历史，下次输入自动联想）
5. **导出**：切到「历史导出」点开始；或切到「实时监听」开始后台提取

## 日常使用（面板操作）

- **历史导出**：选好会话 →（可选）填时间范围或最近 N 条 → 点「开始导出」→ 结果表格里直接给出每个文件的完整路径。一次性任务，导完即止。
- **实时监听**：选好会话 → 勾「同时导出历史」（可选叠加时间范围/最近N）→ 点「开始实时监听」→ 后台每分钟增量追加到同一个文件。**关浏览器页面不影响**；重开面板能看到运行状态；点「停止运行」结束。
- **查看结果**：文件都在 `output/` 目录。实时运行期间请勿用 WPS/Excel 打开正在写入的文件（会锁文件导致当轮失败；关闭后下一轮自动补写，数据不丢——要看就先复制一份）。
- **换号**：换微信账号登录后点「刷新会话数据」即可——程序自动识别当前登录号并重扫密钥。异常时会明确报错，绝不拿旧号数据冒充。

## 导出文件说明

| 模式 | 合并组织 | 分会话组织 |
|---|---|---|
| 历史导出 CSV | `output\chat_export_日期_时间.csv` | `output\私聊_张三_日期_时间.csv` / `群聊_群名_...` |
| 历史导出 JSON | `output\chat_export_...json` | `output\私聊_张三_...json` |
| 实时监听 CSV | `output\realtime_...csv` | `output\实时_私聊_张三_...csv` |
| 实时监听 JSON | `output\realtime_...jsonl`（每行一个 JSON，支持追加） | `output\实时_群聊_群名_...jsonl` |

- JSON 键名：`chat_type / session_name / sender_name / timestamp / time_readable / content`
- CSV 中私聊的发送者列为空串，JSON 中为 `null`（字段②③的规格如此）
- 语音原始文件在 `output\voices\`（转写完成后可删）

## 实时监听说明

- 轮询间隔默认 60 秒（面板可调，最小 5 秒），**由服务端后台线程驱动**——关浏览器页面照常提取
- 勾「同时导出历史」：启动的第一轮先把历史（可叠加时间范围/最近N，按原始消息计）写进同一个实时文件，之后只追加新消息
- 「最近 N 条」按**原始消息**计：勾了成员筛选后，最近 N 条原始消息里该成员没发言 → 这部分导出 0 条，属正常语义
- 游标增量设计：被文件占用等原因错过的轮次，恢复后自动补写，**不丢也不重**

## 换微信账号怎么办

1. 在微信里切换登录（保持微信运行）
2. 点面板「刷新会话数据」——程序自动识别当前登录号；若新号密钥缺失/失效会**自动重扫一次**
3. 自动重扫失败时点「重扫密钥」手动兜底；仍失败会明确报错（绝不拿旧号数据冒充当前号）

## BUG 诊断手册

先看**黑色控制台窗口**——它就是运行日志，所有诊断行都打在那里。常见报错对照：

| 报错/现象 | 原因 | 处理 |
|---|---|---|
| `WXMSG_DATA_ROOT 未配置` | 没配数据根 | 面板首次配置页填写，或手工建 `.env`（参照 config.example） |
| `WXMSG_DATA_ROOT 指向的目录不存在` | 路径填错（注意别填示例） | 微信「设置→文件管理」里核对真实路径 |
| 体检显示 `密钥失效` / `无密钥` | 微信未登录，或重登录换了钥 | 微信登录后点「重扫密钥」 |
| `检测到换号：当前登录账号 X…` | 你选的会话属于另一个微信号 | 确认想导哪个号的聊天；当前号的数据重扫后即可导 |
| `导出文件被占用（多半是 WPS/Excel…）` | 文件被表格软件独占 | 关闭打开的文件；实时模式下一轮自动补写 |
| `导出器无法启动` / 体检 `导出器缺失` | `bin/wxmsg-exporter.exe` 不存在 | 按[开发与测试](#开发与测试)编译一次 |
| `群成员索引构建失败` | contact 库打开失败 | 看黑窗口诊断行；多为微信升级导致，带信息反馈 |
| 成员列表不全 | 发言者池口径：该成员在群里从没发言 | 名册（roster）失效时的兜底来源；正常情况下名册完整 |
| 实时启动后文件里 0 条 | 最近 N 条原始消息里目标成员没发言 | 属正常语义；想导全量就把「最近 N」留空 |
| 页面点按钮提示 `无法连接面板服务` | 黑窗口被关了 | 重新双击 `start_panel.bat` |

**进一步诊断**：命令行进入项目目录，运行下表命令（输出 JSON，仅供自查，不要外发——含会话结构信息）：

```bat
bin\wxmsg-exporter.exe --data-root <你的微信数据根> --list-accounts        :: 账号+密钥状态
bin\wxmsg-exporter.exe --data-root <你的微信数据根> --list-sessions        :: 会话清单
bin\wxmsg-exporter.exe --data-root <你的微信数据根> --group-members-all    :: 全群成员索引（含来源与统计）
bin\wxmsg-exporter.exe --data-root <你的微信数据根> --dump-contact-schema  :: 微信库表结构（定位 schema 变化）
```

微信版本升级后若出现「密钥打不开/成员为空」，先跑 `--dump-contact-schema` 对比结构变化，再反馈。

## 每个文件的用途

```
read_wechat/
├── start_panel.bat            # 唯一入口：双击启动面板（UTF-8 控制台 + python -m wxmsg.cli）
├── AGENTS.md                  # 开发协作规范（每次改动一个 commit/测试先行/不懂就问/代码必注释）
├── README.md                  # 本文件
├── config.example             # 配置样例（复制为 .env 使用；.env 不入库）
├── .env                       # 本机真实配置（WXMSG_DATA_ROOT 等；已 gitignore）
├── conftest.py                # pytest 根配置（把项目根放进 sys.path）
│
├── keyscan_readonly.py        # 密钥扫描器（自 v1 移植）：只读扫微信进程内存取 SQLCipher 密钥，
│                              #   写入 cache/keystore/store.json；纯标准库，零依赖
│
├── wxmsg/                     # Python 编排包（面板与业务逻辑）
│   ├── config.py              #   配置加载：.env + 环境变量 → Config；路径解析与必填校验
│   ├── keys.py                #   密钥管理：store.json 定位、keyscan 子进程调用、日志白名单闸、
│   │                          #     C 盘旧密钥库迁移到项目 cache/
│   ├── exporter.py            #   Rust 导出器子进程封装：argv 构造、JSON 协议、失败自动重扫重试、
│   │                          #     换号防线（数据来源号≠当前登录号时拦截）
│   ├── session.py             #   会话/成员数据模型、组合名（字段②③）、名字匹配评分、消息排序
│   ├── search.py              #   群成员索引构建、私聊/群搜索、搜索历史存取（cache/search_history.json）
│   ├── render.py              #   消息内容渲染：类型→占位符（[图片]等）、引用/标题提取
│   ├── voice.py               #   语音 ASR：调 voice2text.cjs 分批转写，失败回退 [语音]
│   ├── outfile.py             #   落盘：6 字段记录 + CSV/JSON/JSONL 写出 + 实时追加 + 文件命名
│   ├── cli.py                 #   历史导出端到端编排（取数→筛选→ASR→渲染→落盘）+ 面板启动入口
│   ├── realtime.py            #   实时状态机：游标增量、去重、后台轮询语义
│   └── panel.py               #   本地 Web 面板：单页 HTML + HTTP API（127.0.0.1，Host 白名单）
│
├── native/                    # Rust 工作区（数据层，构建产物不入库）
│   ├── Cargo.toml             #   workspace 定义（依赖版本与 v1 对齐，rusqlite+SQLCipher 是关键口径）
│   ├── .cargo/config.toml     #   crates.io 镜像配置（rsproxy）
│   ├── shared/weflow-core/    #   共享核心（自 v1 移植）：SQLCipher 开库、4.x 布局发现、
│   │                          #     会话→消息表路由（md5）、每库密钥解析、时间窗流式读消息
│   ├── wcdb/                  #   业务内省库（自 v1 wcdb-native 移植，rlib 化）：联系人/群成员
│   │                          #     解析（三模型：成员表列/文本表/整数外键 JOIN）、语音定位
│   └── wxmsg-exporter/        #   本项目专用导出器：--list-accounts/--list-sessions/
│                              #     --group-members-all/--messages[--voice-out]/--dump-contact-schema
│
├── bin/                       # cargo 产物 wxmsg-exporter.exe（已 gitignore，仓库不带）
├── tests/                     # pytest 单元/集成测试（test_*.py，共 152 项）
├── output/                    # 导出结果与语音临时文件（聊天明文，已 gitignore，绝不入库）
├── cache/                     # 运行缓存：keystore/store.json（密钥）、search_history.json（已 gitignore）
└── native/target、native/.tools/  # Rust 构建缓存与便携工具链（已 gitignore）
```

## 架构与数据流

```
start_panel.bat
  └→ wxmsg/cli.py ─→ panel.py（127.0.0.1 Web 面板，后台线程驱动实时轮询）
        │
        ├─ keys.py ──→ keyscan_readonly.py 子进程（只读扫微信进程内存取密钥）
        │                └→ 写 cache/keystore/store.json
        ├─ exporter.py ─→ bin/wxmsg-exporter.exe 子进程（Rust：SQLCipher 解密+查询）
        │     └─ weflow-core（布局发现/每库密钥/消息路由/时间窗读取）
        │     └─ wcdb（联系人/群成员三模型解析/语音定位）
        ├─ render.py + voice.py（占位符渲染 + 语音离线 ASR）
        ├─ outfile.py（CSV/JSON/JSONL 落盘）
        └─ realtime.py（游标增量去重，后台线程每分钟一轮）
```

## 从零部署（新机器上从 GitHub 拉下来）

**仓库里有什么、缺什么**：源码、测试、配置样例全部齐全；Python 运行时**零第三方依赖**。
但以下三样因性质特殊**不在仓库里**，需要自行准备：

| 缺什么 | 为什么不在仓库 | 怎么补 |
|---|---|---|
| `bin/wxmsg-exporter.exe`（Rust 导出器编译产物） | 二进制不入库 | 自行编译（见下），或从可信渠道获取他人编译的 exe |
| 密钥库 `cache/keystore/store.json` | 密钥只存在于本机运行中的微信进程内存，**天然无法上传** | 微信登录后点「重扫密钥」自动生成 |
| `.env`（数据根等本机配置） | 含本机路径，不入库 | 首次启动在面板配置页填写，自动生成 |

**编译导出器（一次）**：

```bat
:: 前置：安装 Rust(rustup) 与 Strawberry Perl（编译内置 OpenSSL 必需）
cd native
cargo build --release -p wxmsg-exporter
copy target\release\wxmsg-exporter.exe ..\bin\
```

首次编译约数分钟（OpenSSL 从源码构建）；`Cargo.lock` 已锁定依赖版本，配合
`.cargo/config.toml` 的镜像配置可直接拉取。

**可选增强**：语音转文字（离线 ASR：Node + silk-wasm + sherpa-onnx SenseVoice，模型约几百 MB）。
不配置则语音显示 `[语音]` 占位符，其余功能不受影响。安装步骤：

1. 安装 [Node.js LTS](https://nodejs.org/zh-cn)（安装时一路默认即可）
2. 任意位置建一个 ASR 资源目录（下称 `<asr_root>`），按以下结构摆放：

   ```
   <asr_root>/
   ├── voice2text.cjs              ← 从 Export_from_WeChat_v1 项目根拷贝（ASR 入口脚本）
   ├── package.json                ← 在 vendor 步骤自动生成
   └── vendor/
       ├── node_modules/           ← 第 3 步 npm 安装产物
       └── models/sensevoice/
           ├── model.int8.onnx     ← 第 4 步下载（文件名必须一致）
           └── tokens.txt          ← 第 4 步下载
   ```

3. 安装依赖（版本与已验证环境对齐，sherpa-onnx-win-x64 会自动带上）：

   ```bat
   cd <asr_root>\vendor
   npm init -y
   npm install silk-wasm@3.7.1 sherpa-onnx-node@1.13.3
   ```

4. 下载 SenseVoice 中英日韩粤 int8 模型（二选一）：

   - GitHub release：<https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17.tar.bz2>
   - HuggingFace：<https://huggingface.co/csukuangfj/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17>

   解压后取 `model.int8.onnx` 与 `tokens.txt` 两个文件放进 `<asr_root>\vendor\models\sensevoice\`。

5. 在 `.env` 里配置（或首次面板配置后追加）：

   ```
   WXMSG_ASR_ROOT=<asr_root 的路径>
   WXMSG_NODE_BIN=node
   ```

6. 验证：重启面板 → 体检显示 `ASR 可用` → 导出含语音的会话，内容列出现转写文字。

模型来源：[SenseVoice 预训练模型文档](https://github.com/k2-fsa/sherpa/blob/master/docs/source/onnx/sense-voice/pretrained.rst)。

## 开发与测试

```bat
:: Python 测试（152 项）
pytest

:: Rust 测试（26 项）与重新编译导出器（改 Rust 后必须重编并拷贝 exe）
cd native
cargo test
cargo build --release -p wxmsg-exporter
copy target\release\wxmsg-exporter.exe ..\bin\
```

- 改动规则见 `AGENTS.md`：每次改动一个 commit；交付前测试全绿；功能性代码必须带注释
- Rust 依赖版本与 Export_from_WeChat_v1 对齐（rusqlite 0.32 + bundled SQLCipher），不要随手升级

## 已知边界

- 仅支持 **Windows + 微信 4.x**（实测 4.1.15.x）；3.x 的 MicroMsg.db 布局不支持
- 微信必须在本机登录运行；微信升级后 keyscan 内存布局常量或库 schema 可能失效——用诊断命令定位后反馈
- 群成员在本机微信库上以「整数外键三表关联」存储（4.1.15 实测），已适配；发言者池仅作名册失效时的兜底
- 实时模式下「最近 N 条」按原始消息计：勾了成员筛选后，最近 N 条原始消息里该成员没发言 → 该部分为 0 条
