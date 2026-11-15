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
    srv.app.search_history = panel.search_mod.SearchHistory(tmp_path / "history.json")
    # 搜索历史重定向到 tmp——绝不污染真实项目 cache/search_history.json
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

    def test_rescan_endpoint(self, server, monkeypatch):
        monkeypatch.setattr(panel.keys, "ensure_keys",
                            lambda cfg, log=None: (True, ["[store] 已写入 store.json"]))
        out = _post(server, "/api/rescan")
        assert out["ok"] is True and out["lines"] == ["[store] 已写入 store.json"]
        # 实现：/api/rescan 直通 keys.ensure_keys（重扫输出已过白名单闸）。
        # 功能：换号场景的手动重扫兜底按钮。

    def test_search_priv_endpoint(self, server, monkeypatch):
        monkeypatch.setattr(panel.exporter, "list_sessions", lambda cfg, **kw: {"sessions": [
            {"sid": "wxid_zs", "is_group": False, "session_display": "", "remark": "张三",
             "nickname": "", "alias": "", "sort_ts": 1},
            {"sid": "g1@chatroom", "is_group": True, "session_display": "", "remark": "",
             "nickname": "张三群", "alias": "", "sort_ts": 2},
        ]})
        out = _post(server, "/api/search_priv", {"query": "张三"})
        assert out["ok"] is True
        assert [r["sid"] for r in out["results"]] == ["wxid_zs"]   # 群被过滤（走群搜索行）
        # 实现：/api/search_priv = search.search_privates + 历史词记录。
        # 功能：第一行搜索的后端协议。

    def test_search_group_endpoint(self, server, monkeypatch):
        monkeypatch.setattr(panel.exporter, "list_sessions", lambda cfg, **kw: {"sessions": [
            {"sid": "g1@chatroom", "is_group": True, "session_display": "", "remark": "",
             "nickname": "前端群", "alias": "", "sort_ts": 2},
        ]})
        monkeypatch.setattr(panel.exporter, "group_members_all", lambda cfg, **kw: {"groups": [
            {"sid": "g1@chatroom", "name": "前端群", "error": None, "members": [
                {"username": "wxid_m", "group_display": None, "remark": "小明",
                 "nickname": "", "alias": "", "source": "sender_pool"}]},
        ]})
        out = _post(server, "/api/search_group", {"query": "前端群"})
        assert out["ok"] is True
        assert out["results"][0]["kind"] == "group" and out["results"][0]["sid"] == "g1@chatroom"
        out = _post(server, "/api/search_group", {"query": "小明"})
        assert out["ok"] is True and out["results"][0]["kind"] == "member"
        assert out["results"][0]["username"] == "wxid_m"
        # 实现：/api/search_group 走懒构建索引 + search.search_groups（群在前成员在后）。
        # 功能：第二行搜索的后端协议。

    def test_history_endpoint(self, server):
        out = _post(server, "/api/history", {"kind": "priv"})
        assert out["ok"] is True and isinstance(out["terms"], list)
        out = _post(server, "/api/history", {"kind": "bogus"})
        assert out["ok"] is False
        # 实现：/api/history 直通 SearchHistory.terms（坏类别 → ValueError → ok:false）。
        # 功能：datalist 历史词读取。

    def test_realtime_lifecycle(self, server, monkeypatch):
        """服务端后台线程生命周期：start 起线程 → 自动轮询累计 → stop 后彻底停。"""
        import time
        started = {}
        polls = {"n": 0}
        class FakeRT:
            def __init__(self, cfg, targets, **kw):
                started["targets"] = targets
                started["include_history"] = kw.get("include_history")
            def poll_once(self):
                polls["n"] += 1
                return {"new_total": 1, "sessions": [
                    {"sid": "wxid_a", "display": "张三", "new": 1, "total": polls["n"]}],
                    "at": "t"}
            def summary(self):
                return {"running": True, "total": polls["n"], "files": []}
        monkeypatch.setattr(panel.realtime_mod, "RealtimeExport", FakeRT)
        out = _post(server, "/api/realtime/start", {"targets": [
            {"sid": "wxid_a", "chat_type": 1, "display": "张三", "members": None}],
            "fmt": "csv", "include_history": True, "interval_s": 5})
        assert out["ok"] is True
        assert started["include_history"] is True
        time.sleep(0.8)                                  # 线程先立即 poll 一次再进入等待
        out = _post(server, "/api/realtime/state")
        assert out["ok"] is True and out["running"] is True
        assert out["total"] >= 1                         # 服务端在跑（页面无关）
        out = _post(server, "/api/realtime/stop")
        assert out["ok"] is True and out["stopped"] is True
        n_at_stop = polls["n"]
        time.sleep(1.2)
        assert polls["n"] == n_at_stop                   # 停止后不再轮询
        out = _post(server, "/api/realtime/state")
        assert out["ok"] is True and out["running"] is False
        out = _post(server, "/api/realtime/poll")        # 端点已随前端重构移除
        assert out["ok"] is False and "未知接口" in out["error"]
        # 实现：start 起后台线程（interval=1s）→ state 看到线程累计 → stop 置事件+join。
        # 解析：用户拍板「服务端命令行驱动、页面关闭不影响」——线程在黑窗口进程里跑，
        #       state 端点让重开的页面恢复运行状态显示。
        # 功能：实时后台生命周期协议。


class TestSetupMode:
    def test_setup_flow_over_http(self, tmp_path, monkeypatch):
        # 把"项目根"重定向到 tmp：App.setup 写 .env、load_config 读 .env 都落在 tmp 里，
        # 绝不触碰真实项目的 .env。
        monkeypatch.setattr("wxmsg.config.project_root", lambda: tmp_path)
        srv = ThreadingHTTPServer(("127.0.0.1", 0), panel._Handler)
        srv.daemon_threads = True
        srv.app = panel.App(None)                       # 未配置态
        threading.Thread(target=srv.serve_forever, daemon=True).start()
        try:
            html = urllib.request.urlopen(
                f"http://127.0.0.1:{srv.server_port}/", timeout=10).read().decode("utf-8")
            assert "首次配置" in html                    # 渲染的是 setup 页
            out = _post(srv, "/api/sessions")
            assert out["ok"] is False and out.get("need_setup") is True   # 未配置时业务接口全拒
            out = _post(srv, "/api/setup", {"dataRoot": str(tmp_path / "nope")})
            assert out["ok"] is False and "不存在" in out["error"]        # 坏路径给可操作文案
            (tmp_path / "dr").mkdir()
            out = _post(srv, "/api/setup", {"dataRoot": "dr"})            # 相对路径也支持
            assert out["ok"] is True
            assert (tmp_path / ".env").is_file()                          # .env 自动生成
            assert srv.app.cfg is not None                                # 配置已加载
            html2 = urllib.request.urlopen(
                f"http://127.0.0.1:{srv.server_port}/", timeout=10).read().decode("utf-8")
            assert "首次配置" not in html2                                 # reload 后是正常面板
        finally:
            srv.shutdown()
        # 实现：未配置 → setup 页 + 业务接口闸 + 坏路径报错 + 好路径写 .env 进正常面板。
        # 解析：project_root 重定向让 .env 读写都发生在 tmp（生产代码零改动、测试零污染）。
        # 功能：首次配置（面板里填数据根）的完整闭环。
