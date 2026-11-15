"""wxmsg/cli.py 的端到端测试——假导出器（argv_prefix 注入）驱动完整流水线到落盘文件。"""
# 实现：模块 docstring 声明测试方式（替身导出器 + 真 outfile 落盘，不依赖 Rust exe/真微信）。
# 功能：锁定历史导出主流程（筛选/渲染/字段组装/两种组织/时间解析）的口径。
import csv
import json
import sys
import textwrap
from datetime import datetime

import pytest

from wxmsg import cli
from wxmsg.cli import Target, parse_time_boundary
from wxmsg.config import KEY_DATA_ROOT, load_config

# 实现：import 被测模块与夹具构造器。
# 功能：取得被测对象。

_MESSAGES = [
    {"local_id": 1, "create_time": 1700000100, "sort_seq": 1, "local_type": 1,
     "content": "群里的文本", "sender_username": "wxid_a"},
    {"local_id": 2, "create_time": 1700000100, "sort_seq": 2, "local_type": 3,
     "content": "", "sender_username": "wxid_b"},                       # 图片 → [图片]
    {"local_id": 3, "create_time": 1700000090, "sort_seq": 3, "local_type": 1,
     "content": "更早的文本", "sender_username": "wxid_me"},             # 时间更早，排序后应在最前
    {"local_id": 4, "create_time": 1700000200, "sort_seq": 4, "local_type": 1,
     "content": "被筛掉的", "sender_username": "wxid_c"},                # 不在勾选成员里
]
_MEMBERS = [
    {"username": "wxid_a", "group_display": "", "remark": "张三", "nickname": "三哥", "alias": "zs_wx", "source": "sender_pool"},
    {"username": "wxid_b", "group_display": "小明", "remark": "", "nickname": "", "alias": "", "source": "sender_pool"},
    {"username": "wxid_me", "group_display": "", "remark": "", "nickname": "我自己的号", "alias": "", "source": "sender_pool"},
]
# 实现：消息/成员夹具——覆盖同秒乱序（sort_seq 决定先后）、非文本占位、成员筛选、发送者映射。
# 功能：替身导出器的响应数据。

_FIXTURE = """\
import json, sys
args = sys.argv[1:]
def flag(name):
    return args[args.index(name) + 1] if name in args else None
if "--messages" in args:
    rows = ROWS
    print(json.dumps({
        "mode": "messages", "sid": flag("--sid"), "sid_found": True,
        "route": "md5", "count": len(rows),
        "messages": rows,
    }))
elif "--group-members-all" in args:
    print(json.dumps({"mode": "group_members_all", "groups_total": 1, "roster_ok": 0,
                      "pool_used": 1, "groups": [
        {"sid": "g@chatroom", "name": "", "count": 3,
         "error": "member_table_empty_fallback_sender_pool", "members": MEMBERS}]}))
"""
# 实现：替身导出器模板（纯字符串，无 f-string 转义）——messages/group_members 两种模式按旗标响应。
# 功能：多模式替身。


def _make_prefix(tmp_path):
    script = tmp_path / "fake_full_exporter.py"
    body = _FIXTURE  # 模板直接引用脚本头部定义的 ROWS/MEMBERS 变量，无需文本替换
    script.write_text("import json, sys\nROWS = " + json.dumps(_MESSAGES, ensure_ascii=False) +
                      "\nMEMBERS = " + json.dumps(_MEMBERS, ensure_ascii=False) + "\n" + body,
                      encoding="utf-8")
    return [sys.executable, str(script)]
    # 实现：把夹具数据以字面量形式写进脚本头部，模板直接引用这两个变量。
    # 功能：一次注入、多模式响应的替身导出器。


def _cfg(tmp_path):
    (tmp_path / "data_root").mkdir()
    cfg = load_config(environ={KEY_DATA_ROOT: str(tmp_path / "data_root")}, dotenv={}, root=tmp_path)
    cfg.output_dir = tmp_path / "output"     # 测试把导出指到 tmp，不碰真实 output/
    cfg.node_bin = sys.executable            # ASR 替身走当前解释器（生产是 node）
    return cfg
    # 实现：Config 夹具（output_dir/node_bin 覆盖到 tmp）。
    # 功能：落盘隔离 + ASR 替身可执行。


