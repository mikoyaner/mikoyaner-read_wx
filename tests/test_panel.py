"""wxmsg/panel.py 的单元测试——纯函数口径 + 真 HTTP 服务（ephemeral 端口）+ monkeypatch 替身。"""
# 实现：模块 docstring 声明测试方式（真起 ThreadingHTTPServer，不依赖真导出器）。
# 功能：锁定 Host 防线、会话分组、API 协议与实时端点。
import json
import threading
import urllib.request
from http.server import ThreadingHTTPServer

import pytest

from wxmsg import panel
from wxmsg.config import KEY_DATA_ROOT, load_config
from wxmsg.session import Session

# 实现：import 被测模块、HTTP 服务与客户端工具。
# 功能：取得被测对象。


def _cfg(tmp_path):
    (tmp_path / "data_root").mkdir(exist_ok=True)
    cfg = load_config(environ={KEY_DATA_ROOT: str(tmp_path / "data_root")}, dotenv={}, root=tmp_path)
    cfg.output_dir = tmp_path / "output"
    cfg.node_bin = "node"
    return cfg
    # 实现：Config 夹具。
    # 功能：App/server 的宿主配置。


@pytest.fixture
def server(tmp_path, monkeypatch):
    """起一个真 HTTP 服务（端口 0 = 系统分配），替身注入 exporter/cli 依赖。"""
    srv = ThreadingHTTPServer(("127.0.0.1", 0), panel._Handler)
    srv.daemon_threads = True
    srv.app = panel.App(_cfg(tmp_path))
    t = threading.Thread(target=srv.serve_forever, daemon=True)
    t.start()
    yield srv
    srv.shutdown()
    # 实现：端口 0 让内核挑空闲端口；daemon 线程 + shutdown 保证测试干净退出。
    # 功能：HTTP 集成测试的服务端夹具。


def _post(srv, path, params=None, host=None):
    """POST JSON 客户端（http.client 直控，host 参数可伪造 Host 头测防线）。"""
    import http.client
    conn = http.client.HTTPConnection("127.0.0.1", srv.server_port, timeout=10)
    headers = {"Content-Type": "application/json"}
    body = json.dumps(params or {}).encode("utf-8")
    try:
        conn.putrequest("POST", path, skip_host=True, skip_accept_encoding=True)
        conn.putheader("Host", host or f"127.0.0.1:{srv.server_port}")
        conn.putheader("Content-Type", "application/json")
        conn.putheader("Content-Length", str(len(body)))
        conn.endheaders()
        conn.send(body)
        resp = conn.getresponse()
        data = resp.read().decode("utf-8")
        if resp.status != 200:
            raise urllib.error.HTTPError(url := f"http://127.0.0.1:{srv.server_port}{path}",
                                         resp.status, resp.reason, resp.msg, None)
        return json.loads(data)
    finally:
        conn.close()
    # 实现：skip_host 后手工放 Host——urllib 不允许覆盖 Host，http.client 可以；
    #      非 200 转 HTTPError 与 urllib 行为一致。
    # 功能：HTTP 集成测试的客户端。


class TestCheckHost:
    def test_allowed_and_rejected(self):
        assert panel._check_host("127.0.0.1:8765", 8765) is True
        assert panel._check_host("localhost:8765", 8765) is True
        assert panel._check_host("[::1]:8765", 8765) is True
        assert panel._check_host("evil.com:8765", 8765) is False
        assert panel._check_host(None, 8765) is False
        # 实现：Host 白名单三本机形态。
        # 解析：DNS rebinding 攻击的 Host 头是攻击域名——在这里被拒。
        # 功能：Host 防线的口径锚点。


class TestSplitSessions:
    def test_three_groups(self):
        sessions = [
            Session(sid="g@chatroom", is_group=True, session_display="", remark="", nickname="群A", alias="", sort_ts=30),
            Session(sid="wxid_b", is_group=False, session_display="", remark="老李", nickname="", alias="", sort_ts=20),
            Session(sid="gh_news", is_group=False, session_display="", remark="", nickname="新闻号", alias="", sort_ts=10),
        ]
        out = panel.split_sessions(sessions)
        assert [i["sid"] for i in out["groups"]] == ["g@chatroom"]
        assert [i["sid"] for i in out["privates"]] == ["wxid_b"]
        assert [i["sid"] for i in out["official"]] == ["gh_news"]
        assert out["privates"][0]["display"] == "老李"     # 展示名走回退链
        # 实现：群聊/私聊/公众号三分组 + 展示名预计算。
        # 功能：会话列表数据装配口径。

    def test_targets_from_params(self):
        ts = panel._targets_from_params({"targets": [
            {"sid": "g@chatroom", "chat_type": 0, "display": "群", "members": ["wxid_a"]},
            {"sid": "wxid_b", "chat_type": 1, "display": "老李", "members": None},
            {"sid": "", "chat_type": 1, "display": "空跳过"},
        ]})
        assert len(ts) == 2
        assert ts[0].selected_members == {"wxid_a"}        # 数组 = 筛选集合
        assert ts[1].selected_members is None              # null = 全群
        with pytest.raises(ValueError):
            panel._targets_from_params({"targets": []})
        # 实现：members 两种语义（数组/None）+ 空 sid 跳过 + 全空报错。
        # 功能：目标装配口径。


