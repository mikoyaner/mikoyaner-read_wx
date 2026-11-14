"""会话/成员数据模型 + 名称回退 + 消息排序。

名称回退口径（用户拍板，与 v1 的 best() 顺序不同）：备注 > 昵称 > 微信号 > 内部 id。
导出器交来的是原始名称字段（remark/nickname/alias），回退只在本模块做——口径唯一。
"""
# 实现：模块 docstring 声明职责与名称回退口径的来源。
# 功能：cli/realtime/panel 共用的会话/成员模型与展示名解析。

from __future__ import annotations

from dataclasses import dataclass

# 实现：import dataclass 定义数据模型；零其它依赖（排序/回退全是纯函数）。
# 功能：本模块全部依赖。


def _first_not_blank(*values: str) -> str:
    """返回第一个 strip 后非空的值；全空返回空串。"""
    for v in values:
        if v and v.strip():
            return v.strip()
    return ""
    # 实现：逐个判 strip 非空，早返回。
    # 解析：回退链的实现原语——「非空」一律按 strip 后计，防纯空白备注顶掉有效昵称。
    # 功能：display_name 的共用底座。


@dataclass
class Session:
    """一个会话（私聊联系人或群聊），字段来自导出器 --list-sessions 的原始输出。"""

    sid: str              # 会话 id：wxid_xxx 或 xxx@chatroom
    is_group: bool        # True=群聊（@chatroom）
    session_display: str  # 会话表显示名（群名通常只在这里有）
    remark: str           # 备注（contact.db；群聊也可能有）
    nickname: str         # 昵称（v1 合并口径：群聊时通常是群名）
    alias: str            # 微信号
    sort_ts: int          # 活跃时间戳（秒；0=未知）

    def display_name(self) -> str:
        """字段②的取值：备注 > 昵称 > 微信号 > 内部 id（用户拍板的回退链）。"""
        return _first_not_blank(self.remark, self.nickname, self.alias) or self.sid
        # 实现：回退链走 _first_not_blank，全空兜底 sid。
        # 解析：昵称在 v1 合并口径下对群聊通常就是群名，所以这条链覆盖「群聊名/私聊联系人名」；
        #       刻意不掺 session_display——用户定义的字段②只有备注/微信号/昵称三个来源。
        # 功能：导出文件字段②与面板列表显示名的唯一实现。
    # 实现：dataclass 字段逐一标注来源。
    # 功能：会话的统一载体。


@dataclass
class Member:
    """一个群成员，字段来自导出器 --group-members 的原始输出。"""

    username: str        # 成员 wxid（稳定键）
    group_display: str   # 群内昵称（成员表/成员列表列），可空
    remark: str          # 备注
    nickname: str        # 昵称
    alias: str           # 微信号

    def display_name(self) -> str:
        """字段③的取值：备注 > 昵称 > 微信号 > 内部 id（与字段②同链，不含群内昵称）。

        用户定义的字段③只有「备注、微信号、昵称」三个来源；群内昵称（group_display）
        不在规格里，故不参与回退（只随原始数据保留，面板可用于辅助辨认）。"""
        return _first_not_blank(self.remark, self.nickname, self.alias) or self.username
        # 实现：与 Session.display_name 同链。
        # 解析：字段③是导出规格的一部分，必须严格按用户口径实现；group_display 留在数据里
        #       供面板辅助显示，但绝不进导出列——规格就是规格。
        # 功能：群成员展示名的唯一实现。
    # 实现：dataclass 字段标注来源。
    # 功能：群成员的统一载体。


def sessions_from_export(export: dict) -> list[Session]:
    """把导出器 --list-sessions 的 JSON 转成 Session 列表（保持 Rust 端的活跃时间降序）。"""
    out: list[Session] = []
    for row in export.get("sessions", []):
        out.append(Session(
            sid=row.get("sid", ""),
            is_group=bool(row.get("is_group")),
            session_display=row.get("session_display", "") or "",
            remark=row.get("remark", "") or "",
            nickname=row.get("nickname", "") or "",
            alias=row.get("alias", "") or "",
            sort_ts=int(row.get("sort_ts") or 0),
        ))
    return out
    # 实现：逐行取六字段，缺失一律空串/0（None 与缺键同罪）。
    # 解析：get(key, "") or "" 双保险——Rust 端不会给 null（都是 String），但手工构造的
    #       fixture/未来版本可能给 null，宽松处理让模型层永不因数据形态崩溃。
    # 功能：面板会话列表的数据装配。


def members_from_export(export: dict) -> list[Member]:
    """把导出器 --group-members 的 JSON 转成 Member 列表。"""
    out: list[Member] = []
    for row in export.get("members", []):
        out.append(Member(
            username=row.get("username", ""),
            group_display=row.get("group_display", "") or "",
            remark=row.get("remark", "") or "",
            nickname=row.get("nickname", "") or "",
            alias=row.get("alias", "") or "",
        ))
    return out
    # 实现：逐行取五字段，缺失宽松兜空。
    # 功能：面板群成员勾选列表的数据装配。


def build_sender_map(sessions: list[Session], members: list[Member]) -> dict[str, str]:
    """构建 username → 展示名 的映射，供消息渲染解析群内发送者（字段③）。

    合并规则：members 与 sessions 都提供，同名键以「先到先得」为准（调用方控制顺序：
    群成员表更精确，先放 members；sessions 的联系人信息补漏）。展示名走同一回退链。"""
    out: dict[str, str] = {}
    for m in members:
        if m.username and m.username not in out:
            out[m.username] = m.display_name()
    for s in sessions:
        if s.sid and not s.is_group and s.sid not in out:
            out[s.sid] = s.display_name()
    return out
    # 实现：members 全量收录；sessions 只收私聊方（群 id 不是"人"，不该进发送者映射）。
    # 解析：发送者渲染只需要 username→名字；群聊发送者主要来自群成员表（字段③规格），
    #       私聊方名字放进来是为面板/日志场景兜底。先到先得保证调用方可用顺序表达优先级。
    # 功能：render.py 解析群内发送者名的唯一数据源。


def sort_messages(rows: list[dict]) -> list[dict]:
    """消息按时间正序排序：主键 create_time，同秒按 sort_seq，再按 local_id（全稳定）。

    Rust 端单表内已按 sort_seq 升序，这里主要处理 recent 路径的合并序与未来跨表场景；
    缺失字段按 0 参与排序（与导出器的取值口径一致）。返回新列表，不动入参。"""
    def key(r: dict) -> tuple[int, int, int]:
        return (
            int(r.get("create_time") or 0),
            int(r.get("sort_seq") or 0),
            int(r.get("local_id") or 0),
        )
    # 实现：三元组排序键（时间 → 同秒序号 → 消息号）。
    # 解析：sort_seq 是微信内部单调序号，同秒内它是权威顺序；local_id 作最后 tiebreaker
    #       保证排序全序稳定（同键消息顺序可复现）。
    # 功能：导出文件内消息行的最终顺序依据。
    return sorted(rows, key=key)
    # 实现：sorted 返回新列表（不原地改入参，调用方可安全复用原列表）。
    # 功能：outfile 落盘前的最后一道顺序保障。
