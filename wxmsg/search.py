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

from .session import Session, _match_degree, combined_name, ranked_sessions

# 实现：import json/Path + session 的评分器（口径唯一，不另写一套）。
# 功能：本模块全部依赖。

HISTORY_MAX = 50
# 实现：每类搜索历史的条数上限。
# 解析：datalist 里塞几百条会让联想框又长又乱；50 条是"最近用过的"的合理窗口。
# 功能：SearchHistory 的容量闸。


def build_member_index(groups_export: dict) -> dict:
    """--group-members-all JSON → {sid: {name/error/群原始名称字段, "members": [...]}}。

    成员 dict 保留 Rust 原始字段（username/group_display/remark/nickname/alias/source）；
    群的 remark/nickname/alias/session_display 原始字段同样入索引——组合群名在 Python 端
    算（会话表显示列在部分微信库上为空，不能只用 name）。每群 error 字段（回退原因）透传。"""
    index: dict = {}
    for g in groups_export.get("groups", []):
        index[g.get("sid") or ""] = {
            "name": g.get("name") or "",
            "remark": g.get("remark") or "",
            "nickname": g.get("nickname") or "",
            "alias": g.get("alias") or "",
            "session_display": g.get("session_display") or "",
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
    """群搜索（排序：群在前、成员在后）：

    ① 群名命中 → {kind:"group"}（导该群全部消息）+ **展开该群全部成员条目**
       （用户拍板：不截断，每条形如「成员名（群名）」，只导该成员）；
    ② 群内成员名命中（备注/昵称/微信号/原始id）→ {kind:"member"}（跨全部群匹配）。

    成员条目去重：同一 (群,成员) 若既被展开又被名字命中，保留带得分的命中条目。"""
    q = (query or "").strip()
    if not q:
        return []
    group_sessions = [s for s in sessions if s.is_group]
    session_names = {s.sid: s.display_name() for s in sessions}
    # 组合群名：会话组合名优先（字段②口径），没有会话的群用索引原始字段算，最后兜底 name。
    def group_name_of(sid: str) -> str:
        if sid in session_names:
            return session_names[sid]
        entry = index.get(sid) or {}
        return combined_name(entry.get("remark"), entry.get("nickname"), entry.get("alias"),
                             fallback=entry.get("name") or sid)

    def member_entry(sid: str, m: dict, matched: tuple | None) -> dict:
        gname = group_name_of(sid)
        mdisplay = combined_name(m.get("remark"), m.get("nickname"), m.get("group_display"),
                                 m.get("alias"), fallback=m.get("username") or "")
        username = m.get("username") or ""
        entry = {
            "kind": "member",
            "sid": sid,
            "username": username,
            "group_name": gname,
            "member_display": mdisplay,
            "group_display": m.get("group_display") or "",
            "display": f"{mdisplay}（{gname}）" if gname else mdisplay,
            "source": m.get("source") or "",
            "key": f"{sid}|{username}",
        }
        if matched:
            entry.update({"degree": matched[0], "field": matched[2],
                          "score": matched[0] * 10 + matched[1]})
        else:
            entry.update({"degree": 0, "field": "", "score": 0})
        return entry
        # 实现：成员展示名 = 组合格式（备注 昵称 群内昵称 微信号，与字段③一致），
        #       条目 display 再套「（群名）」——用户拍板的「小舒克（qaq）」形态。
        # 功能：两类成员条目（展开/命中）的统一构造。

    # ① 群名命中（ranked_sessions 已按得分降序）
    group_hits = ranked_sessions(group_sessions, query)
    out: list[dict] = [{"kind": "group", **h} for h in group_hits]
    # ② 群名命中 → 展开该群全部成员（用户拍板：不截断；条目按索引序=发言者池的活跃序）
    seen_keys: set[str] = set()
    for h in group_hits:
        for m in (index.get(h["sid"]) or {}).get("members") or []:
            entry = member_entry(h["sid"], m, matched=None)
            if entry["key"] not in seen_keys:
                out.append(entry)
                seen_keys.add(entry["key"])
    # ③ 成员名字命中（跨全部群）：与展开条目按 key 去重（命中的带得分/字段信息），
    #    未被展开覆盖的（群名没命中但成员名命中）按得分降序排在后面。
    member_hits: list[dict] = []
    for sid, entry in index.items():
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
            member_hits.append(member_entry(sid, m, matched=best))
    member_hits.sort(key=lambda x: (-x["score"], x["display"]))
    for mh in member_hits:
        if mh["key"] not in seen_keys:
            out.append(mh)
            seen_keys.add(mh["key"])
    return out
    # 实现：三段产出——群条目、展开成员（索引序）、名字命中成员（得分序），按 key 去重。
    # 解析：展开段让「搜群名」直接能挑具体成员（用户拍板）；命中段让「搜成员名」跨群定位；
    #       群名在前是硬规则，展开成员紧跟其群条目，跨群命中的成员殿后。
    # 功能：面板第二行搜索的数据源。


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
