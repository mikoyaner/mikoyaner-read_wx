"""语音 ASR：把导出的 silk blob 经 v1 vendor 的 voice2text.cjs（silk-wasm + SenseVoice）离线转写。

协议（v1 pipeline.transcribe_voice_messages 同款）：
- 调用：node voice2text.cjs <silk1> <silk2> ...（一批多条 silk 路径）
- stdout：每行一条 JSON {"file": 路径, "ok": bool, "text": str, "error": str}（NDJSON），
  诊断全走 stderr；退出码是批级成败信号，但部分成功也要从 stdout 捞回。
- 音频不出本机（全程离线模型）。

用户拍板的口径：语音消息导出 ASR 转文字；任何失败回退 [语音] 占位符（render.py 负责），
转写文本不加前缀、按原样进内容列。
"""
# 实现：模块 docstring 声明 ASR 协议与内容列口径。
# 功能：cli/realtime 调语音转写的唯一入口。

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path

# 实现：import json/os/subprocess/Path 提供解析、路径与子进程；零第三方依赖。
# 功能：本模块全部依赖。

_ARGV_CAP = 20000        # 单批 argv 字符预算（Windows 命令行上限 32767，留余量；v1 同款）
_BATCH_TIMEOUT_S = 600   # 单批转写超时（v1 的预算上限口径；模型加载约需几十秒，600s 覆盖大批）
# 实现：分批与超时常量。
# 解析：一条 silk 路径约 78 字，四百多条塞一批就会超 Windows CreateProcess 上限
#       （v1 实测踩过 WinError 206）；cap 取 20000 是给 node/js 前缀与引号留余量。
# 功能：run_asr 的批处理参数。


def collect_jobs(messages: list[dict], voice_dir: Path) -> list[tuple[dict, Path]]:
    """从消息行里收集可转写条目：(消息 dict 引用, silk 绝对路径)。

    只收 voice.status == "extracted" 且 voice.file 非空、且文件真在盘上的条目
    （isfile 滤掉"导出器说解出来了但文件已没了"的幽灵条目）。"""
    jobs: list[tuple[dict, Path]] = []
    for m in messages:
        v = m.get("voice") or {}
        if v.get("status") == "extracted" and v.get("file"):
            p = Path(voice_dir) / v["file"]
            if p.is_file():
                jobs.append((m, p))
    return jobs
    # 实现：线性扫描，存消息 dict 的引用（调用方后续按路径回查文本）。
    # 解析：`or {}` 兜住 voice 为 None 的非语音消息；isfile 二次确认防把不存在路径塞进 argv。
    # 功能：确定本轮真正可转写的清单。


def _batches(files: list[Path]) -> list[list[Path]]:
    """按 argv 字符预算贪心装箱（v1 同款算法）。"""
    batches: list[list[Path]] = []
    cur: list[Path] = []
    cur_len = 0
    for f in files:
        if cur and cur_len + len(str(f)) + 1 > _ARGV_CAP:
            batches.append(cur)
            cur, cur_len = [], 0
        cur.append(f)
        cur_len += len(str(f)) + 1
    if cur:
        batches.append(cur)
    return batches
    # 实现：cur_len 累计"路径字符数+1"，超预算封箱开新批；循环后补最后一批（`if cur` 不能漏）。
    # 解析：单条路径本身超 cap 时让它单独成批（宁可这批失败也不死循环）。
    # 功能：任意条数的语音切成命令行长度安全的批。


