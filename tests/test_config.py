"""wxmsg/config.py 的单元测试——全部通过 environ/dotenv/root 注入，不碰真实 .env 与 os.environ。"""
# 实现：模块 docstring 声明隔离原则。
# 解析：load_config 的三个注入参数就是为测试设计的；真实 .env 只在步骤 12 真机验收时人工确认。
# 功能：锁定配置解析口径（必填闸/默认值/优先级/路径解析/ASR 回退）。
import json

import pytest

from wxmsg.config import (
    ConfigError, DEFAULT_EXPORTER_BIN, DEFAULT_OUTPUT_DIR, DEFAULT_POLL_INTERVAL_S,
    KEY_DATA_ROOT, load_config, load_env_file, project_root, resolve_path,
)
# 实现：import 被测函数与关键默认值常量。
# 功能：测试引用与 config.example 对齐的常量，防默认值漂移。


def _env(data_root: str | None = None, **extra) -> dict[str, str]:
    """构造最小合法 environ 注入（WXMSG_DATA_ROOT 指向真实存在的目录由各测试自定）。"""
    env = {}
    if data_root is not None:
        env[KEY_DATA_ROOT] = data_root
    env.update(extra)
    return env
    # 实现：测试辅助——拼 environ 注入字典。
    # 功能：减少每个用例的重复构造。


class TestRequiredDataRoot:
    def test_missing_raises(self, tmp_path):
        with pytest.raises(ConfigError, match="WXMSG_DATA_ROOT"):
            load_config(environ={}, dotenv={}, root=tmp_path)
        # 实现：不给数据根调用 load_config。
        # 解析：必填闸——错误 message 必须提到键名并给操作指引（match 校验）。
        # 功能：验证缺配置报 ConfigError。

    def test_empty_value_raises(self, tmp_path):
        with pytest.raises(ConfigError):
            load_config(environ={KEY_DATA_ROOT: ""}, dotenv={}, root=tmp_path)
        # 实现：数据根配置成空字符串。
        # 解析：空串与缺失同罪（面板上用户常先建 .env 后填值）。
        # 功能：验证空值处理。

    def test_missing_dir_raises(self, tmp_path):
        with pytest.raises(ConfigError, match="不存在"):
            load_config(environ={KEY_DATA_ROOT: str(tmp_path / "no_such_dir")},
                        dotenv={}, root=tmp_path)
        # 实现：数据根指向不存在的目录。
        # 解析：存在性校验在 config 层做，面板体检因此能直接给可操作文案。
        # 功能：验证目录存在性闸。


class TestDefaultsAndPriority:
    def test_defaults(self, tmp_path):
        cfg = load_config(environ={KEY_DATA_ROOT: str(tmp_path)}, dotenv={}, root=tmp_path)
        assert cfg.output_dir == (tmp_path / DEFAULT_OUTPUT_DIR).resolve()
        assert cfg.exporter_bin == (tmp_path / DEFAULT_EXPORTER_BIN).resolve()
        assert cfg.poll_interval_s == DEFAULT_POLL_INTERVAL_S
        assert cfg.keyscan_timeout_s == 900
        assert cfg.exporter_timeout_s == 180
        assert cfg.node_bin == "node"
        assert cfg.data_root == tmp_path.resolve()
        # 实现：只给必填项，其余全走默认。
        # 解析：默认值断言与 config.example 注释一一对应，改默认值必须同步改这里与 example。
        # 功能：锁定默认值口径。

    def test_env_overrides_dotenv(self, tmp_path):
        dot = tmp_path / ".env"   # 注：dotenv 由注入给出，这个文件仅为说明对应关系
        (tmp_path / "out_dotenv").mkdir()
        (tmp_path / "out_environ").mkdir()
        cfg = load_config(environ={KEY_DATA_ROOT: str(tmp_path), "WXMSG_OUTPUT_DIR": "out_environ"},
                          dotenv={"WXMSG_OUTPUT_DIR": "out_dotenv"}, root=tmp_path)
        assert cfg.output_dir == (tmp_path / "out_environ").resolve()
        # 实现：同一键在 environ 与 dotenv 各配一个不同目录。
        # 解析：优先级口径是「真实环境变量 > .env > 默认值」（与 v1 一致）。
        # 功能：验证优先级顺序。


