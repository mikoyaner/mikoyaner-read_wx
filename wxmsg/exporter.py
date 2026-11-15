"""Rust 导出器子进程调用：stdout=JSON 数据 / stderr=诊断 的协议封装。

安全口径（继承 v1）：list argv 直传不走 shell（杜绝参数注入）；密钥绝不进 argv 进程表之外的
任何输出；失败时自动重扫一次密钥再重试一次（重登录换钥是常态，v1 pipeline.ensure_keys 同款）。

测试注入口：argv_prefix（把 exe 换成 fixture 解释器+脚本）与 rescan（替换重扫动作），
生产代码两个都不传。
"""
# 实现：模块 docstring 声明协议、安全口径与测试注入口。
# 功能：wxmsg 其余模块（cli/realtime/panel）取数的唯一出口。

from __future__ import annotations

import json
import os
import subprocess

from . import keys
from .config import Config

# 实现：import json/subprocess 与同包 keys/config。
# 功能：子进程协议封装所需依赖。

_STDERR_TAIL_LINES = 6
# 实现：错误文案里附带的 stderr 尾部行数。
# 解析：Rust 的错误链在 stderr 尾部（[err] 开头），取尾部几行足够定位；全量塞进错误对象
#       会把巨长的诊断刷进面板。功能：控制错误文案长度。

_MODE_FLAG = {
    "list_accounts": "--list-accounts",
    "list_sessions": "--list-sessions",
    "group_members": "--group-members",
    "messages": "--messages",
}
# 实现：模式名 → 旗标映射。
# 解析：与 wxmsg-exporter main.rs run() 的模式一一对应，改 Rust 端模式名必须同步这里。
# 功能：mode 函数复用同一构造逻辑。


class ExporterError(RuntimeError):
    """导出器执行失败。message 面向面板展示（含 stderr 尾部诊断，不含密钥）。"""
    # 实现：继承 RuntimeError 的薄封装。
    # 解析：单独类型让调用方只接住导出失败给友好提示，不吞真 bug。
    # 功能：本模块对外的唯一异常类型。


def run_exporter(cfg: Config, mode: str, extra_args: list[str], *,
                 argv_prefix: list[str] | None = None,
                 timeout: int | None = None) -> dict:
    """跑一次导出器并解析 stdout JSON。

    argv_prefix：测试注入（如 [sys.executable, fixture.py]），生产传 None 用 cfg.exporter_bin。
    返回解析后的 JSON dict；非零退出/解析失败/超时统一抛 ExporterError（message 带 stderr 尾部）。
    """
    argv = list(argv_prefix) if argv_prefix else [str(cfg.exporter_bin)]
    argv += ["--data-root", str(cfg.data_root), _MODE_FLAG[mode]] + [str(a) for a in extra_args]
    # 实现：argv = [可执行体, --data-root, 模式旗标, ...附加参数]，list 直传子进程。
    # 解析：data_root 与附加参数全部 str() 化（Path/数字都能安全拼）；顺序固定便于 fixture 断言。
    # 功能：统一的子进程命令构造。
    timeout_s = timeout if timeout is not None else cfg.exporter_timeout_s
    # WEFLOW_KEY_STORE 指向项目 cache/ 的密钥库（用户拍板：缓存不写 C 盘）——
    # Rust keystore::store_path() 的最高优先级就是该环境变量，子进程环境对齐后
    # 「keyscan 写的 = 导出器读的」始终是项目内同一份文件。
    env = dict(os.environ)
    env["WEFLOW_KEY_STORE"] = str(keys.store_path())
    try:
        proc = subprocess.run(
            argv, capture_output=True, text=True, encoding="utf-8",
            errors="replace", timeout=timeout_s, env=env,
        )
    except subprocess.TimeoutExpired:
        raise ExporterError(f"导出器超时（{timeout_s}s）：会话消息量可能过大，请缩小时间范围后重试") from None
    except OSError as e:
        raise ExporterError(f"导出器无法启动（{type(e).__name__}）：请先按 README 编译 Rust 导出器") from None
    # 实现：超时/启动失败折成带操作指引的 ExporterError，不抛裸异常。
    # 解析：超时文案指向缩小范围（全量导出大号可能超 180s 默认值）；
    #       OSError 文案指向编译步骤（bin/wxmsg-exporter.exe 不存在是最常见的新机问题）。
    # 功能：把子进程层意外收敛成可展示失败。
    if proc.returncode != 0:
        tail = [ln for ln in proc.stderr.splitlines() if ln.strip()][-_STDERR_TAIL_LINES:]
        raise ExporterError("导出器执行失败（退出码 {code}）{tail}".format(
            code=proc.returncode,
            tail=("：" + " | ".join(tail)) if tail else "",
        ))
    # 实现：非零退出取 stderr 尾部非空行拼进错误文案。
    # 解析：Rust 的 anyhow 错误链在 stderr；tail 摘要 + 退出码足够面板定位；
    #       错误串不含密钥（weflow-core 的错误文案已按"不回显密钥"口径构造）。
    # 功能：把 Rust 端失败如实上报。
    try:
        return json.loads(proc.stdout)
    except ValueError:
        raise ExporterError("导出器输出不是合法 JSON（程序 bug 或版本不匹配，请更新后重试）") from None
    # 实现：stdout 整体 json.loads。
    # 解析：Rust 端 emit 只打一行 JSON 到 stdout，stderr 才是诊断——两者分离让这里可以整段解析；
    #       解析失败 = 协议被破坏，按 bug 处理。
    # 功能：交付结构化数据。


