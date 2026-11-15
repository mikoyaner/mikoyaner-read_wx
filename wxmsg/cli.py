"""历史导出端到端编排：选中的会话 → 取消息 → 群成员筛选 → 渲染 → 落盘。

流水线（对应用户确认的需求）：
  1. 对每个 Target 调导出器 --messages（时间窗 begin/end 或最近 recent 条）；
  2. 群聊按勾选成员筛选发送者（None=全群；私聊不过滤——双方消息都导，用户拍板）；
  3. 配置了 ASR 时把结果集里的语音 silk 转文字（voice.py），失败回退 [语音]；
  4. 逐行渲染内容列（render.py）、解析发送者展示名（字段③回退链）；
  5. 按组织方式落盘：合并单文件 / 每会话一文件（用户拍板：面板可选，默认合并）。

本模块不做任何交互——面板（panel.py）负责收集用户选择后调用 run_export；
cli.py 的 __main__ 入口在步骤 11 接线（启动面板）。
"""
# 实现：模块 docstring 声明流水线与职责边界。
# 功能：面板历史导出与（步骤 10）实时模式的共用编排层。

from __future__ import annotations

from dataclasses import dataclass, field
from datetime import datetime
from pathlib import Path

from . import exporter, outfile, render, search as search_mod, session, voice
from .config import Config, ConfigError, load_config

# 实现：import 同包四模块与 Config。
# 功能：编排层全部依赖。


@dataclass
class Target:
    """一个导出目标：面板勾选后的最小描述。

    selected_members：群内勾选的成员 username 集合；None=全群（含我）。
    私聊忽略该字段（双方都导，字段③恒 null）。"""
    sid: str
    chat_type: int                      # 0=群聊 1=私聊（字段①）
    display: str                        # 展示名（字段②，面板从 Session.display_name() 得来）
    selected_members: set[str] | None = None
    # 实现：dataclass 四字段。
    # 解析：display 由面板算好传入（回退链只在 session.py 实现一次）；selected_members 用
    #       set 保证筛选是 O(1) 查找；None 与空集语义不同——None=不筛，空集=筛出一个都没有。
    # 功能：run_export 的输入单元。


def parse_time_boundary(raw: str, *, end_of_day: bool = False) -> int:
    """面板传来的时间边界 → Unix 秒。

    支持三种写法：空串/None=0（不限）；"YYYY-MM-DD"；"YYYY-MM-DD HH:MM"。
    日期默认 00:00:00；end_of_day=True 时日期解释为当天 23:59:59（结束边界常用）。
    非法格式抛 ValueError，面板层转成用户可读提示。"""
    if raw is None or not str(raw).strip():
        return 0
    text = str(raw).strip()
    for fmt in ("%Y-%m-%d %H:%M", "%Y-%m-%d"):
        try:
            dt = datetime.strptime(text, fmt)
        except ValueError:
            continue
        if end_of_day and fmt == "%Y-%m-%d":
            dt = dt.replace(hour=23, minute=59, second=59)
        return int(dt.timestamp())
    raise ValueError(f"时间格式不合法：{text}（支持 2026-01-31 或 2026-01-31 12:30）")
    # 实现：两种格式逐个 strptime；end_of_day 只作用于纯日期（带时刻的结束边界本来就精确）。
    # 解析：timestamp() 按本机时区——与字段⑤可读时间的口径一致（用户在面板填的"2026-01-01"
    #       按他看到的时间理解）。非法输入明确报错而不是当 0（当 0 会静默变成"不限"，危险）。
    # 功能：面板时间范围的解析出口。


@dataclass
class _Fetch:
    """一次 --messages 取数的归一结果：行 + 路由诊断 + 语音 note。"""
    rows: list[dict] = field(default_factory=list)
    route: str = ""
    sid_found: bool = True
    voice_note: str = ""
    # 实现：内部 dataclass 聚合单次取数产物。
    # 功能：把 exporter JSON 的关键字段收拢，主流程不必反复 .get。


