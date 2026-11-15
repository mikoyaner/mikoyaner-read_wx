"""密钥管理：store.json 读取与 keyscan_readonly.py 子进程调用。

红线（继承 v1）：完整密钥只存在于本机 store.json；keyscan 的 stdout 可能含密钥片段
（掩码后的 key 片段/salt），因此子进程输出必须过白名单（只放行 [scan]/[stat]/[result]/[store]
前缀的诊断行）才能进日志——本模块的 filter_log_lines 就是这道闸。
"""
# 实现：模块 docstring 声明密钥管理的职责与日志红线。
# 功能：为面板体检与导出器自动重扫提供唯一的密钥入口。

from __future__ import annotations

import os
import shutil
import subprocess
import sys
from pathlib import Path

from .config import project_root

# 实现：import os/shutil/subprocess/sys/Path + 项目根定位。
# 功能：本模块全部依赖。

_KEYSCAN_DEFAULT = Path(__file__).resolve().parent.parent / "keyscan_readonly.py"
# 实现：keyscan 脚本默认路径 = 项目根/keyscan_readonly.py（本文件在 wxmsg/ 下，上一级即项目根）。
# 解析：与 keyscan_readonly.py 原样移植的文件配套；测试可注入其它脚本路径。
# 功能：ensure_keys 的默认扫描器来源。

_WHITELIST_PREFIXES = ("[scan]", "[stat]", "[result]", "[store]")
# 实现：允许进日志的行前缀白名单（v1 pipeline.ensure_keys 同款口径）。
# 解析：keyscan 控制台输出里只有这四类前缀的行是纯诊断；其余行（若有）可能含候选密钥片段，
#       一律丢弃——宁可少日志也绝不把密钥材料写进面板/日志文件。
# 功能：子进程输出进日志前的唯一过滤闸。


def store_path() -> Path:
    """store.json 的位置（与 Rust keystore::store_path 同口径的键，不同默认值）：
    WEFLOW_KEY_STORE 环境变量 > <项目根>/cache/keystore/store.json。

    用户拍板：运行缓存一律落在项目文件夹下，不写 C 盘 %APPDATA%——所以默认值
    从 v1 的 APPDATA 改为项目 cache/（Rust 端通过 WEFLOW_KEY_STORE 环境变量对齐）。"""
    env = os.environ.get("WEFLOW_KEY_STORE", "").strip()
    if env:
        return Path(env)
    return project_root() / "cache" / "keystore" / "store.json"
    # 实现：优先环境变量（Python/Rust/子进程三方对齐的唯一开关），缺省回落项目 cache/。
    # 解析：cache/ 已在 .gitignore（密钥绝不入库）；与 v1 的 %APPDATA% 共存互不影响
    #       （v1 仍读它自己的 APPDATA store）。
    # 功能：诊断/面板展示密钥状态的定位依据。


def legacy_appdata_store() -> Path:
    """v1 口径的旧密钥库位置（%APPDATA%\\weflow-key-provider\\store.json），仅迁移用。"""
    base = os.environ.get("APPDATA") or str(Path.home())
    return Path(base) / "weflow-key-provider" / "store.json"
    # 实现：与 v1 keyscan/keystore 的默认路径一致。
    # 功能：migrate_legacy_store 的来源定位。


def migrate_legacy_store() -> str:
    """把 v1 在 C 盘 %APPDATA% 的旧密钥库迁移到项目 cache/（用户拍板：缓存不落 C 盘）。

    迁移 = 原样复制（原文件保留，v1 不受影响）；项目里已有 store 时不动作；两边都没有也不动作。
    返回给控制台的说明文案（空串=无需动作）。"""
    legacy = legacy_appdata_store()
    target = store_path()
    if target.exists() or not legacy.exists():
        return ""
    try:
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(legacy, target)
    except OSError:
        return ""  # 迁移失败不打断启动：项目里没钥时会走 keyscan 重扫，只是多一次扫描
    return (f"已把旧密钥库从 C 盘 %APPDATA% 迁移到 {target.parent} "
            f"（原文件保留，Export_from_WeChat_v1 不受影响）")
    # 实现：复制而非移动——v1 那份密钥仍有效，两边可并存。
    # 解析：失败静默降级（返回空串）而非报错——密钥缺失的后果只是"重扫一次"，不该挡住面板启动。
    # 功能：首次启动的零成本迁移。


