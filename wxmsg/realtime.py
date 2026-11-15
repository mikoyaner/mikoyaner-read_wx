"""实时提取：游标增量轮询 + 追加落盘（面板前端定时触发 poll_once 驱动）。

语义（全部经用户拍板）：
- 每 1 分钟增量读取一次（间隔面板可调，配置默认 60s）——实现为「每分钟增量读数据库文件」，
  密钥只在失效时自动重扫（exporter 层已内置），不反复扫进程内存；
- 起点：勾选「同时导出历史」则先把历史（可叠加时间范围/最近N）写进实时文件再增量；
  不勾则只收启动时刻之后的新消息；
- 落盘：JSON 走 JSONL 追加（每行独立合法 JSON）；CSV 走追加写（首建表头一次）；
- 组织：合并单文件 / 每会话一文件，与历史导出同口径。

游标与去重：每会话记录已写到的 (create_time, sort_seq, local_id) 三元组；每轮以
begin=上一秒重取（导出器闭区间），同秒重叠部分用三元组字典序过滤——同秒后到的消息
（sort_seq 更大）下一轮必然被收到，绝不丢也不重。
"""
# 实现：模块 docstring 声明实时语义与游标口径。
# 功能：面板实时监听页的状态机。

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
from pathlib import Path

from . import cli, exporter, outfile, search as search_mod, session, voice
from .config import Config

# 实现：import 同包 cli（复用 _fetch_messages 的取数+筛选+ASR+渲染流水线）等。
# 功能：实时层全部依赖。


def _row_key(row: dict) -> tuple[int, int, int]:
    """行的全序键：(create_time, sort_seq, local_id)——与会话内真实顺序一致。"""
    return (int(row.get("create_time") or 0),
            int(row.get("sort_seq") or 0),
            int(row.get("local_id") or 0))
    # 实现：三元组 key（sort_seq 是同秒内权威顺序，local_id 是最终 tiebreaker）。
    # 功能：游标比较与排序的唯一键。


@dataclass
class _State:
    """单个会话的实时状态：目标 + 已写游标 + 已写条数。"""
    target: cli.Target
    cursor: tuple[int, int, int]        # 已写到（不含）的最大行键
    path: Path                          # 本会话的追加目标文件（合并模式下指向同一文件）
    count: int = 0
    # 实现：dataclass 四字段。
    # 功能：poll_once 的游标推进依据。


