"""wxmsg/search.py 的单元测试——成员索引、私聊/群搜索排序、搜索历史存取。"""
# 实现：模块 docstring 声明测试范围（纯函数 + 临时文件）。
# 功能：锁定搜索数据层的口径（群在前成员在后、回退链、历史去重置顶）。
import json

import pytest

from wxmsg.search import SearchHistory, build_member_index, search_groups, search_privates
from wxmsg.session import Session

# 实现：import 被测函数。
# 功能：取得被测对象。

_INDEX = build_member_index({"groups": [
    {"sid": "g1@chatroom", "name": "前端交流群", "error": "member_table_empty_fallback_sender_pool",
     "members": [
         {"username": "wxid_zs", "group_display": None, "remark": "张三", "nickname": "三哥", "alias": "zs_wx", "source": "sender_pool"},
         {"username": "wxid_li", "group_display": "小李", "remark": "", "nickname": "", "alias": "", "source": "sender_pool"},
     ]},
    {"sid": "g2@chatroom", "name": "家人群", "error": None,
     "members": [
         {"username": "wxid_ma", "group_display": "老妈", "remark": "", "nickname": "母亲", "alias": "", "source": "roster"},
     ]},
]})
_SESSIONS = [
    Session(sid="wxid_zs_dm", is_group=False, session_display="", remark="张三", nickname="三哥", alias="zs_wx", sort_ts=9),
    Session(sid="g1@chatroom", is_group=True, session_display="", remark="", nickname="前端交流群", alias="", sort_ts=8),
    Session(sid="g2@chatroom", is_group=True, session_display="", remark="", nickname="家人群", alias="", sort_ts=7),
    Session(sid="gh_news", is_group=False, session_display="", remark="", nickname="新闻号", alias="", sort_ts=1),
]
# 实现：索引/会话夹具——覆盖 roster 与 sender_pool 两种来源、私聊/群/公众号三种会话。
# 功能：搜索测试共用夹具。


class TestBuildMemberIndex:
    def test_shape_and_error_passthrough(self):
        assert set(_INDEX) == {"g1@chatroom", "g2@chatroom"}
        assert _INDEX["g1@chatroom"]["name"] == "前端交流群"
        assert _INDEX["g1@chatroom"]["error"] == "member_table_empty_fallback_sender_pool"
        assert _INDEX["g2@chatroom"]["error"] is None
        assert _INDEX["g1@chatroom"]["members"][0]["source"] == "sender_pool"
        # 实现：按 sid 建索引，error 字段透传（诊断不再被吞）。
        # 功能：索引结构锚点。


class TestSearchPrivates:
    def test_filters_groups_and_official(self):
        out = search_privates(_SESSIONS, "张三")
        assert [x["sid"] for x in out] == ["wxid_zs_dm"]
        assert out[0]["kind"] == "priv"
        out = search_privates(_SESSIONS, "群")
        assert out == []          # 群名命中被过滤（群走第二行搜索）
        out = search_privates(_SESSIONS, "新闻")
        assert out == []          # 公告号不进私聊搜索
        # 实现：私聊搜索 = ranked_sessions ∩ 非群非公众号。
        # 功能：第一行搜索的口径。

    def test_empty_query(self):
        assert search_privates(_SESSIONS, "") == []
        # 功能：空查询。


