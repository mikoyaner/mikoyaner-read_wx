"""导出落盘：CSV / JSON / JSONL 三种格式 + 合并/分文件两种组织 + 实时追加。

字段规格（用户拍板，6 列）：
  ①会话类型(0群/1私) ②群聊名/私聊联系人名 ③群内发送者(私聊为空) ④Unix秒时间戳
  ⑤可读时间 ⑥消息内容
- CSV 用 utf-8-sig（Excel 双击打开不乱码，v1 的编码习惯）；
- JSON 用 ensure_ascii=False 的标准数组（历史导出用）；
- JSONL 每行一个 JSON 对象，支持流式追加（实时模式专用，用户拍板）；
- 实时模式的 CSV 走追加写（首行表头只在文件新建时写一次）。

隐私红线：导出文件含聊天明文，只允许落在 output/（已 gitignore）——本模块不做路径
校验（调用方传 cfg.output_dir 下的路径），但文件命名不包含 wxid（用展示名）。
"""
# 实现：模块 docstring 声明字段规格、格式口径与隐私边界。
# 功能：cli/realtime 落盘的唯一出口。

from __future__ import annotations

import csv
import json
import re
from datetime import datetime
from pathlib import Path

# 实现：import csv/json/re/datetime/Path 提供落盘与命名；零第三方依赖。
# 功能：本模块全部依赖。

CSV_HEADERS = [
    "会话类型(0群/1私)",
    "群聊名/私聊联系人名",
    "群内发送者(私聊为空)",
    "时间戳",
    "可读时间",
    "消息内容",
]
# 实现：CSV 表头常量（中文列名，Excel 里直观）。
# 解析：与 README/plan 的字段定义一一对应；JSON 键名走英文（见 make_record），两边语义同源。
# 功能：CSV 首行与顺序的唯一真值表。

FORMATS = ("csv", "json", "jsonl")
# 实现：合法格式集合。
# 功能：格式校验依据。

_INVALID_NAME_CHARS = re.compile(r'[\\/:*?"<>|\r\n\t]')
_NAME_MAX_LEN = 40
# 实现：文件名清洗用的非法字符正则与长度上限。
# 解析：Windows 文件名禁字符 + 换行类控制符；40 字上限沿用 v1 profiles._seg 的经验值
#       （过长的联系人名会把文件名撑爆）；清洗后仍可能重名——调用方按时间戳区分文件。
# 功能：分文件命名的安全清洗。


def sanitize_name(name: str) -> str:
    """展示名 → 可进文件名的安全段：非法字符替换为下划线、截断 40 字、去首尾空白。"""
    cleaned = _INVALID_NAME_CHARS.sub("_", name).strip()
    return cleaned[:_NAME_MAX_LEN] or "unnamed"
    # 实现：正则替换 + 截断 + 空名兜底。
    # 解析：展示名来自微信（用户可控），防路径穿越（../ 等经斜杠替换后无害）与非法字符。
    # 功能：分文件命名的名称段。


def timestamp_slug(now: datetime | None = None) -> str:
    """文件名时间戳段：YYYYmmdd_HHMMSS（本地时间）。"""
    return (now or datetime.now()).strftime("%Y%m%d_%H%M%S")
    # 实现：strftime 直出。
    # 功能：导出文件名的时间段（区分多次导出）。


def merged_filename(fmt: str, now: datetime | None = None) -> str:
    """合并模式文件名：chat_export_<时间戳>.<fmt>。"""
    _check_fmt(fmt)
    return f"chat_export_{timestamp_slug(now)}.{fmt}"
    # 实现：固定前缀 + 时间戳 + 格式后缀。
    # 功能：合并模式的导出文件名。


def session_filename(chat_type: int, display: str, fmt: str, now: datetime | None = None) -> str:
    """分文件模式文件名：私聊_张三_<时间戳>.csv / 群聊_前端群_<时间戳>.json(l)。"""
    _check_fmt(fmt)
    kind = "群聊" if chat_type == 0 else "私聊"
    return f"{kind}_{sanitize_name(display)}_{timestamp_slug(now)}.{fmt}"
    # 实现：类型前缀（0=群聊 1=私聊，与字段①同口径）+ 清洗后的展示名 + 时间戳。
    # 功能：分文件模式的导出文件名。


def _check_fmt(fmt: str) -> None:
    if fmt not in FORMATS:
        raise ValueError(f"不支持的导出格式：{fmt}（可选 {'/'.join(FORMATS)}）")
    # 实现：格式白名单校验，尽早失败。
    # 功能：所有命名/写出函数的统一入参闸。