class TestParseTimeBoundary:
    def test_variants(self):
        assert parse_time_boundary("") == 0
        assert parse_time_boundary(None) == 0
        assert parse_time_boundary("2026-01-31") == __import__("datetime").datetime(2026, 1, 31).timestamp()
        assert parse_time_boundary("2026-01-31 12:30") == __import__("datetime").datetime(2026, 1, 31, 12, 30).timestamp()
        assert parse_time_boundary("2026-01-31", end_of_day=True) == \
            __import__("datetime").datetime(2026, 1, 31, 23, 59, 59).timestamp()
        with pytest.raises(ValueError):
            parse_time_boundary("yesterday")
        # 实现：三种写法 + end_of_day + 非法报错。
        # 解析：timestamp() 按本机时区——与字段⑤口径一致；非法输入必须报错而不是当 0。
        # 功能：时间边界解析锚点。


class TestRunExport:
    def test_group_merged_csv(self, tmp_path):
        cfg = _cfg(tmp_path)
        targets = [Target(sid="g@chatroom", chat_type=0, display="前端群",
                          selected_members={"wxid_a", "wxid_b", "wxid_me"})]
        out = cli.run_export(cfg, targets, fmt="csv", merged=True, now=datetime(2026, 1, 1, 0, 0, 0),
                             argv_prefix=_make_prefix(tmp_path))
        assert out["total"] == 3                          # wxid_c 被筛掉，其余 3 条都要
        f = out["files"][0]
        assert f["path"].endswith(".csv") and f["count"] == 3
        with open(f["path"], encoding="utf-8-sig", newline="") as fh:
            rows = list(csv.reader(fh))
        data = rows[1:]
        # 排序：1700000090（更早）在最前；同秒 100 的两条按 sort_seq 先后
        assert [r[3] for r in data] == ["1700000090", "1700000100", "1700000100"]
        # 字段③组合格式：备注 昵称 群内昵称 微信号（空组件跳过，全空→原始 id）
        senders = {r[5]: r[2] for r in data}
        assert senders["更早的文本"] == "我自己的号"              # 只有昵称
        assert senders["群里的文本"] == "张三 三哥 zs_wx"          # 备注+昵称+微信号
        assert senders["[图片]"] == "小明"                        # 只有群内昵称的人也能认出
        # 实现：合并 CSV + 成员筛选 + 排序 + 字段③组合格式（数据来自成员索引）的端到端。
        # 解析：wxid_b 只有群内昵称"小明"——组合格式把它带进来了（旧回退链下是裸 wxid）。
        # 功能：主流程口径锚点。

    def test_private_split_json(self, tmp_path):
        cfg = _cfg(tmp_path)
        targets = [Target(sid="wxid_a", chat_type=1, display="张三")]
        out = cli.run_export(cfg, targets, fmt="json", merged=False,
                             now=datetime(2026, 1, 1, 0, 0, 0),
                             argv_prefix=_make_prefix(tmp_path))
        assert out["files"][0]["path"].endswith(".json")
        data = json.loads(__import__("pathlib").Path(out["files"][0]["path"]).read_text(encoding="utf-8"))
        assert len(data) == 4                             # 私聊不筛成员，4 条全要
        assert all(r["chat_type"] == 1 for r in data)
        assert all(r["sender_name"] is None for r in data)  # 字段③私聊恒 null
        assert data[0]["session_name"] == "张三"
        # 实现：分文件 JSON + 私聊语义。
        # 功能：私聊口径与分文件组织。

    def test_warnings_on_member_failure(self, tmp_path):
        # 替身在 --group-members-all 时退出 1（模拟成员索引构建失败），--messages 正常
        script = tmp_path / "no_members.py"
        script.write_text(
            "import json, sys\n"
            "if '--group-members-all' in sys.argv[1:]:\n"
            "    sys.exit(1)\n"
            "print(json.dumps({'mode': 'messages', 'sid_found': True,"
            " 'messages': " + json.dumps(_MESSAGES, ensure_ascii=False) + "}))",
            encoding="utf-8")
        cfg = _cfg(tmp_path)
        out = cli.run_export(cfg, [Target(sid="g@chatroom", chat_type=0, display="群")],
                             fmt="csv", now=datetime(2026, 1, 1),
                             argv_prefix=[sys.executable, str(script)])
        assert out["total"] == 4                          # 导出不中断
        assert any("群成员索引构建失败" in w for w in out["warnings"])
        # 实现：索引构建失败 → 发送者退化原始 wxid + warning，不中断导出。
        # 功能：部分失败不连坐的口径。

    def test_voice_asr_end_to_end(self, tmp_path, monkeypatch):
        # 替身导出器：语音消息 + 落 silk 假文件（与 Rust 的 voice_{ts}_{lid}_{sid}.silk 命名一致）
        silk_name = "voice_1700000100_1_g_chatroom.silk"
        script = tmp_path / "voice_exporter.py"
        script.write_text(textwrap.dedent(f'''
            import json, sys, pathlib
            args = sys.argv[1:]
            if "--group-members-all" in args:
                print(json.dumps({{"mode": "group_members_all", "groups_total": 1,
                    "roster_ok": 0, "pool_used": 1, "groups": [
                    {{"sid": "g@chatroom", "name": "", "count": 1, "error": None,
                      "members": [{{"username": "wxid_a", "group_display": "",
                                   "remark": "张三", "nickname": "", "alias": "",
                                   "source": "sender_pool"}}]}}]}}))
            elif "--voice-out" in args:
                vdir = pathlib.Path(args[args.index("--voice-out") + 1])
                vdir.mkdir(parents=True, exist_ok=True)
                (vdir / {silk_name!r}).write_bytes(b"SILK")
                rows = [{{"local_id": 1, "create_time": 1700000100, "sort_seq": 1, "local_type": 34,
                          "content": "", "sender_username": "wxid_a",
                          "voice": {{"status": "extracted", "file": {silk_name!r}}}}}]
                print(json.dumps({{"mode": "messages", "sid_found": True, "messages": rows}}))
        '''), encoding="utf-8")
        # ASR 替身（voice2text.cjs 约定路径，由 python 执行）
        asr = tmp_path / "asr_root"
        asr.mkdir()
        (asr / "voice2text.cjs").write_text(
            'import json, sys\n'
            'for f in sys.argv[1:]:\n'
            '    print(json.dumps({"file": f, "ok": True, "text": "语音被听见了"}))\n',
            encoding="utf-8")
        cfg = _cfg(tmp_path)
        cfg.asr_root = asr                                 # 启用语音链路（voice2text_js 随之有效）
        out = cli.run_export(cfg, [Target(sid="g@chatroom", chat_type=0, display="群")],
                             fmt="csv", now=datetime(2026, 1, 1),
                             argv_prefix=[sys.executable, str(script)])
        recs = list(csv.DictReader(open(out["files"][0]["path"], encoding="utf-8-sig", newline="")))
        assert recs[0]["消息内容"] == "语音被听见了"        # ASR 文本进了内容列
        assert out["warnings"] == []
        # 实现：语音消息 → silk 落盘 → ASR → 内容列，全链路（替身层）。
        # 解析：ASR 文本不加前缀（用户口径：语音用 ASR 转文字，原样进内容列）。
        # 功能：语音链路的端到端锚点。

    def test_empty_targets_raises(self, tmp_path):
        with pytest.raises(ValueError):
            cli.run_export(_cfg(tmp_path), [], fmt="csv")
        # 功能：空目标防线。

    def test_jsonl_rejected_for_history(self, tmp_path):
        cfg = _cfg(tmp_path)
        targets = [Target(sid="wxid_a", chat_type=1, display="张三")]
        with pytest.raises(ValueError, match="jsonl"):
            cli.run_export(cfg, targets, fmt="jsonl", now=datetime(2026, 1, 1),
                           argv_prefix=_make_prefix(tmp_path))
        # 实现：jsonl 是实时专用格式，历史导出明确拒绝。
        # 功能：格式防线。