def filter_log_lines(lines: list[str]) -> list[str]:
    """keyscan 输出的日志闸：只放行白名单前缀的诊断行，其余（可能含密钥材料）丢弃。"""
    return [ln for ln in lines if ln.strip().startswith(_WHITELIST_PREFIXES)]
    # 实现：逐行 strip 后按 _WHITELIST_PREFIXES 元组 startswith 过滤。
    # 解析：startswith 接受元组是 Python 惯用法，一行实现且语义精确；
    #       不做正则/模糊匹配——白名单必须窄，任何"看起来无害"的放宽都是密钥泄漏面。
    # 功能：ensure_keys 返回的诊断行全部经过本闸，调用方拿到的行可安全进日志。


def ensure_keys(cfg, keyscan_py: Path | None = None,
                log=None, timeout: int | None = None) -> tuple[bool, list[str]]:
    """运行 keyscan 只读扫微信进程内存，把密钥写进 store.json。

    返回 (是否成功, 可进日志的诊断行)。keyscan 退出码口径（移植自 v1）：
    0=成功；3=没扫到密钥；4=store 落盘失败；其余=运行错误。
    微信未在本机运行时必然失败（密钥只在运行中的进程内存里）——失败原因在诊断行里。
    """
    script = Path(keyscan_py) if keyscan_py else _KEYSCAN_DEFAULT
    # argv[2] = 输出 store 路径：keyscan main 支持（缺省才是它的 APPDATA 硬编码）——
    # 显式传入保证密钥落项目 cache/（用户拍板：缓存不写 C 盘）。
    cmd = [sys.executable, str(script), str(cfg.data_root), str(store_path())]
    # 实现：用当前解释器起 keyscan 子进程，argv[1]=微信数据根（keyscan main 的入口参数）。
    # 解析：list argv 直传不走 shell（防注入）；PYTHONIOENCODING=utf-8 保证中文诊断行不乱码
    # （v1 同款做法；keyscan 是我们自己的可信脚本，不用 -I 以免丢掉该编码设置）。
    # 功能：构造密钥扫描命令。
    env = dict(os.environ)
    env["PYTHONIOENCODING"] = "utf-8"
    timeout_s = timeout if timeout is not None else cfg.keyscan_timeout_s
    try:
        proc = subprocess.run(
            cmd, capture_output=True, text=True, encoding="utf-8",
            errors="replace", timeout=timeout_s, env=env,
        )
    except subprocess.TimeoutExpired:
        return False, [f"[result] 密钥扫描超时（{timeout_s}s）：微信可能刚启动还没加载配置，稍后再试"]
    except OSError as e:
        return False, [f"[result] 密钥扫描进程启动失败：{type(e).__name__}"]
    # 实现：capture_output 双通道捕获；超时/启动失败都折成 (False, 一行可读诊断)，不抛异常。
    # 解析：超时文案给操作指引（微信刚启动时内存里可能还没有 Cipher 配置）；
    #       OSError 只带异常类型名，不带路径（路径可能含用户名）。
    # 功能：把子进程层的意外全部收敛成可展示的失败。
    merged = proc.stdout.splitlines() + proc.stderr.splitlines()
    safe_lines = filter_log_lines(merged)
    if log:
        for line in safe_lines:
            log(line)
    ok = proc.returncode == 0
    if not ok and not safe_lines:
        # 失败却没有一条白名单诊断：给一行兜底说明，避免调用方面对空原因。
        safe_lines.append(f"[result] 密钥扫描失败（退出码 {proc.returncode}），且无可展示诊断")
    return ok, safe_lines
    # 实现：stdout+stderr 合并过白名单闸，逐行回调 log；成功判据 = 退出码 0。
    # 解析：stderr 也要过闸——traceback 行可能带本机路径，白名单恰好把 traceback 全滤掉，
    #       只留 keyscan 自己打的 [scan]/[result] 行（keyscan 的错误路径已按"不回显路径"口径构造）。
    # 功能：面板「重扫密钥」按钮与导出器自动重扫的统一入口。
