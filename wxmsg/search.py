"""群成员搜索索引 + 名字搜索 + 搜索历史存取（面板 ② 区「搜索→添加」交互的数据层）。

- build_member_index：把 Rust --group-members-all 的 JSON 转成按群索引的成员表；
- search_privates / search_groups：私聊搜索（备注/昵称/微信号）与群搜索（群名 + 群·成员组合），
  评分复用 ranked_sessions 的口径（程度 3/2/1 × 权重 备注5>微信号4>昵称3>会话显示名2）；
- SearchHistory：搜索历史词存取（用户拍板 autocomplete 只用历史词），cache/search_history.json，
  私聊/群聊两个独立列表、各最多 50 条、去重、最近在前。
"""
# 实现：模块 docstring 声明三个职责与评分口径来源。
# 功能：panel ② 区搜索/选择交互的唯一数据层。

from __future__ import annotations

import json
from pathlib import Path

from .session import Session, _match_degree, ranked_sessions

# 实现：import json/Path + session 的评分器（口径唯一，不另写一套）。
# 功能：本模块全部依赖。

HISTORY_MAX = 50
# 实现：每类搜索历史的条数上限。
# 解析：datalist 里塞几百条会让联想框又长又乱；50 条是"最近用过的"的合理窗口。
# 功能：SearchHistory 的容量闸。


def build_member_index(groups_export: dict) -> dict:
    """--group-members-all JSON → {sid: {"name": 群名, "members": [成员 dict]}}。

    成员 dict 保留 Rust 原始字段（username/group_display/remark/nickname/alias/source）；
    Rust 端每群的 error 字段（回退原因）透传为索引里该群的 "error"，供前端展示。"""
    index: dict = {}
    for g in groups_export.get("groups", []):
        index[g.get("sid") or ""] = {
            "name": g.get("name") or "",
            "error": g.get("error"),
            "members": g.get("members") or [],
        }
    return index
    # 实现：按 sid 建索引，宽松兜空（Rust 端字段都有值，容 fixture/未来版本）。
    # 功能：群成员搜索的索引结构。


def search_privates(sessions: list[Session], query: str) -> list[dict]:
    """私聊搜索：ranked_sessions 的子集（只保留非群、非公众号），包装 {kind:"priv"}。"""
    out = []
    for hit in ranked_sessions(sessions, query):
        s = next(x for x in sessions if x.sid == hit["sid"])
        if s.is_group or s.sid.startswith("gh_"):
            continue
        out.append({"kind": "priv", **hit})
    return out
    # 实现：复用 ranked_sessions 评分，再按会话属性过滤。
    # 解析：next 线性查找——候选数最多几十，可接受；公众号不进私聊搜索结果。
    # 功能：面板第一行搜索的数据源。


def search_groups(index: dict, sessions: list[Session], query: str) -> list[dict]:
    """群搜索：①命中群名 → {kind:"group"}（导该群全部消息）；
    ②命中群内成员名（备注/昵称/微信号/原始id）→ {kind:"member"}（导该群该成员消息）。

    排序：群在前、成员在后；各自按得分降序、同分按显示名字典序。"""
    q = (query or "").strip()
    if not q:
        return []
    group_sessions = [s for s in sessions if s.is_group]
    # ① 群名命中（ranked_sessions 已按得分降序）
    group_hits = ranked_sessions(group_sessions, query)
    out: list[dict] = [{"kind": "group", **h} for h in group_hits]
    # ② 群·成员组合：遍历索引里每个群的成员，按成员字段评分（权重与 ranked_sessions 一致）。
    member_hits: list[dict] = []
    for sid, entry in index.items():
        gname = entry.get("name") or ""
        for m in entry.get("members") or []:
            best = None  # (degree, rank, tag)
            for value, rank, tag in (
                (m.get("remark") or "", 5, "备注"),
                (m.get("alias") or "", 4, "微信号"),
                (m.get("username") or "", 4, "原始id"),
                (m.get("nickname") or "", 3, "昵称"),
            ):
                degree = _match_degree(q, value)
                if degree and (best is None or degree * 10 + rank > best[0] * 10 + best[1]):
                    best = (degree, rank, tag)
            if best is None:
                continue
            # 成员展示名走与导出字段③相同的回退链（备注>昵称>微信号>id）。
            display = _first_not_blank(m.get("remark"), m.get("nickname"), m.get("alias")) \
                or m.get("username") or ""
            member_hits.append({
                "kind": "member",
                "sid": sid,
                "username": m.get("username") or "",
                "group_name": gname,
                "member_display": display,
                "group_display": m.get("group_display") or "",
                "display": f"{gname}·{display}" if gname else display,
                "source": m.get("source") or "",
                "degree": best[0],
                "field": best[2],
                "score": best[0] * 10 + best[1],
            })
    member_hits.sort(key=lambda x: (-x["score"], x["display"]))
    out.extend(member_hits)
    return out
    # 实现：群名走 ranked_sessions（会话显示名/备注/昵称都在群会话字段里）；
    #       成员组合在索引上逐群逐成员评分，排序规则与群命中一致（得分降序+字典序）。
    # 解析：成员条目带 source（roster/sender_pool）与 group_display，前端标注来源辅助辨认；
    #       群名命中只给群（不叠加"群·我"之类的组合——导全群已包含所有成员）。
    # 功能：面板第二行搜索的数据源。