def _fetch_messages(cfg: Config, target: Target, begin: int, end: int,
                    recent: int | None, voice_dir: Path | None,
                    argv_prefix=None, rescan=None, log=None) -> _Fetch:
    """调导出器取一个会话的消息并做群成员筛选（字段③的行级过滤在这里）。"""
    exp = exporter.messages(cfg, target.sid, begin=begin, end=end, recent=recent,
                            voice_out=voice_dir, argv_prefix=argv_prefix,
                            rescan=rescan, log=log)
    rows = exp.get("messages", []) or []
    route = exp.get("route", "")
    sid_found = bool(exp.get("sid_found", True))
    note = ""
    # 群聊成员筛选：selected_members=None 表示不筛（全群）；空集筛出 0 条是合法语义。
    # 私聊不过滤（双方都导，用户拍板）。
    if target.chat_type == 0 and target.selected_members is not None:
        rows = [r for r in rows if (r.get("sender_username") or "") in target.selected_members]
    # 语音转写：配置了 ASR 才落 silk 并转写；否则 [语音] 占位（render 分支处理）。
    if voice_dir is not None:
        text_map, note = voice.transcribe_messages(rows, cfg, voice_dir, log=log)
    else:
        text_map, note = {}, ""
    transcribe = voice.make_transcriber(text_map, voice_dir) if voice_dir else None
    # 实现：筛选用 sender_username 精确匹配（wxid 稳定键）；语音转写只对筛后的行做——
    #       被筛掉的消息不值得花 ASR 时间。
    # 解析：transcribe 闭包与 rows 一起交给主流程渲染。
    # 功能：单会话取数 + 行级筛选 + ASR 的完整一步。
    rendered_rows = []
    for r in session.sort_messages(rows):
        rendered = dict(r)
        rendered["content"] = render.render_content(r, transcribe)
        rendered_rows.append(rendered)
    # 实现：排序后逐行渲染内容列（dict 拷贝避免污染原行——同一条 row 可能被多处引用）。
    # 功能：交付渲染完毕、时间正序的行。
    return _Fetch(rows=rendered_rows, route=route, sid_found=sid_found, voice_note=note)


def _sender_display(target: Target, row: dict, sender_map: dict[str, str]) -> str | None:
    """字段③取值：私聊恒 None；群聊按 回退链映射，映射缺失回退原始 wxid。"""
    if target.chat_type != 0:
        return None
    su = row.get("sender_username") or ""
    return sender_map.get(su) or (su or None)
    # 实现：三段式——私聊 None / 映射命中 / 原始 id 兜底。
    # 解析：发送者不在群成员表（已退群）时用原始 wxid 而不是丢字段；
    #       su 为空（库里没登记发送者）给 None，如实反映"不知道是谁发的"。
    # 功能：导出字段③的唯一取值口。


def run_export(cfg: Config, targets: list[Target], fmt: str = "csv", *,
               merged: bool = True, begin: int = 0, end: int = 0,
               recent: int | None = None, now: datetime | None = None,
               argv_prefix=None, rescan=None, log=None) -> dict:
    """历史导出主入口。返回摘要 dict（面板展示用）：

    {"files": [{sid, display, chat_type, count, path}], "total": N, "voice_note": str,
     "warnings": [str]}"""
    if not targets:
        raise ValueError("没有选择任何会话")
    outfile._check_fmt(fmt)
    # 实现：空目标与格式白名单在最前面闸住。
    # 功能：入参防线。
    out_dir = Path(cfg.output_dir)
    out_dir.mkdir(parents=True, exist_ok=True)
    # 语音 silk 目录共享（文件名含会话段不会互相覆盖）；没配 ASR 就完全不碰语音字节。
    voice_dir = (out_dir / "voices") if cfg.voice2text_js else None
    now = now or datetime.now()
    per_session: list[dict] = []
    warnings: list[str] = []
    # 群成员索引（一次导出器进程覆盖全部群）：发送者取名（字段③组合格式）的数据源。
    # 成员表在本机微信 4.1.15 上失效（诊断结论），索引的发言者池回退是实际数据来源。
    member_index: dict | None = None
    if any(t.chat_type == 0 for t in targets):
        try:
            member_index = search_mod.build_member_index(
                exporter.group_members_all(cfg, argv_prefix=argv_prefix,
                                           rescan=rescan, log=log))
        except exporter.ExporterError as e:
            warnings.append(f"群成员索引构建失败，群聊发送者将显示为原始微信号（{e}）")
    for t in targets:
        # 发送者取名表：username → 组合名（备注 昵称 群内昵称 微信号；全空 → 原始 id）。
        sender_map: dict[str, str] = {}
        if t.chat_type == 0:
            entry = (member_index or {}).get(t.sid)
            if not entry:
                warnings.append(f"{t.display}：成员索引里没有这个群（安静群或刚换号），发送者将显示为原始微信号")
            for m in (entry or {}).get("members") or []:
                u = m.get("username") or ""
                if u:
                    sender_map[u] = session.combined_name(
                        m.get("remark"), m.get("nickname"), m.get("group_display"),
                        m.get("alias"), fallback=u)
        fetch = _fetch_messages(cfg, t, begin, end, recent, voice_dir,
                                argv_prefix=argv_prefix, rescan=rescan, log=log)
        if not fetch.sid_found:
            warnings.append(f"{t.display}：当前登录账号里没有这个会话（sid 未命中），结果可能为空")
        if fetch.voice_note:
            warnings.append(f"{t.display}：{fetch.voice_note}")
        records = [outfile.make_record(t.chat_type, t.display,
                                       _sender_display(t, r, sender_map), r)
                   for r in fetch.rows]
        per_session.append({
            "sid": t.sid, "display": t.display, "chat_type": t.chat_type,
            "count": len(records), "records": records,
        })
        # 实现：逐目标走 取数→筛选→ASR→渲染→记录；成员表失败/会话未命中/语音降级都进 warnings
        #       而不是中断——部分会话失败时其余会话照常导出（面板把 warnings 说清楚）。
        # 解析：records 在这里组装（渲染行 + 字段②③），落盘交给最后的组织分支。
        # 功能：单会话完整流水线。
    # 组织方式：合并单文件 / 每会话一文件（用户拍板：面板可选，默认合并）。
    files: list[dict] = []
    if merged:
        all_records: list[dict] = []
        for ps in per_session:
            all_records.extend(ps["records"])
        path = out_dir / outfile.merged_filename(fmt, now)
        _write(fmt, path, all_records)
        files.append({"sid": "", "display": "(合并)", "chat_type": -1,
                      "count": len(all_records), "path": str(path)})
    else:
        for ps in per_session:
            path = out_dir / outfile.session_filename(ps["chat_type"], ps["display"], fmt, now)
            _write(fmt, path, ps["records"])
            files.append({"sid": ps["sid"], "display": ps["display"],
                          "chat_type": ps["chat_type"], "count": ps["count"],
                          "path": str(path)})
    # 实现：合并 = 全部 records 顺序拼接成一个文件；分文件 = 每会话独立命名独立落盘。
    # 解析：合并文件里各会话靠字段②区分；同一会话重复选择时按输入顺序拼接（sort_messages
    #       已保证会话内有序，跨会话顺序=选择顺序）。
    # 功能：两种组织方式的落盘出口。
    total = sum(ps["count"] for ps in per_session)
    return {
        "files": files,
        "total": total,
        "warnings": warnings,
    }
    # 功能：面板导出结果页的数据源。


