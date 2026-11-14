"""wxmsg —— 微信聊天记录提取软件的 Python 编排包。

模块分工：
- config.py   配置加载（.env + 环境变量 → Config）
- keys.py     密钥：读/写 store.json，必要时子进程调 keyscan_readonly.py
- exporter.py Rust 导出器子进程调用（stdout=JSON 数据，stderr=诊断）
- session.py  会话/成员数据模型 + 名称回退（备注>昵称>微信号>内部id）
- render.py   消息类型 → 占位符/标题/引用提取
- voice.py    语音 ASR（调 v1 vendor 的 voice2text.cjs），失败回退 [语音]
- outfile.py  CSV/JSON/JSONL 写出（合并/分文件、实时追加）
- realtime.py 实时轮询（游标增量、去重、密钥失效重扫）
- panel.py    本地 Web 面板（127.0.0.1）
- cli.py      命令行入口（启动面板等）

依赖策略：除 pytest 外全用标准库——密钥扫描、CSV/JSON、HTTP 面板都不引第三方。
"""
# 实现：包级 docstring 声明模块分工与依赖策略。
# 解析：包被 python -m / import wxmsg.xxx 两种方式使用；不在此做任何初始化副作用。
# 功能：作为编排包的入口说明与模块索引。