def make_record(chat_type: int, session_name: str, sender_name: str | None, row: dict) -> dict:
    """一条消息 → 6 字段记录（JSON 形态；CSV 由 write_csv 按表头取列）。

    row 是导出器的消息行；sender_name 私聊传 None（JSON null / CSV 空串）。
    时间戳取原始 Unix 秒；可读时间按本机时区 %Y-%m-%d %H:%M:%S。
    """
    ts = int(row.get("create_time") or 0)
    return {
        "chat_type": chat_type,                      # 0=群聊 1=私聊（字段①）
        "session_name": session_name,                # 字段②（备注>昵称>微信号>id 回退后的展示名）
        "sender_name": sender_name,                  # 字段③（群聊发送者；私聊 None）
        "timestamp": ts,                             # 字段④（Unix 秒，严格"时间戳"）
        "time_readable": datetime.fromtimestamp(ts).strftime("%Y-%m-%d %H:%M:%S"),  # 字段⑤
        "content": row.get("content") or "",         # 字段⑥（render_content 的产出）
    }
    # 实现：六字段逐一定义；ts=0 会得到 1970 年的可读时间（真实库不会出现，防御性可读）。
    # 解析：字段⑤用本机时区——与微信 UI 显示一致（v1 _fmt_ts 同口径）；不做时区配置项，
    #       用户拍板的规格就是"可读时间"，本机时区是其唯一合理解释。
    # 功能：三条写出路径（CSV/JSON/JSONL）共用的记录构造。


def write_csv(path: Path, records: list[dict]) -> None:
    """CSV 覆盖写：表头 + 记录行；utf-8-sig 编码（Excel 兼容）。"""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w", encoding="utf-8-sig", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(CSV_HEADERS)
        for r in records:
            writer.writerow([r.get("chat_type", ""), r.get("session_name", ""),
                             "" if r.get("sender_name") is None else r.get("sender_name"),
                             r.get("timestamp", ""), r.get("time_readable", ""),
                             r.get("content", "")])
    # 实现：csv.writer 自动处理引号/换行转义（content 含逗号/换行安全）；newline="" 是
    # csv 模块的Windows 要求（防多出空行）；sender None → 空串（字段③私聊口径）。
    # 解析：先建父目录——output/ 可能还不存在。
    # 功能：历史导出的 CSV 落盘。


def write_json(path: Path, records: list[dict]) -> None:
    """JSON 覆盖写：标准数组，ensure_ascii=False，UTF-8，缩进 2。"""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as f:
        json.dump(records, f, ensure_ascii=False, indent=2)
    # 实现：标准 JSON 数组（历史导出用——实时模式用 append_jsonl）。
    # 功能：历史导出的 JSON 落盘。


def append_jsonl(path: Path, records: list[dict]) -> None:
    """JSONL 追加写：每行一个 JSON 对象（实时模式的 JSON 形态，用户拍板）。

    追加前不校验既有内容——调用方（realtime）保证同一路径只被一个轮询循环写。"""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "a", encoding="utf-8", newline="\n") as f:
        for r in records:
            f.write(json.dumps(r, ensure_ascii=False) + "\n")
    # 实现："a" 模式逐行写，每行一个完整 JSON 对象。
    # 解析：JSONL 的每行独立合法——文件被中途打断也保留已写入的行（实时模式的容错关键）。
    # 功能：实时模式的 JSON 追加。


def append_csv(path: Path, records: list[dict]) -> None:
    """CSV 追加写：文件不存在（或空）时先写表头，之后只追加数据行。"""
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    need_header = not path.exists() or path.stat().st_size == 0
    with open(path, "a", encoding="utf-8-sig", newline="") as f:
        writer = csv.writer(f)
        if need_header:
            writer.writerow(CSV_HEADERS)
        for r in records:
            writer.writerow([r.get("chat_type", ""), r.get("session_name", ""),
                             "" if r.get("sender_name") is None else r.get("sender_name"),
                             r.get("timestamp", ""), r.get("time_readable", ""),
                             r.get("content", "")])
    # 实现：need_header 判"新文件或空文件"——首次追加建表头，后续只加行。
    # 解析：编码与覆盖写一致（utf-8-sig）；空文件判 size==0 是防"建了文件但没写头"的中间态。
    # 功能：实时模式的 CSV 追加。
