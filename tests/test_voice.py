"""wxmsg/voice.py 的单元测试——用 fixture 脚本顶替 node+voice2text.cjs，走完整 NDJSON 协议。"""
# 实现：模块 docstring 声明测试方式（node_bin 指向当前解释器 + fixture 脚本）。
# 功能：锁定分批/NDJSON 解析/失败线索/三档 note 口径。
import json
import sys
import textwrap
from pathlib import Path

from wxmsg import voice
from wxmsg.config import KEY_DATA_ROOT, load_config

# 实现：import 被测模块与配置构造器。
# 功能：取得被测函数。


def _cfg(tmp_path, asr_root: Path | None):
    (tmp_path / "data_root").mkdir()
    cfg = load_config(environ={KEY_DATA_ROOT: str(tmp_path / "data_root")}, dotenv={}, root=tmp_path)
    cfg.node_bin = sys.executable           # fixture 走当前解释器（生产是 node）
    cfg.asr_root = asr_root                 # voice2text_js 是派生属性（asr_root 拼死文件名）
    return cfg
    # 实现：通过 asr_root 注入（voice2text_js 无 setter，它 = asr_root/voice2text.cjs）。
    # 功能：可控行为的 ASR 环境。


def _asr_fixture(tmp_path, body: str) -> None:
    """写临时 voice2text.cjs 替身（由 python 执行）到 asr_root 约定路径，argv = [脚本, silk...]。"""
    script = tmp_path / "voice2text.cjs"    # 生产里这是 JS；测试里 node_bin=python，后缀无所谓
    script.write_text(textwrap.dedent(body), encoding="utf-8")
    # 实现：写到 asr_root/voice2text.cjs（run_asr 的 is_file 检查按此约定路径）。
    # 功能：可控行为的 ASR 替身。


def _make_silk(tmp_path, names) -> list[Path]:
    files = []
    for n in names:
        p = tmp_path / "voices" / n
        p.parent.mkdir(exist_ok=True)
        p.write_bytes(b"\x02#SILK_V3" + n.encode())   # silk 魔数开头的假 blob（内容无所谓）
        files.append(p)
    return files
    # 实现：造 silk 假文件（collect_jobs 的 isfile 检查需要真文件在盘上）。
    # 功能：转写条目夹具。


class TestCollectJobs:
    def test_filters(self, tmp_path):
        files = _make_silk(tmp_path, ["a.silk", "b.silk"])
        msgs = [
            {"voice": {"status": "extracted", "file": "a.silk"}},              # 收
            {"voice": {"status": "extracted", "file": "missing.silk"}},        # 文件不在盘上：丢
            {"voice": {"status": "not_local", "file": "b.silk"}},              # 状态不对：丢
            {"voice": None},                                                   # 非语音：丢
            {},                                                                # 无 voice 键：丢
        ]
        jobs = voice.collect_jobs(msgs, tmp_path / "voices")
        assert len(jobs) == 1
        assert jobs[0][1] == tmp_path / "voices" / "a.silk"
        assert files  # files 造出来了（a/b 都在盘上），但只有 a 进 jobs
        # 实现：三重过滤（status=extracted + file 非空 + isfile）。
        # 功能：可转写清单的口径。

    def test_batches_greedy(self, tmp_path):
        files = _make_silk(tmp_path, [f"f{i}.silk" for i in range(3)])
        voice._ARGV_CAP = 20  # 临时调小预算触发分批（测完由 tmp_path 隔离，不改全局语义）
        try:
            batches = voice._batches(files)
        finally:
            voice._ARGV_CAP = 20000
        assert len(batches) >= 2
        assert [f for b in batches for f in b] == files  # 不丢不重，保序
        # 实现：贪心装箱按 argv 字符预算封箱。
        # 功能：分批算法锚点（Windows 命令行长度上限的防线）。