class RealtimeExport:
    """一次实时监听的完整状态。生命周期 = 面板页面（前端定时调 poll_once 驱动）。

    用法：构造（完成初始导出）→ 反复 poll_once() → summary() 随时取状态。
    注意：这里是手写 __init__（不是 @dataclass）——构造时要完成"起点导出"等副作用，
    dataclass 生成的 __init__ 做不了这些。
    """
    cfg: Config
    fmt: str                            # 实际落盘格式："csv" 或 "jsonl"（json 会被换成为 jsonl）
    merged: bool
    started: datetime
    path_merged: Path | None            # 合并模式的唯一文件
    states: dict                        # sid → _State
    voice_dir: Path | None
    last_poll: str                      # 最近一轮 poll 的时间（面板展示）

    def __init__(self, cfg: Config, targets: list[cli.Target], fmt: str = "csv", *,
                 merged: bool = True, include_history: bool = False,
                 history_begin: int = 0, history_end: int = 0, history_recent: int | None = None,
                 now: datetime | None = None,
                 argv_prefix=None, rescan=None, log=None):
        if not targets:
            raise ValueError("没有选择任何会话")
        if fmt not in ("csv", "json"):
            raise ValueError("实时模式只支持 csv / json 两种格式（json 自动落 JSONL）")
        self.cfg = cfg
        self.merged = merged
        self.fmt = "jsonl" if fmt == "json" else fmt
        self.started = now or datetime.now()
        self._argv_prefix = argv_prefix     # 测试注入点（_records 拉成员表时复用同一替身）
        self._rescan = rescan
        self.warnings: list[str] = []
        # 群成员索引：有群目标时构建一次（发送者取名数据源，与 cli.run_export 同口径）；
        # 失败降级为空索引（发送者退化原始 id），warning 留给面板展示。
        self._member_index: dict | None = None
        if any(t.chat_type == 0 for t in targets):
            try:
                self._member_index = search_mod.build_member_index(
                    exporter.group_members_all(cfg, argv_prefix=argv_prefix,
                                               rescan=rescan, log=log))
            except exporter.ExporterError as e:
                self.warnings.append(
                    f"群成员索引构建失败，群聊发送者将显示为原始微信号（{e}）")
        out_dir = Path(cfg.output_dir)
        out_dir.mkdir(parents=True, exist_ok=True)
        self.voice_dir = (out_dir / "voices") if cfg.voice2text_js else None
        self.states: dict = {}
        # 文件按组织方式分配：合并 → 单文件；分文件 → 每会话一个。
        self.path_merged = out_dir / outfile.realtime_filename(self.fmt, self.started)
        for t in targets:
            path = self.path_merged if merged else \
                out_dir / outfile.realtime_session_filename(t.chat_type, t.display, self.fmt, self.started)
            # 初始游标：不导历史 = 启动时刻（只收启动后的新消息）；
            # 导历史 = 先把历史写进文件，游标推到已写到的最大行键。
            if include_history:
                fetch = cli._fetch_messages(cfg, t, history_begin, history_end, history_recent,
                                            self.voice_dir, argv_prefix=argv_prefix,
                                            rescan=rescan, log=log)
                records = self._records(t, fetch.rows)
                self._append(path, records)
                cursor = max((_row_key(r) for r in fetch.rows), default=(0, 0, 0))
                count = len(records)
            else:
                cursor = (int(self.started.timestamp()), 0, 0)
                count = 0
            self.states[t.sid] = _State(target=t, cursor=cursor, path=path, count=count)
        # 实现：构造即完成"起点"语义——include_history 决定游标从 0（历史全量）还是启动时刻起算。
        # 解析：include_history 时历史与新消息进同一个文件（用户拍板"先导历史再实时追加"，
        #       而不是出两个文件）；游标取 fetch 行的最大键，同秒末条之后的新消息下一轮必收。
        # 功能：实时监听的起点。
        self.last_poll = self.started.strftime("%Y-%m-%d %H:%M:%S")

    def _records(self, target: cli.Target, rendered_rows: list[dict]) -> list[dict]:
        """渲染行 → 6 字段记录（字段③口径与历史导出一致：私聊 null，群聊走组合名映射）。"""
        sender_map: dict[str, str] = {}
        if target.chat_type == 0:
            # 用启动时构建的成员索引取名（组合格式：备注 昵称 群内昵称 微信号）；
            # 索引缺失/群不在索引里 → 发送者退化原始 wxid（与历史导出同口径）。
            entry = (self._member_index or {}).get(target.sid) or {}
            for m in entry.get("members") or []:
                u = m.get("username") or ""
                if u:
                    sender_map[u] = session.combined_name(
                        m.get("remark"), m.get("nickname"), m.get("group_display"),
                        m.get("alias"), fallback=u)
        return [outfile.make_record(target.chat_type, target.display,
                                    cli._sender_display(target, r, sender_map), r)
                for r in rendered_rows]
        # 实现：群聊每轮重新拉成员表——退群/改备注能及时反映；失败静默退化（首轮已可警示）。
        # 解析：sender_map 失败退化与 cli.run_export 相同；这里不重复发 warning（面板轮询场景
        #       每分钟刷一条同样的 warning 会淹掉真信息）。
        # 功能：poll 产出记录的唯一构造。

    def _append(self, path: Path, records: list[dict]) -> None:
        """按格式追加：csv 走 append_csv（首建表头），jsonl 走 append_jsonl。"""
        if self.fmt == "csv":
            outfile.append_csv(path, records)
        else:
            outfile.append_jsonl(path, records)
        self._wrote_any = True
        # 实现：写入口径集中在这一处。
        # 功能：历史初始导出与每轮增量共用。

    def poll_once(self, *, argv_prefix=None, rescan=None, log=None) -> dict:
        """执行一轮增量：取新消息 → 渲染 → 追加 → 推进游标。返回本轮摘要（面板展示）。"""
        # 注入点跟随轮次更新（保持与构造时同一替身，测试可按轮次换行为）。
        self._argv_prefix = argv_prefix
        self._rescan = rescan
        per_session = []
        total_new = 0
        now_str = datetime.now().strftime("%Y-%m-%d %H:%M:%S")
        for st in self.states.values():
            fetch = cli._fetch_messages(self.cfg, st.target, st.cursor[0], 0, None,
                                        self.voice_dir, argv_prefix=argv_prefix,
                                        rescan=rescan, log=log)
            # 增量过滤：只收严格大于游标键的行（begin=上一秒导致同秒重叠，靠字典序去重）。
            new_rows = [r for r in fetch.rows if _row_key(r) > st.cursor]
            records = self._records(st.target, new_rows)
            if records:
                self._append(st.path, records)
                st.cursor = max(_row_key(r) for r in new_rows)
                st.count += len(records)
            total_new += len(records)
            per_session.append({"sid": st.target.sid, "display": st.target.display,
                                "new": len(records), "total": st.count})
        self.last_poll = now_str
        return {"new_total": total_new, "sessions": per_session, "at": now_str}
        # 实现：begin=游标秒（闭区间重取同秒重叠），_row_key 严格大于过滤增量；
        #       推进游标用本轮新行的最大键——下一轮从那一秒继续。
        # 解析：这样同秒内"后到的消息"（sort_seq 更大）下一轮必收：导出器按 sort_seq 序返回，
        #       本轮取到 seq<=游标 的重叠行全被滤掉，seq 更大的行 key 更大被追加。绝不丢不重。
        # 功能：实时增量的核心一步（面板前端每分钟调一次）。

    def summary(self) -> dict:
        """面板实时页的状态数据：文件清单/各会话计数/最近轮询时间。"""
        if self.merged:
            files = [{"sid": "", "display": "(合并)", "path": str(self.path_merged),
                      "count": sum(s.count for s in self.states.values())}]
        else:
            files = [{"sid": sid, "display": s.target.display, "path": str(s.path),
                      "count": s.count} for sid, s in self.states.items()]
        return {"started": self.started.strftime("%Y-%m-%d %H:%M:%S"),
                "last_poll": self.last_poll, "fmt": self.fmt,
                "files": files,
                "total": sum(s.count for s in self.states.values())}
        # 实现：两种组织方式的状态汇总。
        # 功能：面板实时页的轮询展示数据。
