"""配置加载：.env + 环境变量 → Config 数据类。

口径与 v1 对齐：
- .env 用 utf-8-sig 读（吃掉记事本写文件时加的 BOM，否则首个 key 会变成 \\ufeffWXMSG_...）；
- 真实环境变量优先于 .env 文件（显式 export 可临时覆盖文件值）；
- 路径统一解析：绝对路径原样；%VAR%/~ 展开；相对路径按项目根解析（不按当前工作目录，
  这样双击 start_panel.bat 时无论资源管理器当前目录在哪行为都一致）；
- WXMSG_DATA_ROOT 缺失/不存在直接抛 ConfigError——绝不去猜微信数据在哪（v1 同款红线）。
"""
# 实现：模块 docstring 声明配置加载的四条口径。
# 解析：本项目所有模块取配置只经本文件，保证口径唯一；ConfigError 的 message 面向面板直接展示。
# 功能：提供 load_config 唯一入口与路径/整数的解析工具。

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

# 实现：import os/dataclass/Path 提供环境读取、数据类与路径处理。
# 解析：__future__ annotations 让 asr_root: Path | None 这类注解在旧版 Python 也可用。
# 功能：本模块的全部依赖（零第三方包）。

DEFAULT_OUTPUT_DIR = "output"
DEFAULT_EXPORTER_BIN = "bin/wxmsg-exporter.exe"
DEFAULT_POLL_INTERVAL_S = 60
DEFAULT_KEYSCAN_TIMEOUT_S = 900
DEFAULT_EXPORTER_TIMEOUT_S = 180
DEFAULT_NODE_BIN = "node"
POLL_INTERVAL_MIN_S = 5
# 实现：各配置键的默认值常量 + 轮询间隔下限。
# 解析：默认值必须与 config.example 一一对应，改任何一处要同步另一处；
#       POLL_INTERVAL_MIN_S=5 防止把间隔配成 0/负数/亚秒级把数据库读爆（配置错误时报错而不是静默夹紧）。
# 功能：集中管理默认值，避免魔法数字散落各模块。

KEY_DATA_ROOT = "WXMSG_DATA_ROOT"
KEY_OUTPUT_DIR = "WXMSG_OUTPUT_DIR"
KEY_EXPORTER_BIN = "WXMSG_EXPORTER_BIN"
KEY_POLL_INTERVAL_S = "WXMSG_POLL_INTERVAL_S"
KEY_KEYSCAN_TIMEOUT_S = "WXMSG_KEYSCAN_TIMEOUT_S"
KEY_EXPORTER_TIMEOUT_S = "WXMSG_EXPORTER_TIMEOUT_S"
KEY_ASR_ROOT = "WXMSG_ASR_ROOT"
KEY_NODE_BIN = "WXMSG_NODE_BIN"
# 实现：配置键名常量。
# 解析：键名集中在这一处，config.example/测试/面板体检都引用同名常量，防拼写漂移。
# 功能：统一配置键命名。

ASR_ROOT_FALLBACK = "../Export_from_WeChat_v1"
# 实现：ASR 资源默认回退路径（v1 项目根，其下有 voice2text.cjs 与 vendor/）。
# 解析：语音 ASR 不复制 250MB vendor，直接指向本机已有的 v1 项目根；相对项目根解析。
#       v1 项目不在或没装 ASR 时 asr_root 为 None，语音消息回退 [语音] 占位符，不报错不中断。
# 功能：定义语音转写资源的查找回退顺序。

ENV_FILENAME = ".env"


class ConfigError(Exception):
    """配置缺失/非法。message 面向面板直接展示，不得包含密钥等敏感内容。"""
    # 实现：继承 Exception 的空类，仅靠 docstring 说明用途与 message 纪律。
    # 解析：单独定义类型让调用方（面板/CLI）可以只接住配置错误给友好提示，而不吞真 bug。
    # 功能：作为配置层对外的唯一异常类型。