def _first_not_blank(*values: str) -> str:
    """第一个 strip 后非空的值（与 session.py 的展示回退链同语义）。"""
    for v in values:
        if v and v.strip():
            return v.strip()
    return ""
    # 实现：回退链原语（session.py 同名私有函数在此模块的场景复用——
    # 成员 dict 不是 Session 对象，走不到那个实现）。
    # 功能：成员条目的展示名计算。


class SearchHistory:
    """搜索历史词（用户拍板：autocomplete 只用历史词）。

    存储 cache/search_history.json：{"priv": [...], "group": [...]}，两类独立、
    各最多 HISTORY_MAX 条、去重、最近在前。文件损坏/缺失一律按空历史起步（绝不因缓存坏而崩）。"""

    def __init__(self, path: Path):
        self.path = Path(path)
        self._data = self._load()
        # 实现：路径注入（生产=项目 cache/；测试=tmp）。
        # 功能：两类搜索词的持久化容器。

    def _load(self) -> dict:
        try:
            data = json.loads(self.path.read_text(encoding="utf-8-sig"))
        except (OSError, ValueError):
            return {"priv": [], "group": []}
        if not isinstance(data, dict):
            return {"priv": [], "group": []}
        return {
            "priv": [str(x) for x in data.get("priv") or [] if str(x).strip()],
            "group": [str(x) for x in data.get("group") or [] if str(x).strip()],
        }
        # 实现：utf-8-sig 读（兼容记事本）；坏文件/坏结构一律空表起步。
        # 解析：缓存坏了不该让面板崩——最坏情况只是没有历史词联想。
        # 功能：历史装载。

    def terms(self, kind: str) -> list[str]:
        """某类搜索的历史词（最近在前）。kind 只能是 priv / group。"""
        if kind not in ("priv", "group"):
            raise ValueError(f"未知的历史类别 {kind}")
        return list(self._data.get(kind) or [])
        # 实现：拷贝返回（调用方改不动内部状态）。
        # 功能：datalist 数据源。

    def add(self, kind: str, term: str) -> None:
        """记一次搜索词：去重、置顶、截断到 HISTORY_MAX、立即写盘。"""
        if kind not in ("priv", "group"):
            raise ValueError(f"未知的历史类别 {kind}")
        term = (term or "").strip()
        if not term:
            return
        items = [t for t in self._data.get(kind) or [] if t != term]
        items.insert(0, term)
        self._data[kind] = items[:HISTORY_MAX]
        self._save()
        # 实现：去重 → 插到最前 → 截断 → 落盘。
        # 解析：add 即写盘（面板进程可能被随时关掉，不落盘的搜索词会丢）。
        # 功能：搜索历史的核心动作。

    def _save(self) -> None:
        try:
            self.path.parent.mkdir(parents=True, exist_ok=True)
            self.path.write_text(
                json.dumps(self._data, ensure_ascii=False, indent=1), encoding="utf-8")
        except OSError:
            pass  # 写不进（磁盘/权限）只损失历史词，不影响任何导出功能——静默降级
        # 实现：原子性要求不高（缓存性质），失败静默。
        # 功能：历史持久化。