class TestHttpApi:
    def test_sessions_endpoint(self, server, monkeypatch):
        monkeypatch.setattr(panel.exporter, "list_sessions", lambda cfg, **kw: {
            "sessions": [
                {"sid": "g@chatroom", "is_group": True, "session_display": "", "remark": "", "nickname": "群A", "alias": "", "sort_ts": 9},
                {"sid": "wxid_b", "is_group": False, "session_display": "", "remark": "老李", "nickname": "", "alias": "", "sort_ts": 5},
                {"sid": "gh_x", "is_group": False, "session_display": "", "remark": "", "nickname": "号", "alias": "", "sort_ts": 1},
            ]})
        out = _post(server, "/api/sessions")
        assert out["ok"] is True
        assert [i["sid"] for i in out["groups"]] == ["g@chatroom"]
        assert [i["sid"] for i in out["official"]] == ["gh_x"]
        # 实现：替身 list_sessions → HTTP 全链路。
        # 功能：会话接口协议。

    def test_export_endpoint(self, server, monkeypatch):
        captured = {}
        def fake_run_export(cfg, targets, fmt="csv", **kw):
            captured["targets"] = targets
            captured["fmt"] = fmt
            return {"files": [{"sid": "", "display": "(合并)", "count": 2, "path": "x.csv"}],
                    "total": 2, "warnings": []}
        monkeypatch.setattr(panel.cli, "run_export", fake_run_export)
        out = _post(server, "/api/export", {"targets": [
            {"sid": "g@chatroom", "chat_type": 0, "display": "群", "members": ["wxid_a"]}],
            "fmt": "csv", "merged": True, "begin": "2026-01-01", "end": ""})
        assert out["ok"] is True and out["total"] == 2
        assert captured["fmt"] == "csv"
        assert captured["targets"][0].selected_members == {"wxid_a"}
        assert captured["targets"][0].display == "群"
        # 实现：替身 run_export 捕获参数。
        # 解析：面板层只做参数装配（targets/时间解析），不复制业务——断言聚焦装配正确。
        # 功能：导出接口协议。

    def test_error_protocol(self, server):
        out = _post(server, "/api/members", {"sid": ""})
        assert out["ok"] is False and "sid" in out["error"]
        out = _post(server, "/api/no_such_api")
        assert out["ok"] is False
        # 实现：参数错误/未知接口都走 {"ok": false, "error"}。
        # 功能：错误协议。

    def test_host_rejected(self, server):
        with pytest.raises(urllib.error.HTTPError) as ei:
            _post(server, "/api/sessions", {}, host="evil.com")
        assert ei.value.code == 403
        # 实现：伪造 Host 头 → 403。
        # 功能：DNS rebinding 防线的集成验证。

    def test_realtime_lifecycle(self, server, monkeypatch):
        started = {}
        class FakeRT:
            def __init__(self, cfg, targets, **kw):
                started["targets"] = targets
                started["include_history"] = kw.get("include_history")
            def poll_once(self):
                return {"new_total": 1, "sessions": [{"sid": "wxid_a", "display": "张三", "new": 1, "total": 1}], "at": "t"}
            def summary(self):
                return {"running": True, "total": 1, "files": []}
        monkeypatch.setattr(panel.realtime_mod, "RealtimeExport", FakeRT)
        out = _post(server, "/api/realtime/start", {"targets": [
            {"sid": "wxid_a", "chat_type": 1, "display": "张三", "members": None}],
            "fmt": "csv", "include_history": True})
        assert out["ok"] is True and out["total"] == 1
        assert started["include_history"] is True
        out = _post(server, "/api/realtime/poll")
        assert out["ok"] is True and out["new_total"] == 1
        out = _post(server, "/api/realtime/state")
        assert out["running"] is True
        out = _post(server, "/api/realtime/stop")
        assert out["ok"] is True and out["stopped"] is True
        out = _post(server, "/api/realtime/state")
        assert out["ok"] is True and out["running"] is False
        out = _post(server, "/api/realtime/poll")
        assert out["ok"] is False and "没有正在运行" in out["error"]
        # 实现：start（捕获 targets/include_history）→ poll → state → stop → 空态报错。
        # 功能：实时生命周期协议（前端定时器按此驱动）。