def _write(fmt: str, path: Path, records: list[dict]) -> None:
    """按格式分发到 outfile 的覆盖写函数。"""
    if fmt == "csv":
        outfile.write_csv(path, records)
    elif fmt == "json":
        outfile.write_json(path, records)
    else:
        # 历史导出不写 jsonl（实时模式专用）：行式形态对"一次性完整文件"没有意义。
        raise ValueError("历史导出不支持 jsonl（实时模式的 JSON 走 JSONL 追加）")
    # 实现：csv/json 覆盖写直通；jsonl 明确拒绝。
    # 解析：明确报错优于静默写出一个既像 JSON 又不是数组的文件。
    # 功能：格式分发。


def main(argv=None) -> int:
    """面板启动入口（start_panel.bat 调用）：加载配置 → 起本地面板服务（阻塞）。

    用法：python -m wxmsg.cli [--port 8765]
    必须以 -m 方式运行——cli.py 在 wxmsg 包内，直接按文件路径跑会破坏相对导入。
    """
    import sys
    try:
        sys.stdout.reconfigure(encoding="utf-8")
        sys.stderr.reconfigure(encoding="utf-8")
    except (AttributeError, ValueError):
        pass  # 极旧解释器或已重定向的流没有 reconfigure——不致命（面板输出以浏览器为主）
    # 实现：UTF-8 控制台（中文横幅/错误在 GBK 控制台不乱码，v1 panel.py 同款防护）。
    argv = list(sys.argv[1:] if argv is None else argv)
    port = 8765
    if "--port" in argv:
        i = argv.index("--port")
        if i + 1 < len(argv):
            try:
                port = int(argv[i + 1])
            except ValueError:
                print(f"[用法错误] --port 需要数字端口，收到 {argv[i + 1]!r}")
                return 2
    # 运行缓存全部落在项目 cache/ 下（用户拍板：不写 C 盘）；首次启动把 v1 旧密钥库迁过来。
    from . import keys as _keys
    note = _keys.migrate_legacy_store()
    if note:
        print(f"[keys] {note}")
    try:
        cfg = load_config()
    except ConfigError:
        # 首次运行（或 .env 缺数据根）不再是死路：进面板 setup 模式，页面上填路径自动写 .env。
        print("[配置] 尚未配置微信数据根——面板已启动，请在打开的页面上填写（自动写入 .env）")
        from . import panel
        try:
            panel.serve(None, port=port)
        except OSError as e:
            print(f"[启动失败] 端口 {port} 可能被占用（{e}）。换个端口：python -m wxmsg.cli --port 8766")
            return 1
        return 0
    # panel 在函数内导入而非模块顶部：panel→cli 与 cli→panel 双向引用，顶层互导有初始化顺序风险，
    # 函数内导入让两边都加载完成后再建立引用，稳妥且对正常使用无影响。
    from . import panel
    try:
        panel.serve(cfg, port=port)
    except OSError as e:
        print(f"[启动失败] 端口 {port} 可能被占用（{e}）。换个端口试试：python -m wxmsg.cli --port 8766")
        return 1
    return 0
    # 实现：UTF-8 防护 → --port 解析 → 配置硬闸 → 起面板。
    # 解析：配置错误返回 2（bat 会 pause 让用户看到文案）；端口占用返回 1 并给换端口指引。
    # 功能：双击入口的全部行为。


if __name__ == "__main__":
    raise SystemExit(main())
    # 功能：python -m wxmsg.cli 的执行入口。