def project_root() -> Path:
    """项目根 = wxmsg 包所在目录的上一级（read_wechat/）。"""
    return Path(__file__).resolve().parent.parent
    # 实现：以本文件位置为锚取父父目录，resolve() 消解符号链接/相对路径。
    # 解析：不依赖当前工作目录，双击 bat / 命令行任意目录启动都得到同一项目根。
    # 功能：相对路径（output、bin、ASR 回退）的统一解析基准。


def set_env_key(key: str, value: str, root: Path | None = None) -> Path:
    """把 KEY=VALUE 写进项目根 .env：已存在的键替换值，其它行原样保留；文件不存在则新建。

    返回写入的 .env 路径。面板「首次配置」用它落盘数据根。"""
    env_path = (root or project_root()) / ENV_FILENAME
    lines: list[str] = []
    if env_path.exists():
        text = env_path.read_text(encoding="utf-8-sig")
        lines = text.splitlines()
    # 实现：utf-8-sig 读（兼容记事本 BOM），逐行保留。
    # 功能：旧 .env 的其它配置项（如 ASR 根）不被覆盖丢失。
    replaced = False
    out: list[str] = []
    for line in lines:
        stripped = line.strip()
        if stripped and not stripped.startswith("#") and "=" in stripped and \
                stripped.split("=", 1)[0].strip() == key:
            out.append(f"{key}={value}")
            replaced = True
        else:
            out.append(line)
    if not replaced:
        if out and out[-1].strip():
            out.append("")
        out.append(f"{key}={value}")
    # 实现：逐行匹配 key（忽略行内空白），命中则替换；没命中追加到文件尾（前面留一空行隔开注释块）。
    # 解析：替换而非追加——同一键重复保存不会在 .env 里堆出多行（后行会被 load_env_file 的
    #       dict 覆盖语义掩盖，但文件本身会越来越乱）。
    # 功能：配置键的幂等写入。
    env_path.write_text("\n".join(out) + "\n", encoding="utf-8", newline="\n")
    return env_path
    # 实现：LF 行尾写回（与 git/加载器口径一致，Windows 记事本也能正常打开）。
    # 功能：面板 setup 与将来其它"程序写配置"场景的唯一入口。


def load_env_file(path: Path) -> dict[str, str]:
    """读 .env 为 {KEY: VALUE}；utf-8-sig 编码；# 注释与空行跳过；无 '=' 的行忽略。"""
    env: dict[str, str] = {}
    try:
        text = path.read_text(encoding="utf-8-sig")
    except FileNotFoundError:
        return env
    # 实现：utf-8-sig 读整个文件（缺失返回空 dict），逐行解析进 env。
    # 解析：utf-8-sig 兼容记事本产生的 BOM；FileNotFoundError 是正常情况（首次使用未建 .env）不算错；
    #       其它 OSError（权限/占用）不吞——直接抛给上层，与"读坏了宁可不跑"的 v1 口径一致。
    # 功能：把本机 .env 解析成键值表。
    for lineno, line in enumerate(text.splitlines(), start=1):
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        key, _, value = line.partition("=")
        key = key.strip()
        if not key:
            continue
        env[key] = value.strip()
        # 实现：逐行 strip 后跳过空行/# 注释/无 '=' 行；按第一个 '=' 切 key/value 并各自 strip。
        # 解析：partition 以第一个 '=' 切分，VALUE 里含 '='（如 URL 参数）不受影响；
        #       行号 lineno 仅保留给将来报错用，当前版本宽松跳过坏行（配置文件是用户手写的本机文件）。
        # 功能：产出干净的键值对，供 load_config 与环境变量合并。
    return env


def resolve_path(raw: str, root: Path) -> Path:
    """路径解析：%VAR%/~ 展开；相对路径按 root 解析；返回归一化的绝对路径。"""
    expanded = os.path.expandvars(os.path.expanduser(raw))
    p = Path(expanded)
    if not p.is_absolute():
        p = root / p
    return p.resolve()
    # 实现：先 expandvars（%USERPROFILE% 等 Windows 环境变量引用）再 expanduser（~），
    #       相对路径拼 root 后 resolve() 归一。
    # 解析：resolve() 会消解 .. 与符号链接，输出稳定可比较的绝对路径；
    #       空字符串调用方自行处理（空=未配置），本函数不做空判断。
    # 功能：所有配置路径的统一解析出口（数据根/输出目录/exe/ASR 根）。