def run_asr(jobs: list[tuple[dict, Path]], cfg, log=None) -> tuple[dict[str, dict], list[str]]:
    """跑全部转写批，返回 (按 normpath 索引的结果表, 失败线索列表)。

    结果表键 = os.path.normpath(silk 绝对路径)，值 = voice2text.cjs 的结果 dict
    （含 ok/text/error 字段；ok 为真但 text 为空 = 静音/噪声，调用方按无文本处理）。
    """
    js = cfg.voice2text_js
    if js is None or not js.is_file():
        return {}, [f"语音 {len(jobs)} 条未转写：找不到 voice2text.cjs（检查 WXMSG_ASR_ROOT）"]
    # 实现：调用前 stat ASR 入口脚本，缺失直接整批跳过（v1 同款——否则每批都在 subprocess 抛 OSError）。
    # 功能：把"环境没装好"与"转写跑失败"两类原因分开报。
    by_file: dict[str, dict] = {}
    why: list[str] = []
    for bi, batch in enumerate(_batches([p for _, p in jobs]), 1):
        argv = [cfg.node_bin, str(js), *[str(p) for p in batch]]
        try:
            proc = subprocess.run(
                argv, capture_output=True, text=True, encoding="utf-8",
                errors="replace", timeout=_BATCH_TIMEOUT_S,
            )
        except (OSError, subprocess.TimeoutExpired) as e:
            why.append(f"第 {bi} 批 ASR 运行失败 {type(e).__name__}")
            continue
        # 实现：同步阻塞起 node 子进程，capture_output 收走 stdout/stderr（voice2text.cjs 的
        # stderr 有依赖绝对路径，绝不能让它进面板）；异常只记类型名不记 str(e)——
        # TimeoutExpired 的字符串带完整命令行（含数据根路径），那属于身份信息。
        # 功能：跑一批离线 ASR，失败只降级这一批。
        tail = (proc.stderr or "").strip().splitlines()
        if proc.returncode != 0:
            why.append(f"第 {bi} 批 node 退出码 {proc.returncode}"
                       + (f"：{tail[-1][:160]}" if tail else ""))
        # 实现：退出码非 0 时取 stderr 末行拼线索（截 160 字符防刷屏）。
        # 解析：voice2text.cjs 约定 stdout=NDJSON、stderr=诊断，退出码是批级信号；
        #       不 continue——node 可能"前 N 条成功、后一条崩"，stdout 仍要解析。
        # 功能：批级失败归因。
        for line in (proc.stdout or "").splitlines():
            line = line.strip()
            if not line.startswith("{"):
                continue
            try:
                rec = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(rec, dict) and rec.get("file"):
                by_file[os.path.normpath(rec["file"])] = rec
        # 实现：只认以 "{" 开头的行（滤掉 sherpa 原生层偶尔打到 stdout 的加载日志），
        # 单行解析失败只丢这一行；键过 normpath 消掉斜杠方向差异（v1 同款）。
        # 功能：跨批汇总结果表。
    if log:
        for line in why:
            log(line)
    return by_file, why
    # 实现：失败线索回调 log（进面板状态）并原样返回给调用方拼 note。
    # 功能：交付可按路径回查的转写结果与失败原因。


def transcribe_messages(messages: list[dict], cfg, voice_dir: Path,
                        log=None) -> tuple[dict[str, str | None], str]:
    """一次性转写消息流里的全部语音，返回 (silk路径 → 转写文本|None, 状态说明)。

    note 为空串 = 全部成功；非空 = 有降级（全部失败/部分失败/环境缺失），
    面板把 note 展示出来——语音是对方原话，转不出来绝不能静默。
    """
    jobs = collect_jobs(messages, voice_dir)
    if not jobs:
        return {}, ""
    by_file, why = run_asr(jobs, cfg, log=log)
    text_map: dict[str, str | None] = {}
    ok = 0
    for _, p in jobs:
        rec = by_file.get(os.path.normpath(str(p)))
        text = rec.get("text") if rec and rec.get("ok") else None
        text_map[os.path.normpath(str(p))] = text or None
        if text:
            ok += 1
    # 实现：按 jobs 原顺序逐条回查，三重条件（有结果/ok/文本非空）都满足才算成功；
    # text 判空必须做——SenseVoice 对静音也回 ok:true 但 text 为空串（v1 口径）。
    # 功能：产出 render 回调用的 文本表。
    total = len(jobs)
    if ok == 0:
        note = (f"语音 {total} 条全部未转写"
                + ("：" + "；".join(why[:3]) if why else "（node 没报错也没出结果）"))
    elif ok < total or why:
        note = (f"语音 {total} 条只转写成功 {ok} 条"
                + ("；" + "；".join(why[:2]) if why else ""))
    else:
        note = ""
    # 实现：三档口径（全好/部分好/全坏）与 v1 一致；why 截前几条防状态行撑爆。
    # 功能：把任何降级如实说出来。
    return text_map, note


def make_transcriber(text_map: dict[str, str | None], voice_dir: Path):
    """构造 render_content 需要的 transcribe 回调：voice_file 名 → 转写文本|None。"""
    def transcribe(voice_file: str) -> str | None:
        return text_map.get(os.path.normpath(str(Path(voice_dir) / voice_file)))
    return transcribe
    # 实现：闭包把 voice.file 相对名拼回 silk 绝对路径再查表（键口径与 transcribe_messages 一致）。
    # 功能：render_content 的语音分支数据源。
