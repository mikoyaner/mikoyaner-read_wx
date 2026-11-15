"""wxmsg/exporter.py 的单元测试——用 fixture 替身脚本顶替 exe，走完整子进程协议。"""
# 实现：模块 docstring 声明测试方式（argv_prefix 注入替身，不依赖真 Rust exe/真微信）。
# 功能：锁定子进程协议（argv 构造/退出码/JSON 解析/超时）与自动重扫重试语义。
import json
import sys
import textwrap

import pytest

from wxmsg import exporter
from wxmsg.config import KEY_DATA_ROOT, load_config

# 实现：import 被测模块、json/sys（构造 fixture 与 argv 前缀）、pytest（异常断言）。
# 功能：取得被测函数。


def _cfg(tmp_path):
    (tmp_path / "data_root").mkdir()
    return load_config(environ={KEY_DATA_ROOT: str(tmp_path / "data_root")}, dotenv={}, root=tmp_path)
    # 实现：最小合法 Config（exporter_bin 不被用到——argv_prefix 顶替）。
    # 功能：各测试共用夹具。


def _fixture(tmp_path, body: str) -> list[str]:
    """写临时替身脚本并返回 argv 前缀 [python, script]。body 里可用 sys.argv[1:]。"""
    script = tmp_path / "fake_exporter.py"
    script.write_text(textwrap.dedent(body), encoding="utf-8")
    return [sys.executable, str(script)]
    # 实现：替身脚本 + 当前解释器组成 argv_prefix。
    # 功能：可控行为的导出器替身（生产代码传 argv_prefix=None 走真 exe）。


class TestRunExporter:
    def test_success_and_arg_construction(self, tmp_path):
        prefix = _fixture(tmp_path, '''
            import json, sys
            # 把收到的 argv 原样回显进 JSON，供断言参数构造顺序
            print(json.dumps({"mode": "echo", "argv": sys.argv[1:]}))
        ''')
        cfg = _cfg(tmp_path)
        out = exporter.messages(cfg, "abc@chatroom", begin=10, end=20,
                                voice_out=tmp_path / "v", argv_prefix=prefix)
        assert out["mode"] == "echo"
        # argv 是 sys.argv[1:]（已跳过解释器与脚本名），所以从 --data-root 开始
        assert out["argv"][:2] == ["--data-root", str(cfg.data_root)]
        assert out["argv"][2:4] == ["--messages", "--sid"]
        assert out["argv"][4] == "abc@chatroom"
        assert out["argv"][5:9] == ["--begin", "10", "--end", "20"]
        assert out["argv"][9:11] == ["--voice-out", str(tmp_path / "v")]
        # 实现：替身把 argv 回显，断言 data-root/模式旗标/附加参数的顺序与值。
        # 解析：参数顺序是协议的一部分（Rust 端手写解析按序吃值），顺序错=功能错。
        # 功能：锁定 Python→Rust 的命令构造。

    def test_nonzero_exit_includes_stderr_tail(self, tmp_path):
        prefix = _fixture(tmp_path, '''
            import sys
            sys.stderr.write("noise\\n[err] 数据根目录不存在\\n")
            sys.exit(1)
        ''')
        with pytest.raises(exporter.ExporterError, match="退出码 1.*数据根目录不存在"):
            exporter.run_exporter(_cfg(tmp_path), "list_accounts", [], argv_prefix=prefix)
        # 实现：替身退出码 1 + stderr 诊断。
        # 解析：错误文案必须带上 stderr 尾部（Rust 的 [err] 行），否则面板只知道"失败"不知道为什么。
        # 功能：验证失败上报口径。

    def test_invalid_json_raises(self, tmp_path):
        prefix = _fixture(tmp_path, '''
            print("this is not json")
        ''')
        with pytest.raises(exporter.ExporterError, match="不是合法 JSON"):
            exporter.run_exporter(_cfg(tmp_path), "list_sessions", [], argv_prefix=prefix)
        # 实现：替身打印非 JSON。
        # 解析：stdout 被污染 = 协议破坏，按 bug 报错而不是吞掉。
        # 功能：验证 JSON 解析闸。

    def test_timeout_raises(self, tmp_path):
        prefix = _fixture(tmp_path, '''
            import time
            time.sleep(3)
        ''')
        with pytest.raises(exporter.ExporterError, match="超时"):
            exporter.run_exporter(_cfg(tmp_path), "list_accounts", [],
                                  argv_prefix=prefix, timeout=1)
        # 实现：替身睡 3 秒，超时 1 秒。
        # 功能：验证超时分支（测试约 1 秒）。

    def test_weflow_key_store_env_passed(self, tmp_path, monkeypatch):
        # 替身打印收到的 WEFLOW_KEY_STORE，断言子进程环境里被指向项目 cache/
        prefix = _fixture(tmp_path, '''
            import json, os
            print(json.dumps({"store_env": os.environ.get("WEFLOW_KEY_STORE", "")}))
        ''')
        monkeypatch.delenv("WEFLOW_KEY_STORE", raising=False)
        cfg = _cfg(tmp_path)
        out = exporter.run_exporter(cfg, "list_accounts", [], argv_prefix=prefix)
        assert out["store_env"] == str(exporter.keys.store_path())
        assert "cache" in out["store_env"]
        # 实现：run_exporter 给子进程注入 WEFLOW_KEY_STORE（Rust keystore 的最高优先级来源）。
        # 解析：这是「密钥库统一落项目 cache/」的落地机制——keyscan 写它、导出器读它，同一份。
        # 功能：密钥库位置的三方对齐。


