"""wxmsg/session.py 的单元测试——名称回退链、JSON 装配、发送者映射、消息排序。"""
# 实现：模块 docstring 声明测试范围（全部纯函数）。
# 功能：锁定用户拍板的名称回退口径与排序口径。
from wxmsg.session import (
    Member, Session, build_sender_map, members_from_export, ranked_sessions,
    sessions_from_export, sort_messages,
)
# 实现：import 被测函数与数据类。
# 功能：取得被测对象。


class TestSessionDisplayName:
    def test_chain(self):
        s = Session(sid="wxid_x", is_group=False, session_display="会话名",
                    remark="张三", nickname="三哥", alias="zhangsan_wx", sort_ts=0)
        assert s.display_name() == "张三"          # 备注优先
        s.remark = "  "                            # 纯空白备注不算有效值
        assert s.display_name() == "三哥"          # 退到昵称
        s.nickname = ""
        assert s.display_name() == "zhangsan_wx"   # 退到微信号
        s.alias = ""
        assert s.display_name() == "wxid_x"        # 全空兜底内部 id
        # 实现：回退链 = 备注 > 昵称 > 微信号 > 内部 id（用户拍板，非 v1 best() 顺序）。
        # 解析：纯空白按空处理（_first_not_blank 里 strip 判定）；
        #       session_display 刻意不在链里——字段②规格只有备注/微信号/昵称三个来源。
        # 功能：导出字段②的口径锚点。

    def test_group_display_falls_to_nickname(self):
        g = Session(sid="123@chatroom", is_group=True, session_display="前端交流群",
                    remark="", nickname="前端交流群", alias="", sort_ts=9)
        assert g.display_name() == "前端交流群"
        # 实现：群聊场景——v1 合并口径下 nickname 通常承载群名。
        # 功能：验证「群聊名」字段在有群名时的取值。


class TestMemberDisplayName:
    def test_chain_excludes_group_display(self):
        m = Member(username="wxid_m", group_display="群里叫小明",
                   remark="", nickname="小明本名", alias="")
        assert m.display_name() == "小明本名"
        m.nickname = ""
        assert m.display_name() == "wxid_m"        # 群内昵称不参与回退，直接跳到 id
        m.alias = "ming_wx"
        assert m.display_name() == "ming_wx"       # 微信号在群内昵称之前
        # 实现：字段③链 = 备注 > 昵称 > 微信号 > 内部 id；group_display 只留在数据层。
        # 解析：用户定义的字段③只有「备注、微信号、昵称」三个来源——群内昵称不进导出列，
        #       这是规格就规格的口径；面板如需辅助辨认可用原始 group_display。
        # 功能：导出字段③的口径锚点。


class TestFromExport:
    def test_sessions_tolerates_missing_and_null(self):
        export = {"sessions": [
            {"sid": "a@chatroom", "is_group": True, "session_display": None, "sort_ts": "123"},
            {"sid": "wxid_b"},
        ]}
        out = sessions_from_export(export)
        assert out[0].is_group is True
        assert out[0].session_display == ""        # null → 空串
        assert out[0].sort_ts == 123               # 字符串数字也收
        assert out[1].is_group is False
        assert out[1].remark == ""
        # 实现：缺失键与 None 一律宽松兜空。
        # 解析：模型层永不因数据形态崩溃（fixture/未来版本可能给 null）。
        # 功能：JSON → Session 装配的健壮性锚点。

    def test_members_tolerates_missing(self):
        out = members_from_export({"members": [{"username": "wxid_m"}]})
        assert out[0].group_display == "" and out[0].alias == ""
        # 功能：成员装配健壮性。


class TestBuildSenderMap:
    def test_members_first_and_private_only(self):
        sessions = [
            Session(sid="g@chatroom", is_group=True, session_display="群", remark="", nickname="群", alias="", sort_ts=0),
            Session(sid="wxid_f", is_group=False, session_display="", remark="老王", nickname="", alias="", sort_ts=0),
        ]
        members = [Member(username="wxid_m", group_display="小明", remark="", nickname="小明", alias="")]
        m = build_sender_map(sessions, members)
        assert m["wxid_m"] == "小明"               # 成员进表
        assert m["wxid_f"] == "老王"               # 私聊方进表
        assert "g@chatroom" not in m               # 群 id 不是"人"，不进发送者映射
        # 实现：members 全量收录，sessions 只收私聊方；先 members 后 sessions（先到先得）。
        # 功能：发送者渲染的数据源口径。

    def test_no_overwrite(self):
        members = [Member(username="wxid_x", group_display="", remark="成员表版本", nickname="", alias="")]
        sessions = [Session(sid="wxid_x", is_group=False, session_display="", remark="会话表版本", nickname="", alias="", sort_ts=0)]
        m = build_sender_map(sessions, members)
        assert m["wxid_x"] == "成员表版本"
        # 实现：同名键先到先得（members 在前）。
        # 解析：群成员表的名称通常比会话表更准（会话表只有显示名）。
        # 功能：验证先到先得语义。


