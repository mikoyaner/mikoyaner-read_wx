"""wxmsg/realtime.py 的单元测试——游标增量、去重、两种格式/组织、起点语义。"""
# 实现：模块 docstring 声明测试方式（替身导出器按 --begin 过滤模拟数据库增量）。
# 功能：锁定实时核心语义（绝不丢、绝不重、起点可控）。
import csv
import json
import sys
from datetime import datetime
from pathlib import Path

import pytest

from wxmsg import realtime
from wxmsg.cli import Target
from wxmsg.config import KEY_DATA_ROOT, load_config
from wxmsg.outfile import CSV_HEADERS

# 实现：import 被测模块与夹具构造器。
# 功能：取得被测对象。

# 替身数据库：锚在 2026-01-01 00:00:00（UTC+8）的 Unix 秒上，测试里的"启动时刻"用它做偏移
_BASE = 1767196800
_ROWS = [
    {"local_id": 1, "create_time": _BASE + 90, "sort_seq": 1, "local_type": 1, "content": "更早", "sender_username": "wxid_a"},
    {"local_id": 2, "create_time": _BASE + 100, "sort_seq": 1, "local_type": 1, "content": "第一秒A", "sender_username": "wxid_a"},
    {"local_id": 3, "create_time": _BASE + 100, "sort_seq": 2, "local_type": 1, "content": "第一秒B", "sender_username": "wxid_a"},
    {"local_id": 4, "create_time": _BASE + 200, "sort_seq": 1, "local_type": 1, "content": "第二秒", "sender_username": "wxid_a"},
    {"local_id": 5, "create_time": _BASE + 300, "sort_seq": 1, "local_type": 1, "content": "第三秒", "sender_username": "wxid_a"},
]
# 替身数据库 v2：多一条 BASE+400 的（轮次间换数据，模拟"新消息到达"）
_ROWS_WITH_400 = _ROWS + [
    {"local_id": 6, "create_time": _BASE + 400, "sort_seq": 1, "local_type": 1, "content": "新到的", "sender_username": "wxid_a"},
]

_SCRIPT = """\
import json, sys
args = sys.argv[1:]
begin = int(args[args.index("--begin") + 1])
rows = ROWS
if "--recent" in args:
    n = int(args[args.index("--recent") + 1])
    rows = rows[-n:]
print(json.dumps({"mode": "messages", "sid_found": True, "route": "md5",
                  "messages": [r for r in rows if r["create_time"] >= begin]}))
"""
# 实现：替身导出器按 --begin 闭区间过滤 + --recent 尾部截断（模拟 weflow-core 行为）。
# 解析：真实导出器的 begin 语义就是 ct>=begin，recent 是"最近 N 条原始消息"（筛选前）。
# 功能：可控增量的数据库替身。


def _make_prefix(tmp_path, rows):
    script = tmp_path / "fake_rt_exporter.py"
    script.write_text("import json, sys\nROWS = " + json.dumps(rows, ensure_ascii=False) + "\n" + _SCRIPT,
                      encoding="utf-8")
    return [sys.executable, str(script)]
    # 实现：替身（rows 由测试注入，可在轮次间换数据模拟新消息到达）。
    # 功能：可控行为的导出器替身。


def _cfg(tmp_path):
    (tmp_path / "data_root").mkdir(exist_ok=True)   # 同一 tmp_path 可能被调两次
    cfg = load_config(environ={KEY_DATA_ROOT: str(tmp_path / "data_root")}, dotenv={}, root=tmp_path)
    cfg.output_dir = tmp_path / "output"
    return cfg
    # 实现：Config 夹具（无 ASR——实时测试聚焦游标语义）。
    # 功能：落盘隔离。


def _target():
    return Target(sid="wxid_a", chat_type=1, display="张三")
    # 实现：单私聊目标（实时游标语义与会话类型无关，用最简形态）。
    # 功能：各测试共用的目标。