def _run_with_rescan(cfg: Config, mode: str, extra_args: list[str], *,
                     argv_prefix: list[str] | None = None,
                     rescan=None, log=None) -> dict:
    """执行模式调用，失败时自动重扫一次密钥再重试一次（v1 ensure_keys 的失败路径同款）。

    rescan：测试注入点；默认 keys.ensure_keys(cfg)（重扫密钥）。
    """
    do_rescan = rescan if rescan is not None else (
        lambda: keys.ensure_keys(cfg, log=log)[0]
    )
    try:
        return run_exporter(cfg, mode, extra_args, argv_prefix=argv_prefix)
    except ExporterError:
        # 第一次失败：多半是重登录换钥（旧钥打不开库）或还没扫过钥——重扫后再试一次。
        if log:
            log("导出失败，自动重扫密钥后重试一次…")
        if not do_rescan():
            raise ExporterError("重扫密钥后仍失败：微信可能未在本机登录，或微信版本超出适配范围") from None
        return run_exporter(cfg, mode, extra_args, argv_prefix=argv_prefix)
    # 实现：失败 → 重扫（rescan 注入）→ 重试一次；重扫失败或重试仍失败都抛 ExporterError。
    # 解析：只重试一次——无限重试会把"结构性失败"（版本不匹配/数据根错）变成卡死循环；
    #       「重扫后仍失败」的文案给出两个最常见根因。
    # 功能：把"换钥导致的一次性失败"对用户透明化。


def list_accounts(cfg: Config, **kw) -> dict:
    """--list-accounts：数据根下各账号目录 + 密钥状态（体检页数据源）。"""
    return _run_with_rescan(cfg, "list_accounts", [], **kw)
    # 功能：透传给面板体检。


def list_sessions(cfg: Config, account: str | None = None, **kw) -> dict:
    """--list-sessions：当前账号的全部会话（含四个名称字段与活跃时间）。"""
    extra = []
    if account:
        extra += ["--account", account]
    return _run_with_rescan(cfg, "list_sessions", extra, **kw)
    # 实现：--account 可选（显式指定账号目录名/wxid）。
    # 功能：面板会话列表数据源。


def group_members(cfg: Config, sid: str, account: str | None = None, **kw) -> dict:
    """--group-members：某群完整成员名单。"""
    extra = ["--sid", sid]
    if account:
        extra += ["--account", account]
    return _run_with_rescan(cfg, "group_members", extra, **kw)
    # 功能：面板群成员勾选列表数据源。


def messages(cfg: Config, sid: str, *, begin: int = 0, end: int = 0,
             recent: int | None = None, voice_out=None,
             account: str | None = None, **kw) -> dict:
    """--messages：某会话消息。begin/end=秒级时间窗（0=不限）；recent=最近 N 条（互斥优先）。

    voice_out：语音 silk 落盘目录；给出时结果集内的语音消息会被提取，行内带 voice 元数据。
    """
    extra = ["--sid", sid, "--begin", str(begin), "--end", str(end)]
    if recent is not None:
        extra += ["--recent", str(recent)]
    if voice_out:
        extra += ["--voice-out", str(voice_out)]
    if account:
        extra += ["--account", account]
    return _run_with_rescan(cfg, "messages", extra, **kw)
    # 实现：--sid 必带；begin/end 恒传（0=不限，Rust 端闭区间过滤）；recent/voice-out/account 可选。
    # 解析：recent 传 0 时按"取 0 条"处理（Rust 端 n=0 短路返回空）——调用方不要传 0，
    #       面板层会用 None 表示"不按条数取"。
    # 功能：历史导出与实时增量的统一取数入口。
