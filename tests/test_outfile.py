"""wxmsg/outfile.py 的单元测试——六字段记录、CSV/JSON/JSONL 三格式、命名清洗与追加语义。"""
# 实现：模块 docstring 声明测试范围（全部纯文件操作）。
# 功能：锁定字段规格与三格式的落盘口径。
import csv
import json

import pytest

from wxmsg import outfile
from wxmsg.outfile import (
    CSV_HEADERS, append_csv, append_jsonl, make_record, merged_filename,
    sanitize_name, session_filename, write_csv, write_json,
)
# 实现：import 被测函数与表头常量。
# 功能：取得被测对象。

_ROW = {"local_id": 7, "create_time": 1700000000, "sort_seq": 99,
        "local_type": 1, "content": "你好，世界\n第二行", "sender_username": "wxid_x"}


class TestMakeRecord:
    def test_six_fields(self):
        rec = make_record(0, "前端群", "张三", _ROW)
        assert rec["chat_type"] == 0
        assert rec["session_name"] == "前端群"
        assert rec["sender_name"] == "张三"
        assert rec["timestamp"] == 1700000000
        assert rec["content"] == "你好，世界\n第二行"
        assert rec["time_readable"] == "2023-11-15 06:13:20"  # 本机时区（UTC+8）
        # 实现：六字段逐一断言。
        # 解析：可读时间按本机时区格式化——断言值按 UTC+8 环境写定，其它时区的机器跑测试
        #       会红（本项目仅在本机 Windows 跑，口径明确）。
        # 功能：字段规格的锚点。

    def test_private_sender_none(self):
        rec = make_record(1, "张三", None, _ROW)
        assert rec["sender_name"] is None
        # 实现：私聊 sender 传 None。
        # 解析：JSON 里是 null；CSV 里由 write_csv 落空串（字段③私聊口径）。
        # 功能：私聊字段③锚点。

    def test_missing_row_fields(self):
        rec = make_record(1, "x", None, {})
        assert rec["timestamp"] == 0 and rec["content"] == ""
        # 实现：行字段缺失宽松兜底。
        # 功能：防御性。


class TestNames:
    def test_sanitize(self):
        assert sanitize_name(r'张三/君:*?"<>|') == "张三_君_______"
        assert sanitize_name("很长的名字" * 20) == ("很长的名字" * 20)[:40]
        assert sanitize_name("   ") == "unnamed"
        assert sanitize_name("  ok  ") == "ok"
        # 实现：非法字符替换/截断/空名兜底。
        # 解析：Windows 文件名禁字符全部替换为下划线；40 字沿用 v1 _seg 经验值。
        # 功能：分文件命名的安全段。

    def test_filenames(self):
        now = __import__("datetime").datetime(2026, 11, 15, 1, 2, 3)
        assert merged_filename("csv", now) == "chat_export_20261115_010203.csv"
        assert session_filename(0, "前端群", "jsonl", now) == "群聊_前端群_20261115_010203.jsonl"
        assert session_filename(1, "张三", "json", now) == "私聊_张三_20261115_010203.json"
        with pytest.raises(ValueError):
            merged_filename("xlsx", now)
        # 实现：合并/分文件命名与格式白名单。
        # 功能：文件命名口径。


class TestWriters:
    def test_csv_roundtrip(self, tmp_path):
        recs = [make_record(0, "前端群", "张三", _ROW),
                make_record(1, "张三", None, _ROW)]
        p = tmp_path / "out.csv"
        write_csv(p, recs)
        raw = p.read_bytes()
        assert raw.startswith(b"\xef\xbb\xbf")          # utf-8-sig BOM（Excel 兼容）
        with open(p, encoding="utf-8-sig", newline="") as f:
            rows = list(csv.reader(f))
        assert rows[0] == CSV_HEADERS
        assert rows[1] == ["0", "前端群", "张三", "1700000000", recs[0]["time_readable"],
                           "你好，世界\n第二行"]          # 含换行的内容被引号包裹（csv 模块行为）
        assert rows[2] == ["1", "张三", "", "1700000000", recs[1]["time_readable"],
                           "你好，世界\n第二行"]           # 私聊 sender 落空串
        # 实现：utf-8-sig 覆盖写 + 表头 + None→空串。
        # 解析：csv.writer 对换行内容自动加引号——Excel 打开单元格内换行正常。
        # 功能：CSV 落盘口径。

    def test_json_roundtrip(self, tmp_path):
        recs = [make_record(1, "张三", None, _ROW)]
        p = tmp_path / "out.json"
        write_json(p, recs)
        data = json.loads(p.read_text(encoding="utf-8"))
        assert data == recs
        assert data[0]["sender_name"] is None           # JSON 里保持 null
        assert p.read_text(encoding="utf-8").startswith("[")  # 标准数组
        # 实现：标准 JSON 数组、ensure_ascii=False、私聊 null。
        # 功能：JSON 落盘口径。

    def test_jsonl_append(self, tmp_path):
        p = tmp_path / "out.jsonl"
        append_jsonl(p, [make_record(1, "张三", None, _ROW)])
        append_jsonl(p, [make_record(1, "张三", None, {**_ROW, "create_time": 1700000001})])
        lines = p.read_text(encoding="utf-8").strip().splitlines()
        assert len(lines) == 2
        assert all(json.loads(ln)["timestamp"] for ln in lines)  # 每行独立合法
        assert json.loads(lines[1])["timestamp"] == 1700000001
        # 实现：追加两轮各一行，每行独立 JSON。
        # 功能：实时模式 JSON 追加口径。

    def test_csv_append_header_once(self, tmp_path):
        p = tmp_path / "out.csv"
        append_csv(p, [make_record(1, "张三", None, _ROW)])
        append_csv(p, [make_record(1, "张三", None, {**_ROW, "create_time": 1700000001})])
        with open(p, encoding="utf-8-sig", newline="") as f:
            rows = list(csv.reader(f))
        assert len(rows) == 3                            # 表头 + 2 行数据
        assert rows[0] == CSV_HEADERS
        assert rows[2][3] == "1700000001"
        # 实现：首建写表头、追加只加数据行。
        # 功能：实时模式 CSV 追加口径。

    def test_csv_append_to_existing_empty_file(self, tmp_path):
        p = tmp_path / "out.csv"
        p.write_text("", encoding="utf-8")               # 建了文件但没写头
        append_csv(p, [make_record(1, "张三", None, _ROW)])
        with open(p, encoding="utf-8-sig", newline="") as f:
            rows = list(csv.reader(f))
        assert rows[0] == CSV_HEADERS and len(rows) == 2
        # 实现：空文件也要补表头（防"建了文件没写头"的中间态）。
        # 功能：追加的防御分支。

    def test_writes_create_parent_dirs(self, tmp_path):
        write_json(tmp_path / "deep" / "dir" / "out.json", [])
        assert (tmp_path / "deep" / "dir" / "out.json").exists()
        # 实现：父目录自动创建（output/ 可能尚不存在）。
        # 功能：目录保障。