class TestSearchGroups:
    def test_group_hit_before_member_hits(self):
        # 夹具里没有任何成员字段含"前端交流群" → 只有群名命中，结果恰为 1 条群条目
        out = search_groups(_INDEX, _SESSIONS, "前端交流群")
        assert [x["kind"] for x in out] == ["group"]
        assert out[0]["sid"] == "g1@chatroom" and out[0]["display"] == "前端交流群"
        # 实现：群名命中排最前（本例中无成员命中）。
        # 功能：排序口径（用户拍板：按XX群聊在前面）。

    def test_group_and_member_mixed_order(self):
        # 一个查询同时命中群与成员：群在前是**硬规则**——即使成员得分更高（此处成员全等 35 分，
        # 群只是前缀命中 23 分），群条目也排在成员条目之前（用户拍板的展示顺序）
        idx = build_member_index({"groups": [
            {"sid": "g1@chatroom", "name": "张三群", "error": None,
             "members": [{"username": "wxid_zs", "group_display": None, "remark": "张三",
                          "nickname": "", "alias": "", "source": "roster"}]},
        ]})
        sess = [Session(sid="g1@chatroom", is_group=True, session_display="",
                        remark="", nickname="张三群", alias="", sort_ts=0)]
        out = search_groups(idx, sess, "张三")
        assert [x["kind"] for x in out] == ["group", "member"]   # 群在前、成员在后（硬规则）
        assert out[0]["score"] < out[1]["score"]                 # 顺序与得分无关
        # 实现：两类条目分段排序（群段、成员段各自按得分降序），段序固定群在前。
        # 功能：混合命中排序。

    def test_member_hit(self):
        out = search_groups(_INDEX, _SESSIONS, "张三")
        assert all(x["kind"] == "member" for x in out)
        assert out[0]["sid"] == "g1@chatroom" and out[0]["username"] == "wxid_zs"
        assert out[0]["field"] == "备注" and out[0]["score"] == 35
        assert out[0]["display"] == "前端交流群·张三"          # 群·成员 展示形态
        assert out[0]["source"] == "sender_pool"
        # 实现：成员按 备注>微信号>原始id>昵称 评分；展示名 = 群名·成员回退名。
        # 功能：成员命中条目锚点。

    def test_member_fallback_chain(self):
        # 只有原始 id 可搜（备注/昵称/微信号全空）→ 按「原始id」字段命中，展示名回退到 id
        out = search_groups(_INDEX, _SESSIONS, "wxid_li")
        assert out[0]["username"] == "wxid_li" and out[0]["field"] == "原始id"
        assert out[0]["degree"] == 3 and out[0]["member_display"] == "wxid_li"
        # 实现：搜索字段与导出字段③一致（群内昵称 group_display 不参与匹配）。
        # 功能：字段口径一致性。

    def test_nickname_hit(self):
        out = search_groups(_INDEX, _SESSIONS, "母亲")
        assert out[0]["username"] == "wxid_ma" and out[0]["field"] == "昵称"
        assert out[0]["group_name"] == "家人群"
        # 功能：roster 来源成员的昵称命中。

    def test_empty_query(self):
        assert search_groups(_INDEX, _SESSIONS, "") == []
        # 功能：空查询。


class TestSearchHistory:
    def test_add_dedupe_recent_first(self, tmp_path):
        h = SearchHistory(tmp_path / "history.json")
        h.add("priv", "张三")
        h.add("priv", "李四")
        h.add("priv", "张三")            # 重复 → 置顶去重
        assert h.terms("priv") == ["张三", "李四"]
        # 实现：去重 + 最近在前。
        # 功能：历史词核心语义。

    def test_kinds_independent_and_persist(self, tmp_path):
        h = SearchHistory(tmp_path / "history.json")
        h.add("priv", "张三")
        h.add("group", "前端群")
        h2 = SearchHistory(tmp_path / "history.json")   # 重新装载（模拟重启）
        assert h2.terms("priv") == ["张三"]
        assert h2.terms("group") == ["前端群"]
        # 实现：两类独立 + 写盘 + 重启可读。
        # 功能：持久化锚点。

    def test_max_cap(self, tmp_path):
        h = SearchHistory(tmp_path / "history.json")
        for i in range(60):
            h.add("group", f"词{i}")
        assert len(h.terms("group")) == 50
        assert h.terms("group")[0] == "词59"             # 最近在前
        assert "词0" not in h.terms("group")             # 最老的被挤出
        # 实现：HISTORY_MAX=50 容量闸。
        # 功能：容量上限。

    def test_blank_and_bad_kind(self, tmp_path):
        h = SearchHistory(tmp_path / "history.json")
        h.add("priv", "   ")                             # 空白词忽略
        assert h.terms("priv") == []
        with pytest.raises(ValueError):
            h.terms("bogus")
        with pytest.raises(ValueError):
            h.add("bogus", "x")
        # 实现：空白词不记；类别白名单。
        # 功能：防御分支。

    def test_corrupt_file_starts_empty(self, tmp_path):
        (tmp_path / "history.json").write_text("{坏掉的", encoding="utf-8")
        h = SearchHistory(tmp_path / "history.json")
        assert h.terms("priv") == [] and h.terms("group") == []
        # 实现：坏缓存按空历史起步（绝不让缓存问题崩面板）。
        # 功能：损坏防御。

    def test_file_roundtrip_content(self, tmp_path):
        h = SearchHistory(tmp_path / "history.json")
        h.add("priv", "张三")
        data = json.loads((tmp_path / "history.json").read_text(encoding="utf-8"))
        assert data == {"priv": ["张三"], "group": []}
        # 实现：文件结构 {priv:[], group:[]}。
        # 功能：落盘格式锚点。