def _parse_int(raw: str | None, key: str, default: int) -> int:
    """空/未配置 → default；非整数或 <1 → ConfigError。"""
    if raw is None or raw == "":
        return default
    try:
        value = int(raw)
    except ValueError:
        raise ConfigError(f"{key} 必须是整数，当前值不合法") from None
    if value < 1:
        raise ConfigError(f"{key} 必须 >= 1，当前值 {value}")
    return value
    # 实现：空值回落默认；int() 失败或 <1 抛 ConfigError（from None 掐掉无意义的 traceback）。
    # 解析：错误 message 只带键名与值，不带任何路径/密钥；调用方对轮询间隔另有 >=5 的下限校验。
    # 功能：整数型配置（超时/间隔）的统一解析与校验。


@dataclass
class Config:
    """解析后的运行配置。字段均为已解析的绝对路径/合法整数。"""

    data_root: Path                 # 微信数据根（必填，如 D:/xwechat_files）
    output_dir: Path                # 导出结果目录（默认 项目根/output）
    exporter_bin: Path              # Rust 导出器路径（默认 项目根/bin/wxmsg-exporter.exe）
    poll_interval_s: int            # 实时轮询间隔秒（默认 60，下限 5）
    keyscan_timeout_s: int          # 密钥扫描超时秒（默认 900）
    exporter_timeout_s: int         # 导出器子进程超时秒（默认 180）
    asr_root: Path | None           # v1 项目根（含 voice2text.cjs 与 vendor/）；None=语音回退占位符
    node_bin: str                   # Node 可执行文件名/路径（默认 node）

    @property
    def voice2text_js(self) -> Path | None:
        """voice2text.cjs 的完整路径；未配置 ASR 根时为 None。"""
        if self.asr_root is None:
            return None
        return self.asr_root / "voice2text.cjs"
        # 实现：asr_root 拼上固定文件名 voice2text.cjs。
        # 解析：v1 的 ASR 入口脚本固定在项目根（vendor/ 只放 node_modules 与模型），
        #       所以这里不做目录扫描，拼死文件名即可；None 透传给 voice.py 走占位符分支。
        # 功能：语音模块定位 ASR 入口脚本的唯一途径。
    # 实现：dataclass 字段逐一标注含义；voice2text_js 是派生属性。
    # 解析：所有路径字段在 load_config 里就完成解析与存在性校验，使用方拿到的都是"可直接用"的值。
    # 功能：承载全项目运行配置的单一数据结构。