class TestPathResolution:
    def test_userprofile_expansion(self, tmp_path, monkeypatch):
        monkeypatch.setenv("USERPROFILE", str(tmp_path))
        (tmp_path / "wxdata").mkdir()  # 展开后的数据根必须真实存在（config 有存在性硬闸）
        cfg = load_config(environ={KEY_DATA_ROOT: r"%USERPROFILE%\wxdata"}, dotenv={}, root=tmp_path)
        assert cfg.data_root == (tmp_path / "wxdata").resolve()
        # 实现：数据根写 %USERPROFILE%\wxdata，monkeypatch 控制展开结果。
        # 解析：%VAR% 展开是 config.example 承诺的三种写法之一（Windows 用户常见习惯）。
        # 功能：验证环境变量引用展开。

    def test_relative_resolved_against_root_not_cwd(self, tmp_path):
        (tmp_path / "proj" / "data_root").mkdir(parents=True)  # 相对解析后的目标目录须真实存在
        cfg = load_config(environ={KEY_DATA_ROOT: "data_root"}, dotenv={},
                          root=tmp_path / "proj")
        assert cfg.data_root == (tmp_path / "proj" / "data_root").resolve()
        # 实现：数据根给相对路径，root 指到另一个目录，故意不改当前工作目录。
        # 解析：相对路径必须按项目根解析而不是 cwd——双击 bat 启动时资源管理器的当前目录不可控。
        # 功能：验证「相对项目根」口径。
        assert cfg.data_root.is_absolute()
        # 实现：补充绝对性断言。
        # 功能：保证后续 os.walk 之类直接可用。


class TestIntParsing:
    def test_poll_interval_invalid_raises(self, tmp_path):
        with pytest.raises(ConfigError, match="整数"):
            load_config(environ={KEY_DATA_ROOT: str(tmp_path), "WXMSG_POLL_INTERVAL_S": "abc"},
                        dotenv={}, root=tmp_path)
        # 实现：轮询间隔配成非整数。
        # 功能：验证整数解析报错。

    def test_poll_interval_below_min_raises(self, tmp_path):
        with pytest.raises(ConfigError, match="最小"):
            load_config(environ={KEY_DATA_ROOT: str(tmp_path), "WXMSG_POLL_INTERVAL_S": "1"},
                        dotenv={}, root=tmp_path)
        # 实现：轮询间隔配 1 秒（低于下限 5）。
        # 解析：下限在 config 层报错而不是静默夹紧——配置错误要让用户看见。
        # 功能：验证轮询间隔下限闸。

    def test_timeouts_accept_valid(self, tmp_path):
        cfg = load_config(environ={KEY_DATA_ROOT: str(tmp_path),
                                   "WXMSG_KEYSCAN_TIMEOUT_S": "1200",
                                   "WXMSG_EXPORTER_TIMEOUT_S": "300"},
                          dotenv={}, root=tmp_path)
        assert cfg.keyscan_timeout_s == 1200
        assert cfg.exporter_timeout_s == 300
        # 实现：两个超时配合法大值。
        # 功能：验证整数正常路径。