class TestRescanRetry:
    def test_retry_after_rescan(self, tmp_path):
        marker = tmp_path / "flipped.marker"
        prefix = _fixture(tmp_path, f'''
            import sys
            from pathlib import Path
            # 第一次调用退出 1，之后成功（用 marker 文件翻面）
            if not Path({str(marker)!r}).exists():
                Path({str(marker)!r}).write_text("x")
                sys.exit(1)
            print('{{"ok": true}}')
        ''')
        calls = []
        cfg = _cfg(tmp_path)
        out = exporter.list_accounts(cfg, argv_prefix=prefix,
                                     rescan=lambda: (calls.append(1), True)[1])
        assert out == {"ok": True}
        assert len(calls) == 1
        # 实现：第一次退出 1 → rescan 被调（计数 1）→ 重试成功。
        # 解析：对应"重登录换钥"的真实场景——一次重扫就能恢复。
        # 功能：锁定「失败→重扫→重试一次」语义。

    def test_rescan_fail_raises(self, tmp_path):
        prefix = _fixture(tmp_path, '''
            import sys
            sys.exit(1)
        ''')
        with pytest.raises(exporter.ExporterError, match="重扫密钥后仍失败"):
            exporter.list_accounts(_cfg(tmp_path), argv_prefix=prefix, rescan=lambda: False)
        # 实现：替身永远失败，rescan 返回 False。
        # 解析：重扫失败必须停（不给无限重试机会），文案给出两个最常见根因。
        # 功能：验证重扫失败分支。

    def test_retry_still_fails_raises(self, tmp_path):
        prefix = _fixture(tmp_path, '''
            import sys
            sys.exit(1)
        ''')
        with pytest.raises(exporter.ExporterError, match="退出码 1"):
            exporter.list_accounts(_cfg(tmp_path), argv_prefix=prefix, rescan=lambda: True)
        # 实现：rescan 成功但重试仍失败。
        # 解析：重试只有一次——结构性失败（版本不匹配/数据根错）不能变成卡死循环。
        # 功能：验证重试仍失败分支。

    def test_first_success_never_rescans(self, tmp_path):
        prefix = _fixture(tmp_path, '''
            import json
            print(json.dumps({"ok": True}))
        ''')
        calls = []
        exporter.list_sessions(_cfg(tmp_path), argv_prefix=prefix,
                               rescan=lambda: (calls.append(1), True)[1])
        assert calls == []
        # 实现：首次成功，rescan 计数为 0。
        # 功能：验证成功路径零额外开销。

    def test_account_switch_auto_rescan(self, tmp_path):
        # 换号防线：第一次返回旧号数据（serves_current_login=False），重扫后拿到当前号
        marker = tmp_path / "switch.marker"
        prefix = _fixture(tmp_path, f'''
            import sys, json
            from pathlib import Path
            ok = Path({str(marker)!r}).exists()
            if not ok:
                Path({str(marker)!r}).write_text("x")
            print(json.dumps({{"ok": True, "serves_current_login": ok,
                               "current_login_wxid": "wxid_new",
                               "serving_wxid": ("wxid_old" if not ok else "wxid_new")}}))
        ''')
        calls = []
        out = exporter.list_sessions(_cfg(tmp_path), argv_prefix=prefix,
                                     rescan=lambda: (calls.append(1), True)[1])
        assert out["serves_current_login"] is True and out["serving_wxid"] == "wxid_new"
        assert len(calls) == 1
        # 实现：serves_current_login=False → 自动重扫一次 → 重试拿到新号数据。
        # 解析：这是「换号后点一下刷新即可」的核心——用户无感知完成密钥更新。
        # 功能：换号自愈路径。

    def test_account_switch_persists_raises(self, tmp_path):
        # 重扫后仍是旧号数据 → 明确报错，绝不拿旧号数据冒充当前号
        prefix = _fixture(tmp_path, '''
            import json
            print(json.dumps({"ok": True, "serves_current_login": False,
                              "current_login_wxid": "wxid_new", "serving_wxid": "wxid_old"}))
        ''')
        calls = []
        with pytest.raises(exporter.ExporterError, match="检测到换号"):
            exporter.list_sessions(_cfg(tmp_path), argv_prefix=prefix,
                                   rescan=lambda: (calls.append(1), True)[1])
        assert len(calls) == 1
        # 实现：重扫一次后仍不匹配 → ExporterError（文案带两个 wxid）。
        # 解析：旧号密钥恰好还能用时会"成功返回错的数据"——比报错更危险，必须拦截。
        # 功能：换号不可恢复分支。

    def test_mismatch_ignored_when_no_current_login(self, tmp_path):
        # current_login_wxid 为空 = 数据根下没有任何账号在写库（微信没登录）——
        # 这不是换号，不触发重扫防线（重扫也扫不出钥，白等几十秒）
        prefix = _fixture(tmp_path, '''
            import json
            print(json.dumps({"ok": True, "serves_current_login": False,
                              "current_login_wxid": ""}))
        ''')
        calls = []
        out = exporter.list_sessions(_cfg(tmp_path), argv_prefix=prefix,
                                     rescan=lambda: (calls.append(1), True)[1])
        assert calls == [] and out["serves_current_login"] is False
        # 实现：换号防线要求 current_login_wxid 非空才触发。
        # 功能：区分「换号」与「微信没登录」。
