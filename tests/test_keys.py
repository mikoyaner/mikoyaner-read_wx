"""wxmsg/keys.py 的单元测试——keyscan 用临时 fixture 脚本，绝不碰真实微信进程。"""
# 实现：模块 docstring 声明隔离原则。
# 功能：锁定日志白名单闸与 ensure_keys 的退出码/超时/诊断口径。
import textwrap

from wxmsg import keys
from wxmsg.config import KEY_DATA_ROOT, load_config, project_root

# 实现：import 被测模块与配置构造器。
# 功能：取得被测函数与 cfg 夹具构造。


def _cfg(tmp_path):
    (tmp_path / "data_root").mkdir()
    return load_config(environ={KEY_DATA_ROOT: str(tmp_path / "data_root")}, dotenv={}, root=tmp_path)
    # 实现：最小合法 Config（数据根指向空目录——keyscan fixture 不真读它）。
    # 功能：各测试共用夹具。


def _fixture_script(tmp_path, body: str):
    """写一个临时 keyscan 替身脚本，返回其路径。body 是脚本主体（可 print/exit）。"""
    script = tmp_path / "fake_keyscan.py"
    script.write_text(textwrap.dedent(body), encoding="utf-8")
    return script
    # 实现：body 直接作为脚本主体（不 import 任何东西，纯 print/sys.exit）。
    # 功能：可控行为的 keyscan 替身。


class TestFilterLogLines:
    def test_whitelist_only(self):
        lines = [
            "[scan] 发现 3 个 .db",
            "candidate key fragment should-drop",   # 无前缀：可能含密钥材料，必须丢
            "[stat] needles=1 nodes=1",
            "[result] 命中 2 / 3 个库",
            "[store] 已写入",
            "Traceback (most recent call last):",   # traceback 带本机路径，必须丢
            "",
        ]
        kept = keys.filter_log_lines(lines)
        assert kept == ["[scan] 发现 3 个 .db", "[stat] needles=1 nodes=1",
                        "[result] 命中 2 / 3 个库", "[store] 已写入"]
        # 实现：白名单四前缀外的行全部丢弃。
        # 解析：这是密钥材料的日志闸——任何无前缀行（含 traceback/裸 key 片段）都不能进日志。
        # 功能：钉死红线口径。

    def test_empty(self):
        assert keys.filter_log_lines([]) == []
        # 功能：空输入不炸。


class TestEnsureKeys:
    def test_success_filters_secret_lines(self, tmp_path):
        script = _fixture_script(tmp_path, '''
            import sys
            print("[scan] 开始扫描")
            print("RAW_KEY_MATERIAL_xxx_should_not_appear")   # 模拟含密钥材料的行
            print("[store] 已写入 store.json")
        ''')
        cfg = _cfg(tmp_path)
        ok, lines = keys.ensure_keys(cfg, keyscan_py=script)
        assert ok is True
        assert any("[store] 已写入" in ln for ln in lines)
        assert all("should_not_appear" not in ln for ln in lines)
        # 实现：fixture 打印两条白名单行 + 一条裸材料行，退出码 0。
        # 解析：成功时也必须过滤——白名单闸不分成功/失败路径。
        # 功能：验证成功路径与日志闸。

    def test_failure_exit3_keeps_result_line(self, tmp_path):
        script = _fixture_script(tmp_path, '''
            import sys
            print("[result] 内存里没找到 Config.Cipher 字样")
            sys.exit(3)
        ''')
        ok, lines = keys.ensure_keys(cfg=_cfg(tmp_path), keyscan_py=script)
        assert ok is False
        assert any("[result]" in ln for ln in lines)
        # 实现：退出码 3（没扫到）+ [result] 诊断行。
        # 解析：失败原因必须可见（面板据此提示"微信未运行/版本变化"）。
        # 功能：验证失败路径的可诊断性。

    def test_failure_without_whitelist_gets_fallback_line(self, tmp_path):
        script = _fixture_script(tmp_path, '''
            import sys
            print("random noise without prefix")
            sys.exit(1)
        ''')
        ok, lines = keys.ensure_keys(cfg=_cfg(tmp_path), keyscan_py=script)
        assert ok is False
        assert len(lines) == 1 and "退出码 1" in lines[0]
        # 实现：失败但白名单后没有一行——兜底一行说明（避免调用方面对空原因）。
        # 功能：验证兜底分支。

    def test_timeout(self, tmp_path):
        script = _fixture_script(tmp_path, '''
            import time
            time.sleep(3)
        ''')
        ok, lines = keys.ensure_keys(cfg=_cfg(tmp_path), keyscan_py=script, timeout=1)
        assert ok is False
        assert any("超时" in ln for ln in lines)
        # 实现：fixture 睡 3 秒，超时设 1 秒。
        # 解析：超时折成 (False, 一行可读诊断) 而不是抛异常——面板按钮场景不该炸栈。
        # 功能：验证超时分支（测试耗时约 1 秒）。

    def test_store_path_default_is_project_cache(self, tmp_path, monkeypatch):
        monkeypatch.delenv("WEFLOW_KEY_STORE", raising=False)
        assert keys.store_path() == project_root() / "cache" / "keystore" / "store.json"
        monkeypatch.setenv("WEFLOW_KEY_STORE", str(tmp_path / "custom.json"))
        assert keys.store_path() == tmp_path / "custom.json"
        # 实现：默认 = 项目 cache/keystore/store.json（用户拍板：缓存不写 C 盘）；
        #       WEFLOW_KEY_STORE 环境变量仍是最优先（Python/Rust/子进程三方的对齐开关）。
        # 解析：v1 的 %APPDATA% 位置不再是默认——只在迁移时作为来源读取。
        # 功能：store 定位口径（迁移到项目内）。

    def test_migrate_legacy_store(self, tmp_path, monkeypatch):
        # 用 WEFLOW_KEY_STORE 把迁移目标指进 tmp——migrate 走 store_path()，天然支持注入，
        # 测试绝不触碰真实项目 cache/（那里是运行时密钥的领地）。
        target = tmp_path / "cache" / "keystore" / "store.json"
        monkeypatch.setenv("WEFLOW_KEY_STORE", str(target))
        fake_appdata = tmp_path / "appdata"
        legacy = fake_appdata / "weflow-key-provider" / "store.json"
        legacy.parent.mkdir(parents=True)
        legacy.write_text('{"db_keys":{}}', encoding="utf-8")
        monkeypatch.setenv("APPDATA", str(fake_appdata))
        note = keys.migrate_legacy_store()
        assert "迁移" in note
        assert target.exists()
        assert target.read_text(encoding="utf-8") == '{"db_keys":{}}'
        assert legacy.exists()                          # 原文件保留（v1 不受影响）
        assert keys.migrate_legacy_store() == ""        # 目标已有 → 不再动作
        # 实现：复制不移动 + 目标已有跳过。
        # 解析：migrate 的目标是 store_path()——与运行时读写同一份，测试通过环境变量重定向到 tmp。
        # 功能：C 盘 → 项目内 的密钥库迁移口径。

    def test_migrate_noop_when_absent(self, tmp_path, monkeypatch):
        monkeypatch.setenv("WEFLOW_KEY_STORE", str(tmp_path / "cache" / "store.json"))
        monkeypatch.setenv("APPDATA", str(tmp_path / "empty_appdata"))  # 没有 legacy
        assert keys.migrate_legacy_store() == ""
        assert not (tmp_path / "cache" / "store.json").exists()
        # 实现：两边都没有 → 无动作（不建空目录）。
        # 功能：全新机器零噪音。