class TestRunAsr:
    def test_success_ndjson(self, tmp_path):
        files = _make_silk(tmp_path, ["a.silk", "b.silk"])
        _asr_fixture(tmp_path, '''
            import json, sys
            for f in sys.argv[1:]:
                print(json.dumps({"file": f, "ok": True, "text": "听写:" + f, "error": ""}))
        ''')
        msgs = [{"voice": {"status": "extracted", "file": p.name}} for p in files]
        jobs = voice.collect_jobs(msgs, tmp_path / "voices")
        cfg = _cfg(tmp_path, tmp_path)
        by_file, why = voice.run_asr(jobs, cfg)
        assert why == []
        assert by_file[voice.os.path.normpath(str(files[0]))]["text"] == f"听写:{files[0]}"
        # 实现：替身按协议对每个 argv 文件回一条 NDJSON。
        # 功能：NDJSON 解析与路径归一索引。

    def test_partial_then_exit1(self, tmp_path):
        files = _make_silk(tmp_path, ["a.silk", "b.silk"])
        _asr_fixture(tmp_path, '''
            import json, sys
            print(json.dumps({"file": sys.argv[1], "ok": True, "text": "第一条"}))
            sys.stderr.write("fatal: model missing\\n")
            sys.exit(1)
        ''')
        msgs = [{"voice": {"status": "extracted", "file": p.name}} for p in files]
        jobs = voice.collect_jobs(msgs, tmp_path / "voices")
        by_file, why = voice.run_asr(jobs, _cfg(tmp_path, tmp_path))
        assert len(why) == 1 and "退出码 1" in why[0] and "fatal" in why[0]
        assert by_file[voice.os.path.normpath(str(files[0]))]["text"] == "第一条"
        assert voice.os.path.normpath(str(files[1])) not in by_file
        # 实现：node 非 0 退出但 stdout 已有结果——结果仍要捞回，退出码进 why。
        # 解析：对应"前 N 条成功、某条崩了"的真实形态（v1 口径：解析不因退出码跳过）。
        # 功能：部分成功锚点。

    def test_missing_js(self, tmp_path):
        files = _make_silk(tmp_path, ["a.silk"])
        jobs = voice.collect_jobs([{"voice": {"status": "extracted", "file": "a.silk"}}],
                                  tmp_path / "voices")
        by_file, why = voice.run_asr(jobs, _cfg(tmp_path, None))
        assert by_file == {} and why and "voice2text.cjs" in why[0]
        # 实现：js 缺失整批跳过（环境缺失 vs 转写失败分开报）。
        # 功能：环境缺失分支。


class TestTranscribeMessages:
    def test_end_to_end_and_note(self, tmp_path):
        files = _make_silk(tmp_path, ["a.silk", "b.silk"])
        _asr_fixture(tmp_path, '''
            import json, sys
            for f in sys.argv[1:]:
                ok = "b.silk" not in f          # 模拟 b 这条静音（ok 但无文本）
                print(json.dumps({"file": f, "ok": ok, "text": ("文字" if ok else "")}))
        ''')
        msgs = [{"voice": {"status": "extracted", "file": p.name}} for p in files]
        text_map, note = voice.transcribe_messages(msgs, _cfg(tmp_path, tmp_path), tmp_path / "voices")
        assert text_map[voice.os.path.normpath(str(files[0]))] == "文字"
        assert text_map[voice.os.path.normpath(str(files[1]))] is None   # ok 但空文本 = 没转出来
        assert "只转写成功 1 条" in note
        # 实现：三重条件（结果/ok/text 非空）判定成功；部分成功给非空 note。
        # 解析：SenseVoice 对静音回 ok:true 但 text 空串，必须按没转出来处理（v1 踩过的坑）。
        # 功能：端到端转写与三档 note。

    def test_all_ok_empty_note(self, tmp_path):
        files = _make_silk(tmp_path, ["a.silk"])
        _asr_fixture(tmp_path, '''
            import json, sys
            print(json.dumps({"file": sys.argv[1], "ok": True, "text": "你好"}))
        ''')
        msgs = [{"voice": {"status": "extracted", "file": "a.silk"}}]
        text_map, note = voice.transcribe_messages(msgs, _cfg(tmp_path, tmp_path), tmp_path / "voices")
        assert note == ""
        assert list(text_map.values()) == ["你好"]
        # 功能：全好档 note 为空。

    def test_no_jobs(self, tmp_path):
        text_map, note = voice.transcribe_messages([{"content": "纯文本"}], _cfg(tmp_path, None),
                                                   tmp_path / "voices")
        assert text_map == {} and note == ""
        # 实现：没有语音条目零成本早退（无语音不是错误，不该有状态噪音）。
        # 功能：空清单分支。

    def test_make_transcriber(self, tmp_path):
        files = _make_silk(tmp_path, ["a.silk"])
        text_map = {str(files[0]): "你好"}
        cb = voice.make_transcriber(text_map, tmp_path / "voices")
        assert cb("a.silk") == "你好"
        assert cb("nope.silk") is None
        # 实现：回调把 voice.file 相对名拼回 silk 绝对路径再查表。
        # 功能：render 回调的数据源口径。