class TestAsrRoot:
    def test_explicit_valid(self, tmp_path):
        asr = tmp_path / "v1_root"
        asr.mkdir()
        (asr / "voice2text.cjs").write_text("// js", encoding="utf-8")
        cfg = load_config(environ={KEY_DATA_ROOT: str(tmp_path), "WXMSG_ASR_ROOT": "v1_root"},
                          dotenv={}, root=tmp_path)
        assert cfg.asr_root == asr.resolve()
        assert cfg.voice2text_js == (asr / "voice2text.cjs").resolve()
        # 实现：显式配置 ASR 根且 voice2text.cjs 在位。
        # 解析：voice2text_js 是派生属性（asr_root 拼死文件名，v1 的入口脚本固定在项目根）。
        # 功能：验证 ASR 显式配置路径。

    def test_explicit_missing_degrades_to_none(self, tmp_path):
        cfg = load_config(environ={KEY_DATA_ROOT: str(tmp_path), "WXMSG_ASR_ROOT": "no_such"},
                          dotenv={}, root=tmp_path)
        assert cfg.asr_root is None
        # 实现：显式配置的 ASR 根不存在。
        # 解析：降级为 None（语音回退 [语音] 占位符）而不是报错——ASR 是增值功能，不卡主流程。
        # 功能：验证 ASR 降级口径。

    def test_fallback_hits_v1_layout(self, tmp_path):
        # 回退路径 = root/../Export_from_WeChat_v1（即 tmp_path/Export_from_WeChat_v1）
        v1 = tmp_path / "Export_from_WeChat_v1"
        v1.mkdir()
        (v1 / "voice2text.cjs").write_text("// js", encoding="utf-8")
        cfg = load_config(environ={KEY_DATA_ROOT: str(tmp_path)}, dotenv={}, root=tmp_path / "proj")
        assert cfg.asr_root == v1.resolve()
        # 实现：项目根建在 tmp_path/proj，旁边放标准 v1 布局。
        # 解析：回退条件带 voice2text.cjs 存在性检查，防「目录在但 ASR 没装」的半可用状态。
        # 功能：验证默认回退能命中真实 v1 布局。

    def test_no_asr_anywhere(self, tmp_path):
        cfg = load_config(environ={KEY_DATA_ROOT: str(tmp_path)}, dotenv={}, root=tmp_path / "proj")
        assert cfg.asr_root is None
        # 实现：回退路径也不存在。
        # 功能：验证彻底无 ASR 时 asr_root=None。


class TestLoadEnvFile:
    def test_parse_and_bom(self, tmp_path):
        env_file = tmp_path / ".env"
        env_file.write_bytes(
            "﻿# 注释行\nWXMSG_DATA_ROOT=D:/xwechat_files\nBAD_LINE_NO_EQUALS\n"
            "URL=https://a.b?x=1&y=2\n  SPACED  =  value  \n".encode("utf-8-sig"))
        env = load_env_file(env_file)
        assert env["WXMSG_DATA_ROOT"] == "D:/xwechat_files"
        assert env["URL"] == "https://a.b?x=1&y=2"      # 值里的 '=' 不被切坏
        assert env["SPACED"] == "value"                  # 两端 strip
        assert all(k != "" for k in env)
        # 实现：带 BOM 的 .env（utf-8-sig 编码写入）+ 注释 + 坏行 + 含 '=' 的值 + 空白。
        # 解析：utf-8-sig 吃 BOM 是 v1 踩过的坑（BOM 黏在首个 key 上导致读不到）；
        #       partition 按第一个 '=' 切分保护 URL 参数。
        # 功能：锁定 .env 解析口径。

    def test_missing_file_returns_empty(self, tmp_path):
        assert load_env_file(tmp_path / "nope.env") == {}
        # 实现：读不存在的 .env。
        # 解析：首次使用未建 .env 是正常状态，返回空表而不是抛错。
        # 功能：验证缺文件分支。


def test_project_root_is_repo_root():
    root = project_root()
    assert (root / "AGENTS.md").is_file() and (root / ".gitignore").is_file()
    assert (root / "config.example").is_file()
    # 实现：用仓库标志性文件验证 project_root 指向 read_wechat/。
    # 解析：project_root 是全部相对路径的基准，锚错会全局错位——用真实文件钉住。
    # 功能：验证项目根定位。
