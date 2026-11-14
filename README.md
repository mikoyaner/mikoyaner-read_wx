# read_wechat — 微信聊天记录提取软件

参照 `Export_from_WeChat_v1` 的数据链路，从本机微信 4.x（Windows）提取指定联系人私聊 /
群聊的消息与发送时间，导出为 CSV 或 JSON。

## 功能（规划）

- 会话多选：多个私聊联系人 / 多个群聊
- 群内筛选：勾选群成员，只导被选成员的消息
- 历史提取：全部 / 时间范围 / 最近 N 条
- 实时提取：每分钟增量抓取新消息并追加写入（JSON 走 JSONL 行格式）
- 字段（6 列）：会话类型(0群聊/1私聊) / 群名或私聊联系人名 / 群内发送者(私聊为空) /
  Unix 秒时间戳 / 可读时间 / 消息内容
- 名称回退（字段②③）：备注 > 昵称 > 微信号 > 内部 id
- 非文本消息：语音走 ASR 转文字（复用 v1 vendor 的离线模型），其余用占位符
  （[图片] / [视频] / [动画表情] / [系统消息]，引用/文件/链接消息提取标题）

## 使用

1. 复制 `config.example` 为 `.env`，填写 `WXMSG_DATA_ROOT`（微信数据根，如 `D:/xwechat_files`）
2. 微信在本机登录运行（密钥在微信进程内存里，只读扫描获取）
3. 双击 `start_panel.bat`，浏览器会自动打开本地面板（仅监听 127.0.0.1，不对外网开放）

## 架构

```
keyscan_readonly.py 只读扫微信进程内存取 SQLCipher 密钥（纯标准库 ctypes，不注入）
        ↓ 写 %APPDATA%\weflow-key-provider\store.json
Rust 导出器 bin/wxmsg-exporter.exe（rusqlite + bundled SQLCipher，解密+查询，stdout=JSON）
        ↓ 子进程调用（list argv 不走 shell）
Python 编排 wxmsg/（会话/成员选择、消息渲染、语音 ASR、CSV/JSON 写出、实时轮询）
        ↓
本地 Web 面板 panel.py（127.0.0.1，浏览器操作）
```

数据层与 `Export_from_WeChat_v1` 同源（`native/shared/weflow-core`），支持微信 4.x 的
`db_storage` 布局：消息按会话分表 `Msg_<md5(会话id)>`，群发送者经 Name2ID 反查。

## 隐私红线

- 导出文件含聊天明文，只落 `output/`（已 gitignore），绝不入库
- 密钥只写本机 `%APPDATA%\weflow-key-provider\store.json`，绝不入日志、不入仓库
- 全程只读微信数据；绝不写微信数据目录

## 开发

- 仅支持 Windows + 微信 4.x（v1 实测 4.1.15.13 口径；3.x 的 MicroMsg.db 布局不支持）
- 测试：`pytest`（Python）、`cargo test`（Rust，native/ 下）
- Rust 改动后需重新编译并拷贝 exe：`cargo build --release -p wxmsg-exporter`
  然后 `cp target/release/wxmsg-exporter.exe ../bin/`（cargo test 不刷新 bin 产物）
- 每次改动一个 commit；功能性代码必须带注释（见 AGENTS.md）