class TestRealtimeStart:
    def test_no_history_starts_at_now(self, tmp_path):
        cfg = _cfg(tmp_path)
        # 启动时刻 = 2026-01-01 00:02:30（历史里 90/100 都早于它，不该出现）
        rt = realtime.RealtimeExport(cfg, [_target()], fmt="csv", merged=True,
                                     include_history=False, now=datetime(2026, 1, 1, 0, 2, 30),
                                     argv_prefix=_make_prefix(tmp_path, _ROWS))
        assert rt.path_merged.exists() is False or rt.path_merged.stat().st_size == 0
        out = rt.poll_once(argv_prefix=_make_prefix(tmp_path, _ROWS))
        # --begin=150 → 替身返回 200/300 两条（90/100 被时间窗过滤）
        assert out["new_total"] == 2
        with open(rt.path_merged, encoding="utf-8-sig", newline="") as f:
            rows = list(csv.reader(f))
        assert rows[0] == CSV_HEADERS
        assert [r[3] for r in rows[1:]] == [str(_BASE + 200), str(_BASE + 300)]
        # 实现：不导历史 → 游标=启动时刻，历史行被起点拦住。
        # 功能：「仅从启动时刻开始」语义。

    def test_include_history_then_increment(self, tmp_path):
        cfg = _cfg(tmp_path)
        rt = realtime.RealtimeExport(cfg, [_target()], fmt="json", merged=True,
                                     include_history=True, now=datetime(2026, 1, 1, 0, 0, 0),
                                     argv_prefix=_make_prefix(tmp_path, _ROWS))
        # 初始：全部 5 条进文件（json 格式自动换成 jsonl）
        assert rt.fmt == "jsonl"
        assert rt.summary()["total"] == 5
        # 第一轮 poll（数据没变）：0 条新
        out = rt.poll_once(argv_prefix=_make_prefix(tmp_path, _ROWS))
        assert out["new_total"] == 0
        # 新消息到达（ts=400）后第二轮 poll：只收 1 条
        out = rt.poll_once(argv_prefix=_make_prefix(tmp_path, _ROWS_WITH_400))
        assert out["new_total"] == 1
        assert out["sessions"][0]["total"] == 6
        lines = rt.path_merged.read_text(encoding="utf-8").strip().splitlines()
        assert len(lines) == 6
        assert json.loads(lines[-1])["content"] == "新到的"
        # 实现：include_history → 历史进实时文件 → 游标推到最大键 → 后续只追加增量。
        # 解析：300 秒那条每轮都会被 --begin=300 重取，但键 == 游标被过滤（不重）；
        #       400 秒新行键更大 → 收（不丢）。
        # 功能：「先导历史再实时追加」语义 + 游标去重核心。

    def test_same_second_late_arrival(self, tmp_path):
        # 同秒后到的消息（seq 更大）：cursor=(100,1,2) 后，seq=2 的"第一秒B"必须能被收到
        rows_v1 = [_ROWS[0], _ROWS[1]]                       # 只有 seq=1 那条
        rows_v2 = _ROWS[:3]                                  # seq=2 到达
        cfg = _cfg(tmp_path)
        rt = realtime.RealtimeExport(cfg, [_target()], fmt="csv", merged=True,
                                     include_history=True, now=datetime(2026, 1, 1),
                                     argv_prefix=_make_prefix(tmp_path, rows_v1))
        assert rt.summary()["total"] == 2                    # 90 + 100(seq1)
        out = rt.poll_once(argv_prefix=_make_prefix(tmp_path, rows_v2))
        assert out["new_total"] == 1                         # 只有"第一秒B"是新的
        lines = rt.path_merged.read_text(encoding="utf-8-sig").strip().splitlines()
        assert "第一秒B" in lines[-1]
        # 实现：begin=100 重取同秒 → seq=1 的行被键过滤，seq=2 的行键更大被追加。
        # 解析：这是"同秒消息分批落库"的真实场景——只按时间戳去重会丢这条。
        # 功能：同秒增量锚点（游标三元组设计存在的理由）。

    def test_split_files(self, tmp_path):
        cfg = _cfg(tmp_path)
        t1 = Target(sid="wxid_a", chat_type=1, display="张三")
        t2 = Target(sid="g@chatroom", chat_type=0, display="前端群")
        rt = realtime.RealtimeExport(cfg, [t1, t2], fmt="json", merged=False,
                                     include_history=True, now=datetime(2026, 1, 1),
                                     argv_prefix=_make_prefix(tmp_path, _ROWS))
        files = rt.summary()["files"]
        assert len(files) == 2
        assert files[0]["path"].endswith("实时_私聊_张三_20260101_000000.jsonl")
        assert files[1]["path"].endswith("实时_群聊_前端群_20260101_000000.jsonl")
        assert all(Path(f["path"]).exists() for f in files)
        # 实现：json → jsonl + 分文件命名（"实时_"前缀）。
        # 功能：分文件组织口径。

    def test_recent_history_no_explosion_with_member_filter(self, tmp_path):
        """回归：include_history + history_recent + 成员筛选，筛选后 0 条时游标必须锚在
        原始窗口末端——否则下一轮会把整段历史全部拉回来（真机踩到 recent=10 → 1799 条）。"""
        cfg = _cfg(tmp_path)
        rows = [{"local_id": i, "create_time": 100 + i, "sort_seq": i, "local_type": 1,
                 "content": f"msg{i}", "sender_username": "wxid_a"} for i in range(1, 13)]
        # 12 条群消息全部来自 wxid_a；目标成员 wxid_me 最近 10 条里一条都没有
        rt = realtime.RealtimeExport(cfg, [Target(sid="g@chatroom", chat_type=0, display="群",
                                                  selected_members={"wxid_me"})],
                                     fmt="csv", merged=True, include_history=True,
                                     history_recent=10, now=datetime(2026, 1, 1),
                                     argv_prefix=_make_prefix(tmp_path, rows))
        assert rt.summary()["total"] == 0                 # 窗口内没有目标成员的消息
        out = rt.poll_once(argv_prefix=_make_prefix(tmp_path, rows))
        assert out["new_total"] == 0                      # 不回爆全历史
        rows_v2 = rows + [{"local_id": 20, "create_time": 115, "sort_seq": 20,
                           "local_type": 1, "content": "新到的", "sender_username": "wxid_me"}]
        out = rt.poll_once(argv_prefix=_make_prefix(tmp_path, rows_v2))
        assert out["new_total"] == 1                      # 新消息照常增量
        assert "新到的" in rt.path_merged.read_text(encoding="utf-8-sig")
        # 实现：游标 = 原始窗口末端键（筛选前），poll 的 begin=该键的秒 → 同窗重取，
        # 行键 > 游标才收录——被筛掉的行永远不会再被拉回。
        # 功能：成员筛选 + 最近 N 条 的语义正确性。

    def test_bad_inputs_rejected(self, tmp_path):
        with pytest.raises(ValueError):
            realtime.RealtimeExport(_cfg(tmp_path), [], fmt="csv")
        with pytest.raises(ValueError):
            realtime.RealtimeExport(_cfg(tmp_path), [_target()], fmt="xlsx")
        # 实现：空目标与格式白名单。
        # 功能：入参防线。