class TestRankedSessions:
    def _sess(self, **kw):
        base = dict(sid="wxid_x", is_group=False, session_display="", remark="",
                    nickname="", alias="", sort_ts=0)
        base.update(kw)
        return Session(**base)
        # 实现：Session 快捷构造。
        # 功能：匹配测试夹具。

    def test_field_priority(self):
        sessions = [self._sess(sid="wxid_a", remark="张三", nickname="三哥", alias="zs")]
        out = ranked_sessions(sessions, "张三")
        assert out[0]["field"] == "备注" and out[0]["degree"] == 3 and out[0]["score"] == 35
        out = ranked_sessions(sessions, "三哥")
        assert out[0]["field"] == "昵称" and out[0]["score"] == 33
        out = ranked_sessions(sessions, "zs")
        assert out[0]["field"] == "微信号" and out[0]["score"] == 34
        # 实现：字段权重 备注5 > 微信号4 > 昵称3（v1 ranked_matches 同款）。
        # 解析：score = degree*10 + rank（全等命中 → 3*10+权重）。
        # 功能：字段优先级锚点。

    def test_degree_tiers(self):
        sessions = [self._sess(sid="wxid_a", nickname="王大锤")]
        out = ranked_sessions(sessions, "王大锤")
        assert out[0]["degree"] == 3                            # 完全相等
        out = ranked_sessions(sessions, "王大")
        assert out[0]["degree"] == 2                            # 前缀
        out = ranked_sessions(sessions, "大锤")
        assert out[0]["degree"] == 1                            # 包含
        out = ranked_sessions(sessions, "李逵")
        assert out == []                                        # 不匹配 → 空候选
        # 实现：三档程度（相等/前缀/包含）。
        # 功能：程度分档锚点。

    def test_unique_flag_semantics(self):
        # 同分场景：两条都是「备注命中 + 前缀」→ score 相同 → 按展示名字典序保证可复现
        two = [self._sess(sid="a", remark="王哥"), self._sess(sid="b", remark="王二")]
        out = ranked_sessions(two, "王")
        assert len(out) == 2 and out[0]["score"] == out[1]["score"] == 25
        assert out[0]["display"] <= out[1]["display"]           # 同分按展示名字典序
        assert out[0]["display"] == "王二" and out[1]["display"] == "王哥"
        # 实现：同分字典序（HashMap 迭代序不确定，排序保证可复现——v1 同款）。
        # 功能：歧义候选排序。

    def test_empty_query(self):
        assert ranked_sessions([self._sess()], "") == []
        assert ranked_sessions([self._sess()], "   ") == []
        # 实现：空查询返回空（避免空串 contains 命中所有人——v1 同款防御）。
        # 功能：空查询分支。

    def test_group_session_display_matches(self):
        g = self._sess(sid="g@chatroom", is_group=True, nickname="前端交流群")
        out = ranked_sessions([g], "前端")
        assert out[0]["is_group"] is True and out[0]["field"] == "昵称"
        # 实现：群名（昵称槽位）参与匹配。
        # 功能：群聊按群名搜索。
class TestSortMessages:
    def test_order_by_time_then_seq_then_id(self):
        rows = [
            {"create_time": 100, "sort_seq": 5, "local_id": 1},
            {"create_time": 100, "sort_seq": 2, "local_id": 9},
            {"create_time": 99, "sort_seq": 999999, "local_id": 1},
            {"create_time": 100, "sort_seq": 2, "local_id": 3},
            {},                                       # 全缺 → (0,0,0) 排最前
        ]
        original = list(rows)
        out = sort_messages(rows)
        assert [ (r.get("create_time"), r.get("sort_seq"), r.get("local_id"))
                 for r in out ] == [
            (None, None, None),
            (99, 999999, 1),
            (100, 2, 3),
            (100, 2, 9),
            (100, 5, 1),
        ]
        assert rows == original                    # 入参不被原地修改
        # 实现：三元组排序键（时间 → 同秒 sort_seq → local_id），sorted 返回新列表。
        # 解析：sort_seq 是同秒内权威顺序；local_id 作最终 tiebreaker 保证全序可复现；
        #       缺字段按 0 参与排序（与导出器取值口径一致）。
        # 功能：导出文件内消息行顺序的口径锚点。