def load_config(environ: dict[str, str] | None = None,
                dotenv: dict[str, str] | None = None,
                root: Path | None = None) -> Config:
    """组装 Config。

    environ/dotenv/root 三个参数供测试注入；生产调用 load_config() 不传参：
    - environ 缺省取 os.environ（真实环境变量）
    - dotenv 缺省读项目根 .env（不存在则为空表）
    - 优先级：真实环境变量 > .env > 默认值
    """
    root = root or project_root()
    if dotenv is None:
        dotenv = load_env_file(root / ENV_FILENAME)
    if environ is None:
        environ = dict(os.environ)

    def lookup(key: str) -> str | None:
        # 实现：闭包按"环境变量 > .env"顺序取值，都没有返回 None。
        # 解析：环境变量里的空字符串也算"配置了但为空"，与 .env 空值同样走默认值分支。
        # 功能：统一两处来源的取值顺序。
        value = environ.get(key)
        if value is None or value == "":
            value = dotenv.get(key)
        return value or None

    data_raw = lookup(KEY_DATA_ROOT)
    if not data_raw:
        raise ConfigError(
            "WXMSG_DATA_ROOT 未配置：请在项目根建 .env（参照 config.example），"
            "填微信数据根，如 D:/xwechat_files。程序绝不猜测微信数据位置。")
    data_root = resolve_path(data_raw, root)
    if not data_root.is_dir():
        raise ConfigError(f"WXMSG_DATA_ROOT 指向的目录不存在：请核对微信 设置→文件管理 里的路径")
    # 实现：数据根必填校验（缺失给操作指引，存在性校验失败给核对指引），随后 resolve_path 归一。
    # 解析：这条是硬闸——数据根错了后面每一步都是空跑，必须在入口拦住；
    #       错误文案不含本机用户名等敏感信息（示例路径用占位盘符）。
    # 功能：保证数据根有效，这是整个链路的第一道体检。

    output_dir = resolve_path(lookup(KEY_OUTPUT_DIR) or DEFAULT_OUTPUT_DIR, root)
    exporter_bin = resolve_path(lookup(KEY_EXPORTER_BIN) or DEFAULT_EXPORTER_BIN, root)
    poll_interval_s = _parse_int(lookup(KEY_POLL_INTERVAL_S), KEY_POLL_INTERVAL_S,
                                 DEFAULT_POLL_INTERVAL_S)
    if poll_interval_s < POLL_INTERVAL_MIN_S:
        raise ConfigError(
            f"{KEY_POLL_INTERVAL_S} 最小 {POLL_INTERVAL_MIN_S} 秒（当前 {poll_interval_s}），"
            "过密会高频重复读数据库")
    keyscan_timeout_s = _parse_int(lookup(KEY_KEYSCAN_TIMEOUT_S), KEY_KEYSCAN_TIMEOUT_S,
                                   DEFAULT_KEYSCAN_TIMEOUT_S)
    exporter_timeout_s = _parse_int(lookup(KEY_EXPORTER_TIMEOUT_S), KEY_EXPORTER_TIMEOUT_S,
                                    DEFAULT_EXPORTER_TIMEOUT_S)
    # 实现：可选项依次解析（输出目录/导出器/轮询间隔/两个超时），轮询间隔加 >=5 下限。
    # 解析：exe 路径不在这里做存在性检查——Rust 步骤 3 才会产出 exe，启动体检（面板）会查；
    #       轮询下限是配置期报错而非静默夹紧，避免"配了 1 秒却跑成 5 秒"的困惑。
    # 功能：产出全部可选配置的最终值。
    asr_raw = lookup(KEY_ASR_ROOT)
    asr_root: Path | None = None
    if asr_raw:
        asr_root = resolve_path(asr_raw, root)
        if not asr_root.is_dir():
            asr_root = None  # 显式配置了但不存在：按未配置处理，语音回退占位符
    else:
        fallback = resolve_path(ASR_ROOT_FALLBACK, root)
        if fallback.is_dir() and (fallback / "voice2text.cjs").is_file():
            asr_root = fallback
    # 实现：ASR 根两级策略——显式配置则解析（不存在降级 None）；未配置则尝试 v1 项目根回退，
    #       且要求 voice2text.cjs 真实存在才算命中。
    # 解析：降级而不是报错，因为语音 ASR 是增值功能（失败回退 [语音] 占位符），不应卡死主流程；
    #       回退条件带 voice2text.cjs 存在性检查，防"目录在但 ASR 没装"的半可用状态。
    # 功能：确定语音转写资源是否可用，供 voice.py 判断走 ASR 还是占位符。
    node_bin = lookup(KEY_NODE_BIN) or DEFAULT_NODE_BIN
    return Config(
        data_root=data_root,
        output_dir=output_dir,
        exporter_bin=exporter_bin,
        poll_interval_s=poll_interval_s,
        keyscan_timeout_s=keyscan_timeout_s,
        exporter_timeout_s=exporter_timeout_s,
        asr_root=asr_root,
        node_bin=node_bin,
    )
    # 实现：全部字段解析完毕后一次性组装 Config 返回。
    # 解析：load_config 是纯函数（三个注入点全部可控），面板每次请求重新调用即可拿到最新配置，
    #       不需要全局单例；测试不依赖真实 .env / 真实 os.environ。
    # 功能：配置层的唯一组装出口。
