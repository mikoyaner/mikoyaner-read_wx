"""wxmsg/session.py 的单元测试——名称回退链、JSON 装配、发送者映射、消息排序。"""
# 实现：模块 docstring 声明测试范围（全部纯函数）。
# 功能：锁定用户拍板的名称回退口径与排序口径。
from wxmsg.session import (
    Member, Session, combined_name, ranked_sessions, sessions_from_export, sort_messages,
)
# 实现：import 被测函数与数据类。
# 功能：取得被测对象。


class TestSessionDisplayName:
    def test_combined_format(self):
        s = Session(sid="wxid_x", is_group=False, session_display="会话名",
                    remark="张三", nickname="三哥", alias="zhangsan_wx", sort_ts=0)
        assert s.display_name() == "张三 三哥 zhangsan_wx"
        s.remark = "  "                            # 纯空白备注跳过
        assert s.display_name() == "三哥 zhangsan_wx"
        s.nickname = ""
        assert s.display_name() == "zhangsan_wx"   # 只剩微信号
        s.alias = ""
        assert s.display_name() == "wxid_x"        # 全空兜底内部 id
        # 实现：字段② = 组合格式「备注 昵称 微信号」，空组件跳过，全空兜底内部 id。
        # 解析：组合格式取代旧的单名回退链（用户拍板：三个名字都在才好认人）。
        # 功能：导出字段②的口径锚点。

    def test_group_display_falls_to_nickname(self):
        g = Session(sid="123@chatroom", is_group=True, session_display="前端交流群",
                    remark="", nickname="前端交流群", alias="", sort_ts=9)
        assert g.display_name() == "前端交流群"
        # 实现：群聊场景——nickname 通常承载群名。
        # 功能：验证「群聊名」字段在有群名时的取值。


class TestMemberDisplayName:
    def test_combined_includes_group_display(self):
        m = Member(username="wxid_m", group_display="群里叫小明",
                   remark="", nickname="小明本名", alias="")
        assert m.display_name() == "小明本名 群里叫小明"   # 昵称 + 群内昵称
        m.nickname = ""
        assert m.display_name() == "群里叫小明"            # 只有群内昵称的人也能认出
        m.group_display = ""
        assert m.display_name() == "wxid_m"                # 全空兜底原始 id
        m.alias = "ming_wx"
        m.group_display = "群昵"
        assert m.display_name() == "群昵 ming_wx"          # 空的备注/昵称跳过
        # 实现：字段③ = 组合格式「备注 昵称 群内昵称 微信号」，空组件跳过，全空兜底 id。
        # 解析：群内昵称是用户拍板加进来的——很多人在群里只有群内昵称，不加会认不出。
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

class TestCombinedName:
    def test_skip_blank_and_fallback(self):
        assert combined_name("张三", "", "  ", "zs") == "张三 zs"
        assert combined_name("", "", "", fallback="wxid_x") == "wxid_x"
        assert combined_name() == ""
        assert combined_name("王 二", "昵称") == "王 二 昵称"   # 名字本身含空格：拼接即可
        # 实现：空格连接、空组件跳过、全空回落 fallback。
        # 解析：这是字段②③的统一取值口径（用户拍板的组合格式）。
        # 功能：组合名工具锚点。


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
