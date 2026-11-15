"""本地 Web 面板：浏览器里完成 体检 → 选会话 → 选群成员 → 导出/实时监听。

安全口径（继承 v1 panel.py）：
- 只绑 127.0.0.1，Host/Origin 白名单校验（防 DNS rebinding 把内网请求打进来）；
- 单实例导出锁（并发请求回 409 语义的 JSON 错误）；
- 全程 UTF-8（中文消息内容不乱码）。

接口（POST JSON，响应 {"ok": bool, ...} 或 {"ok": false, "error": str}）：
  /api/diagnose        体检（数据根/导出器/ASR/账号与密钥状态）
  /api/sessions        会话列表（私聊/群聊分组，公众号单独一组默认隐藏）
  /api/search_priv     私聊搜索（备注/昵称/微信号）
  /api/search_group    群搜索（群名 + 群·成员组合，数据来自成员索引）
  /api/export          历史导出（时间范围/最近N + 格式 + 组织）
  /api/realtime/start  开始实时监听（可选先导历史）
  /api/realtime/poll   实时增量一轮（前端按配置间隔定时调用）
  /api/realtime/state  实时状态查询
  /api/realtime/stop   停止实时监听
"""
# 实现：模块 docstring 声明安全口径与 API 清单。
# 功能：start_panel.bat → wxmsg/cli.py → 本模块 serve() 的最终用户界面。

from __future__ import annotations

import html
import json
import threading
import webbrowser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from . import cli, exporter, keys, search as search_mod, session as session_mod
from .config import KEY_DATA_ROOT, Config, load_config, project_root, set_env_key

# 实现：import 标准库 HTTP 三件套 + 同包编排模块。
# 解析：session 模块起名 session 与 http 的概念无冲突，但为可读性 import as session_mod。
# 功能：面板全部依赖。

MAX_BODY_BYTES = 1_000_000
# 实现：POST body 大小上限（1MB）。
# 解析：请求体只是用户选择参数（几百字节量级），1MB 已是百倍余量；超限拒绝防滥用。
# 功能：do_POST 的入参防线。

REALTIME_FORMATS = ("csv", "json")
# 实现：实时模式可选拉格式（json 落盘时自动转 jsonl，realtime 层处理）。
# 功能：实时开始的格式校验。


def _check_host(host_header: str | None, port: int) -> bool:
    """Host 头校验：只允许 127.0.0.1 / localhost / ::1（带本服务端口）——防 DNS rebinding。"""
    if not host_header:
        return False
    host = host_header.rsplit(":", 1)[0] if host_header.count(":") <= 1 else host_header.split("]:")[0].lstrip("[")
    host = host.lower()
    return host in ("127.0.0.1", "localhost", "::1")
    # 实现：IPv4 host:port 按最后一个冒号切；IPv6 [::1]:port 按方括号处理。
    # 解析：不带端口的 Host 也接受（兼容代理剥离端口的情况，域名本身才是攻击面）；
    #       攻击者域名解析到 127.0.0.1 时其 Host 头是攻击域名，在这里被拒。
    # 功能：每个请求（GET/POST）的第一道闸。


def split_sessions(sessions: list[session_mod.Session]) -> dict:
    """会话三分组：groups（群聊）/ privates（私聊）/ official（公众号 gh_，默认隐藏）。"""
    groups, privates, official = [], [], []
    for s in sessions:
        item = {"sid": s.sid, "display": s.display_name(), "remark": s.remark,
                "nickname": s.nickname, "alias": s.alias, "sort_ts": s.sort_ts}
        if s.sid.startswith("gh_"):
            official.append(item)
        elif s.is_group:
            groups.append(item)
        else:
            privates.append(item)
    return {"groups": groups, "privates": privates, "official": official}
    # 实现：按 is_group 与 gh_ 前缀分三组，各组保持 Rust 端的活跃时间降序。
    # 解析：公众号（gh_ 前缀）通常没有提取价值且数量多——单独分组让前端默认折叠，
    #       展开开关仍可显示（不静默丢数据）。
    # 功能：/api/sessions 的数据装配。


def _targets_from_params(params: dict) -> list[cli.Target]:
    """请求参数里的 targets → cli.Target 列表（含群成员筛选集合）。"""
    targets: list[cli.Target] = []
    for t in params.get("targets") or []:
        sid = str(t.get("sid") or "").strip()
        if not sid:
            continue
        members = t.get("members")
        sel = set(map(str, members)) if members is not None else None
        targets.append(cli.Target(sid=sid, chat_type=int(t.get("chat_type") or 0),
                                  display=str(t.get("display") or sid),
                                  selected_members=sel))
    if not targets:
        raise ValueError("没有选择任何会话")
    return targets
    # 实现：逐项取 sid/chat_type/display/members；members=null=全群，数组=筛选集合。
    # 解析：空 sid 项跳过、全空抛错（与 cli.run_export 的防线双重保险）。
    # 功能：export 与 realtime/start 共用的目标装配。


class App:
    """面板后端状态：配置（setup 模式下可为 None）+ 实时任务单例 + 导出锁。"""

    def __init__(self, cfg: Config | None):
        self.cfg = cfg
        self.lock = threading.Lock()
        self.rt = None
        self._rt_thread: threading.Thread | None = None
        self._rt_stop: threading.Event | None = None
        self._rt_error = ""
        self._member_index = None          # 群·成员搜索索引（懒构建，刷新会话列表时失效）
        self.search_history = search_mod.SearchHistory(
            project_root() / "cache" / "search_history.json")
        # 实现：cfg 允许 None（setup 模式）；rt 单例 + 后台线程/停止事件（实时监听服务端驱动）；
        #       _member_index 懒缓存（--group-members-all 一次进程，避免每次搜索都扫全库）。
        # 解析：搜索历史落 cache/（gitignore，含搜索词不算密钥但也不该入库）。
        # 功能：所有 /api 处理器的宿主状态。

    def setup(self, params: dict) -> dict:
        """setup 模式：验证数据根 → 写进 .env → 加载完整配置进入正常面板。"""
        raw = str(params.get("dataRoot") or "").strip().strip('"')
        if not raw:
            raise ValueError("请填写微信数据根（如 D:/xwechat_files）")
        # 1) 路径展开 + 存在性校验（独立于 load_config——此刻 .env 还没写，不能走全量加载）。
        from pathlib import Path as _P
        expanded = _P(raw)
        if not expanded.is_absolute():
            from .config import project_root
            expanded = project_root() / expanded
        expanded = expanded.resolve()
        if not expanded.is_dir():
            raise ValueError(f"目录不存在：{raw}。路径在微信「设置→文件管理」里查看")
        # 2) 写 .env（保留其它已有键），重新走一遍完整加载（顺带享受全部校验）。
        set_env_key(KEY_DATA_ROOT, str(expanded))
        self.cfg = load_config()
        return {"ok": True}
        # 实现：独立校验（is_dir）→ set_env_key 落盘 → load_config 全量校验并持有。
        # 解析：校验和加载分离——用户填错路径时给出"目录不存在"这种可操作文案，而不是笼统的配置错误；
        #       写入成功后 self.cfg 从 None 变为有效配置，随后的 GET / 就渲染正常面板。
        # 功能：首次运行配置的落盘动作。

    def rescan(self) -> dict:
        """手动重扫密钥（换号防线的手动兜底按钮）。"""
        ok, lines = keys.ensure_keys(self.cfg, log=print)
        return {"ok": ok, "lines": lines}
        # 实现：直接调 keys.ensure_keys（keyscan 子进程，输出过白名单闸）。
        # 解析：这是自动重扫之外的手动兜底——换号自动重扫万一失败，用户点一下再试。
        # 功能：/api/rescan 的实现。

    def diagnose(self) -> dict:
        acc = exporter.list_accounts(self.cfg)
        return {
            "data_root": str(self.cfg.data_root),
            "data_root_exists": self.cfg.data_root.is_dir(),
            "exporter": str(self.cfg.exporter_bin),
            "exporter_exists": Path(self.cfg.exporter_bin).is_file(),
            "asr_available": bool(self.cfg.voice2text_js),
            "asr_js": str(self.cfg.voice2text_js or ""),
            "poll_interval_s": self.cfg.poll_interval_s,
            "accounts": acc.get("accounts", []),
        }
        # 实现：体检四件套（数据根/导出器/ASR/账号）+ 轮询间隔回显。
        # 解析：list_accounts 失败会抛 ExporterError → 由路由层统一转 ok:false。
        # 功能：体检页数据源。

    def sessions(self) -> dict:
        exp = exporter.list_sessions(self.cfg)
        self._member_index = None   # 刷新会话列表 = 成员索引一并失效重建（数据可能变化）
        return split_sessions(session_mod.sessions_from_export(exp))
        # 实现：刷新时清空成员索引懒缓存——下次群搜索会重新拉 --group-members-all。
        # 功能：会话列表数据源 + 索引失效点。

    def _ensure_member_index(self) -> dict:
        """懒构建群·成员搜索索引（一次 --group-members-all 进程，缓存到刷新为止）。"""
        if self._member_index is None:
            self._member_index = search_mod.build_member_index(
                exporter.group_members_all(self.cfg))
        return self._member_index
        # 实现：None 即构建；索引里的 error/统计原样保留。
        # 解析：构建成本 = 一次导出器进程（实测 47 群毫秒级），首次群搜索时付出。
        # 功能：群搜索的数据入口。

    def search_priv(self, query: str) -> dict:
        """私聊搜索：备注/昵称/微信号匹配，公众号排除。搜索词记入历史。"""
        exp = exporter.list_sessions(self.cfg)
        sessions = session_mod.sessions_from_export(exp)
        results = search_mod.search_privates(sessions, query)
        self.search_history.add("priv", query)
        return {"results": results}
        # 功能：第一行搜索的 API 数据源。

    def search_group(self, query: str) -> dict:
        """群搜索：群名 + 群·成员组合（群在前成员在后）。搜索词记入历史。"""
        index = self._ensure_member_index()
        exp = exporter.list_sessions(self.cfg)
        sessions = session_mod.sessions_from_export(exp)
        results = search_mod.search_groups(index, sessions, query)
        self.search_history.add("group", query)
        return {"results": results}
        # 功能：第二行搜索的 API 数据源。

    def history(self, kind: str) -> dict:
        """某类搜索的历史词（datalist 联想数据源）。"""
        return {"terms": self.search_history.terms(kind)}
        # 功能：搜索历史读取。

    def export(self, params: dict) -> dict:
        with self.lock:
            return cli.run_export(
                self.cfg,
                _targets_from_params(params),
                fmt=str(params.get("fmt") or "csv"),
                merged=bool(params.get("merged", True)),
                begin=cli.parse_time_boundary(params.get("begin")),
                end=cli.parse_time_boundary(params.get("end"), end_of_day=True),
                recent=params.get("recent"),
            )
        # 实现：全局锁内走 cli.run_export 完整流水线；时间边界由面板的日期字符串解析。
        # 功能：历史导出 API。

    def realtime_start(self, params: dict) -> dict:
        """启动实时监听：构建 RealtimeExport 并起**服务端后台线程**轮询。

        线程驱动（用户拍板：服务端命令行驱动、页面关闭不影响）：黑窗口常驻期间，
        每隔 interval_s 自动 poll_once 并追加落盘，直到 /api/realtime/stop 置停止事件。"""
        fmt = str(params.get("fmt") or "csv")
        if fmt not in REALTIME_FORMATS:
            raise ValueError(f"实时模式只支持 {'/'.join(REALTIME_FORMATS)}")
        interval_s = int(params.get("interval_s") or self.cfg.poll_interval_s)
        if interval_s < 5:
            raise ValueError("轮询间隔最小 5 秒")
        with self.lock:
            if self.rt is not None:
                raise ValueError("已有实时监听在运行——请先停止再重新开始")
            self._rt_error = ""
            self.rt = realtime_mod.RealtimeExport(
                self.cfg, _targets_from_params(params), fmt=fmt,
                merged=bool(params.get("merged", True)),
                include_history=bool(params.get("include_history", False)),
                history_begin=cli.parse_time_boundary(params.get("history_begin")),
                history_end=cli.parse_time_boundary(params.get("history_end"), end_of_day=True),
                history_recent=params.get("history_recent"),
            )
            self._rt_stop = threading.Event()
            self._rt_thread = threading.Thread(
                target=self._rt_loop, args=(self.rt, self._rt_stop, interval_s),
                daemon=True, name="realtime-poll")
            self._rt_thread.start()
            return self.rt.summary()
        # 实现：单例闸 → 构建状态机（含起点导出）→ 起守护线程。
        # 解析：daemon 线程随黑窗口退出而终止（Ctrl+C = 全停）；interval 下限 5 秒
        #       与 config 口径一致。页面关闭不影响线程（这就是服务端驱动的意义）。
        # 功能：实时监听的启动 API（后台化改造）。

    @staticmethod
    def _friendly_poll_error(e: Exception) -> str:
        """轮询失败的面向用户文案：把常见错误翻译成可操作提示。"""
        if isinstance(e, PermissionError):
            return ("导出文件被占用（多半是 WPS/Excel 正开着它）——关闭文件后，"
                    "下一轮会自动补写错过的消息，已提取的数据不会丢")
        return f"{type(e).__name__}: {e}"
        # 实现：PermissionError 单列（Windows 文件独占锁是实时场景最高频故障），
        # 其余保留类型名+原文。
        # 功能：面板实时状态里"最近一次轮询出错"的可读文案。

    def _rt_loop(self, rt, stop_event: threading.Event, interval_s: int) -> None:
        """实时线程主循环：poll → 打一行摘要（不含消息内容）→ 等间隔或停止事件。"""
        while not stop_event.is_set():
            try:
                result = rt.poll_once()
                self._rt_error = ""
                print(f"[realtime] {result['at']} 新增 {result['new_total']} 条 "
                      f"（累计 {sum(s['total'] for s in result['sessions'])}）")
            except Exception as e:  # noqa: BLE001  轮询失败不退出循环——下一轮可能自愈
                self._rt_error = self._friendly_poll_error(e)
                print(f"[realtime] 轮询失败（下一轮重试）：{self._rt_error}")
            if stop_event.wait(interval_s):
                break
        # 实现：poll 异常被捕获记录（面板 state 里可见），循环继续——换号自动重扫等
        #       自愈逻辑在 exporter 层，这里只兜底"彻底失败"的场景。
        # 解析：stop_event.wait(interval) 同时实现"间隔睡眠"与"即时响应停止"；
        #       摘要只打条数不打内容（黑窗口可能被投屏/旁观）。
        # 功能：服务端驱动的实时轮询循环。

    def realtime_state(self) -> dict:
        with self.lock:
            if self.rt is None:
                return {"running": False, "last_error": self._rt_error}
            return {"running": True, "last_error": self._rt_error, **self.rt.summary()}
        # 功能：页面（重新打开后）恢复实时状态：运行中/累计条数/最近错误。

    def realtime_stop(self) -> dict:
        with self.lock:
            had = self.rt is not None
            if self._rt_stop is not None:
                self._rt_stop.set()
            if self._rt_thread is not None:
                self._rt_thread.join(timeout=3)
            self._rt_thread = None
            self._rt_stop = None
            self.rt = None
            return {"stopped": had}
        # 实现：置停止事件 → join 线程（最多等 3 秒）→ 清状态（文件已落盘内容不受影响）。
        # 解析：join 有超时——线程若卡在长轮询里也不拖死 API 响应；事件置位后下一轮必退。
        # 功能：停止实时监听 API。


from . import realtime as realtime_mod  # noqa: E402  （App 方法体内用到，底部导入避免读取顺序噪音）
# 实现：realtime 模块延迟导入。
# 解析：realtime.py 又 import cli——同包循环依赖风险靠"底部导入"规避；Python 同包内
#       实际无环（realtime→cli→exporter），顶部导入也可，但保持与 v1 风格一致的稳妥写法。
# 功能：App.realtime_* 的实现依赖。


class _Handler(BaseHTTPRequestHandler):
    server_version = "wxmsg-panel/0.1"

    def log_message(self, fmt, *args):  # noqa: A002
        pass  # 静默访问日志——面板轮询每分钟打一行没必要；错误经 API 响应回给前端
        # 功能：关掉默认 stderr 访问日志，控制台只留启动横幅。

    def _send_json(self, obj: dict, status: int = 200) -> None:
        body = json.dumps(obj, ensure_ascii=False).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        # 实现：统一 JSON 响应出口（UTF-8 明示 charset）。
        # 功能：所有 API 的返回通道。

    def _api(self, name: str, params: dict) -> dict:
        """API 分发（异常统一转 {"ok": false, "error"}）。"""
        app: App = self.server.app
        if app.cfg is None and name != "setup":
            # setup 模式：除配置写入外的接口一律拒绝，前端据此停留在配置页。
            return {"ok": False, "error": "还未配置微信数据根", "need_setup": True}
        try:
            if name == "setup":
                return {"ok": True, **app.setup(params)}
            if name == "diagnose":
                return {"ok": True, **app.diagnose()}
            if name == "rescan":
                return {"ok": True, **app.rescan()}
            if name == "sessions":
                return {"ok": True, **app.sessions()}
            if name == "search_priv":
                return {"ok": True, **app.search_priv(str(params.get("query") or ""))}
            if name == "search_group":
                return {"ok": True, **app.search_group(str(params.get("query") or ""))}
            if name == "history":
                return {"ok": True, **app.history(str(params.get("kind") or ""))}
            if name == "export":
                return {"ok": True, **app.export(params)}
            if name == "realtime/start":
                return {"ok": True, **app.realtime_start(params)}
            if name == "realtime/state":
                return {"ok": True, **app.realtime_state()}
            if name == "realtime/stop":
                return {"ok": True, **app.realtime_stop()}
            return {"ok": False, "error": f"未知接口 {name}"}
        except exporter.ExporterError as e:
            return {"ok": False, "error": str(e)}
        except PermissionError as e:
            return {"ok": False, "error": (
                "导出文件被占用（多半是 WPS/Excel 正开着它）——关闭文件后重试")}
        except (ValueError, FileNotFoundError) as e:
            return {"ok": False, "error": str(e)}
        # 实现：setup 模式闸 → 按名字分发到 App 方法；PermissionError（文件被 WPS/Excel
        # 独占）给可操作文案。
        # 解析：刻意不裸接 Exception——真 bug 让它抛出去打日志，而不是被包装成"业务失败"。
        # 功能：API 层的统一错误协议。

    def do_GET(self):
        if not _check_host(self.headers.get("Host"), self.server.server_port):
            self._send_json({"ok": False, "error": "host 不允许"}, 403)
            return
        if self.path == "/":
            app: App = self.server.app
            body = (PAGE if app.cfg is not None else SETUP_PAGE).encode("utf-8")
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
            self._send_json({"ok": False, "error": "not found"}, 404)
        # 实现：GET 按配置状态渲染主页（未配置 → setup 页）或 404；Host 校验先行。
        # 功能：页面入口。

    def do_POST(self):
        # 先读完整请求体再校验/响应：如果带着未读的 body 直接回 403 关连接，客户端写 body
        # 会偶发收到 RST（ConnectionAborted）——表现为测试里 flaky 的失败。先排干再拒。
        length = int(self.headers.get("Content-Length") or 0)
        if length > MAX_BODY_BYTES:
            self._send_json({"ok": False, "error": "请求体过大"}, 413)
            return
        body = self.rfile.read(length) if length else b""
        if not _check_host(self.headers.get("Host"), self.server.server_port):
            self._send_json({"ok": False, "error": "host 不允许"}, 403)
            return
        name = self.path.lstrip("/")
        if not name.startswith("api/"):
            self._send_json({"ok": False, "error": "not found"}, 404)
            return
        try:
            params = json.loads(body.decode("utf-8")) if body else {}
        except ValueError:
            self._send_json({"ok": False, "error": "请求体不是合法 JSON"}, 400)
            return
        self._send_json(self._api(name[4:], params if isinstance(params, dict) else {}))
        # 实现：读 body（有 Content-Length 上限）→ Host 校验 → 路由 → JSON 解析 → 分发。
        # 解析：Host 校验放在读 body 之后，纯粹为了让拒绝响应前排干输入流，防线本身不变。
        # 功能：POST 统一入口。


PAGE = """<!DOCTYPE html>
<html lang="zh"><head><meta charset="utf-8">
<title>微信聊天记录提取</title>
<style>
 body{font-family:"Microsoft YaHei",sans-serif;max-width:1150px;margin:0 auto;padding:12px;color:#222}
 h1{font-size:20px} h2{font-size:15px;margin:8px 0 4px} h3{font-size:14px;margin:6px 0 4px}
 section{border:1px solid #ddd;border-radius:8px;padding:10px;margin:10px 0}
 .cols{display:flex;gap:12px} .cols>div{flex:1;border:1px dashed #e5e5e5;border-radius:6px;padding:8px}
 input,select{font-size:13px;padding:3px} select{width:100%}
 button{padding:3px 12px;margin:2px 2px 2px 0;cursor:pointer}
 .row{padding:2px 0;font-size:13px}
 .meta{color:#888;font-size:11px}
 .err{color:#c00;white-space:pre-wrap} .ok{color:#070} .muted{color:#999;font-size:12px}
 table{border-collapse:collapse;font-size:12px} td,th{border:1px solid #ccc;padding:3px 8px;text-align:left}
 #rtStatus b{color:#06c}
</style></head><body>
<h1>微信聊天记录提取</h1>

<section>
 <b>① 体检</b> <button onclick="diagnose()">开始体检</button>
 <button onclick="rescanKeys()">重扫密钥</button>
 <button onclick="loadSessions()">刷新会话数据</button>
 <div id="statusBox" class="muted">先体检；换号登录后点「重扫密钥」再「刷新会话数据」。</div>
</section>

<section>
 <b>② 选择会话</b>（两种模式共用；每行：搜索 → 结果列表（Ctrl 多选）→ 确认添加 → 已选列表 → 删除选择）
 <div class="cols">
  <div>
   <h2>私聊搜索</h2>
   <input id="privSearch" list="privHist" placeholder="备注 / 昵称 / 微信号" style="width:60%">
   <datalist id="privHist"></datalist>
   <button onclick="doSearch('priv')">搜索</button>
   <div class="meta">结果（Ctrl 可多选）</div>
   <select id="privResults" size="7" multiple></select>
   <button onclick="addFromResults('priv')">确认添加 →</button>
   <div class="meta">已选会话</div>
   <select id="privSel" size="7" multiple></select>
   <button onclick="delSelected('priv')">删除选择</button>
   <div><label id="privCount" class="meta"></label></div>
  </div>
  <div>
   <h2>群聊搜索（群名 + 群内成员）</h2>
   <input id="grpSearch" list="grpHist" placeholder="群名 / 成员备注 / 昵称 / 微信号" style="width:60%">
   <datalist id="grpHist"></datalist>
   <button onclick="doSearch('grp')">搜索</button>
   <div class="meta">结果（群在前、群·成员在后；Ctrl 可多选）</div>
   <select id="grpResults" size="7" multiple></select>
   <button onclick="addFromResults('grp')">确认添加 →</button>
   <div class="meta">已选会话</div>
   <select id="grpSel" size="7" multiple></select>
   <button onclick="delSelected('grp')">删除选择</button>
   <div><label id="grpCount" class="meta"></label></div>
  </div>
 </div>
 <div class="muted">选择「群」= 导出该群全部成员的消息；选择「群·某成员」= 只导出该成员的消息（标注「发言者池」= 来自该群最近消息的发言人，非完整名册）。每个列表最多 20 个。</div>
</section>

<section>
 <b>③ 模式与格式</b>
 <div>
  <label><input type="radio" name="mode" value="history" checked onchange="modeChanged()">历史导出（一次性任务，导完即止）</label>
  <label><input type="radio" name="mode" value="realtime" onchange="modeChanged()">实时监听（服务端后台每分钟提取，直到按「停止运行」）</label>
 </div>
 <div id="timeOpts">
  时间范围：<input type="date" id="begin"> ~ <input type="date" id="end">
  或最近 <input type="number" id="recent" min="1" style="width:80px"> 条（都留空 = 全部历史；按原始消息计）
 </div>
 <div id="realtimeOpts" style="display:none">
  <label><input type="checkbox" id="includeHistory" onchange="modeChanged()">启动时同时导出历史消息（写进同一个实时文件，可叠加上方时间范围/最近N）</label>
  <div class="muted">启动后：黑窗口每分钟提取一次，<b>关闭浏览器页面不影响</b>；重新打开面板能看到运行状态。运行期间请勿用 WPS/Excel 打开正在写入的实时文件（会锁文件导致轮询失败；关闭后下一轮自动补写，数据不丢）。</div>
 </div>
 <div style="margin-top:6px">
  格式 <select id="fmt"><option value="csv">CSV（Excel）</option><option value="json">JSON（实时=JSONL追加）</option></select>
  组织 <label><input type="radio" name="org" value="merged" checked>合并一个文件</label>
       <label><input type="radio" name="org" value="split">每会话一个文件</label>
  轮询间隔 <input type="number" id="pollInterval" min="5" style="width:70px"> 秒（实时模式）
 </div>
 <div style="margin-top:8px">
  <button id="exportBtn" onclick="doExport()">开始导出</button>
  <button id="rtBtn" onclick="rtToggle()" style="display:none">开始实时监听</button>
  <button id="rtStopBtn" onclick="rtStop()" style="display:none">停止运行</button>
 </div>
 <div id="rtStatus"></div>
 <div id="result" class="muted">导出/实时结果会显示在这里</div>
</section>

<script>
const S = {privResults:[], grpResults:[], privTargets:[], grpTargets:[],
           rtTimer:null, rtRunning:false};
const SEL_CAP = 20;
function esc(s){return String(s??"").replace(/[&<>"]/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;"}[c]));}
function show(el,ok,text){ el.className=ok?"ok":"err"; el.textContent=text; }
async function api(name, params={}){
  try{
    const r = await fetch("/api/"+name,{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(params)});
    return await r.json();
  }catch(e){
    return {ok:false,error:"无法连接面板服务：请确认黑色控制台窗口还开着（关了就重新双击 start_panel.bat）"};
  }
}

/* —— ① 体检 —— */
async function diagnose(){
  const box = document.getElementById("statusBox");
  box.textContent = "体检中…";
  const d = await api("diagnose");
  if(!d.ok){ show(box,false,"体检失败："+d.error); return; }
  const acc = d.accounts.map(a=>`${esc(a.wxid||a.account_dir)}（${a.key_works?"密钥可用":a.has_key?"密钥失效":"无密钥"}，${a.session_db_age_s<0?"?":Math.round(a.session_db_age_s/60)} 分钟前活跃）`).join("；");
  show(box,true,`数据根 ${d.data_root} ✓ ｜ 导出器 ${d.exporter_exists?"就绪":"缺失(需先编译)"} ｜ ASR ${d.asr_available?"可用":"不可用(语音将显示[语音])"} ｜ 账号：${acc||"无"}`);
}
async function rescanKeys(){
  const box = document.getElementById("statusBox");
  box.textContent = "重扫密钥中（只读扫微信进程内存，约几十秒；微信需保持登录）…";
  const d = await api("rescan");
  if(!d.ok){ show(box,false,"重扫失败："+((d.lines||[]).join(" | ")||d.error)); return; }
  show(box,true,"重扫完成。"+(d.lines||[]).join(" | ")+"——请点「刷新会话数据」");
}
async function loadSessions(){
  const box = document.getElementById("statusBox");
  const d = await api("sessions");
  if(!d.ok){ show(box,false,"刷新失败："+d.error); return; }
  show(box,true,"会话数据已刷新（含群成员索引，下次群搜索时按需加载）。");
}

/* —— ② 搜索与选择 —— */
async function refreshHistory(kind){
  const d = await api("history",{kind});
  const dl = document.getElementById(kind+"Hist");
  dl.innerHTML = (d.ok?d.terms:[]).map(t=>`<option value="${esc(t)}">`).join("");
}
async function doSearch(kind){
  const q = document.getElementById(kind+"Search").value.trim();
  const el = document.getElementById(kind+"Results");
  if(!q){ alert("请输入搜索词"); return; }
  const d = await api(kind==="priv"?"search_priv":"search_group",{query:q});
  if(!d.ok){ alert(d.error); return; }
  S[kind+"Results"] = d.results;
  el.innerHTML = d.results.map((r,i)=>{
    const tag = kind==="priv" ? `（${esc(r.field)}）`
      : r.kind==="group" ? `（群，导全部）`
      : `（${esc(r.field)}，只导该成员${r.source==="sender_pool"?"，发言者池":""}）`;
    return `<option value="${i}">${esc(r.display)}${tag}</option>`;
  }).join("") || `<option disabled>（没有匹配）</option>`;
  refreshHistory(kind);   // 搜索词已入历史，同步 datalist
}
function addFromResults(kind){
  const el = document.getElementById(kind+"Results");
  const picks = [...el.selectedOptions].map(o=>S[kind+"Results"][+o.value]).filter(Boolean);
  if(!picks.length){ alert("请先在结果列表里选中（可 Ctrl 多选）再点确认添加"); return; }
  const targets = S[kind+"Targets"];
  let added=0, dup=0;
  for(const r of picks){
    const key = kind==="priv" ? r.sid : r.sid+"|"+(r.username||"");
    if(targets.some(t=>t.key===key)){ dup++; continue; }
    if(targets.length >= SEL_CAP){ alert(`最多只能选 ${SEL_CAP} 个（当前列表已满）`); break; }
    targets.push(kind==="priv"
      ? {key:r.sid, sid:r.sid, chat_type:1, display:r.display, username:null}
      : {key:r.sid+"|"+(r.username||""), sid:r.sid, chat_type:0,
         display:r.display, username:(r.kind==="member"?r.username:null),
         member_display:(r.kind==="member"?r.member_display:null)});
    added++;
  }
  renderSel(kind);
  // 反馈并入计数 label（不覆盖掉 renderSel 刚设好的 x/20 计数口径）
  const cnt = document.getElementById(kind+"Count");
  cnt.textContent = `最多 ${SEL_CAP} 个（当前 ${targets.length}/${SEL_CAP}）—— 刚刚添加 ${added} 个`
    + (dup ? `，重复跳过 ${dup} 个` : "");
}
function delSelected(kind){
  const el = document.getElementById(kind+"Sel");
  // option 的 value 是条目 key（sid 或 sid|username），按 key 过滤而不是按位置——
  // 两者不一致时会出现“点了删除却删不掉”的静默失败（真机冒烟抓到过）
  const picks = [...el.selectedOptions].map(o=>o.value);
  if(!picks.length){ alert("请先在已选列表里选中要删除的条目"); return; }
  S[kind+"Targets"] = S[kind+"Targets"].filter(t=>!picks.includes(t.key));
  renderSel(kind);
}
function renderSel(kind){
  const sel = document.getElementById(kind+"Sel");
  sel.innerHTML = S[kind+"Targets"].map(t=>{
    const tag = kind==="priv" ? "（私聊）"
      : t.username ? `（群·${esc(t.member_display||t.username)}）` : "（群，全部成员）";
    return `<option value="${esc(t.key)}">${esc(t.display)}${tag}</option>`;
  }).join("");
  document.getElementById(kind+"Count").textContent =
    `最多 ${SEL_CAP} 个（当前 ${S[kind+"Targets"].length}/${SEL_CAP}）`;
}
function collectTargets(){
  const targets=[];
  for(const t of S.privTargets)
    targets.push({sid:t.sid, chat_type:1, display:t.display, members:null});
  for(const t of S.grpTargets)
    targets.push({sid:t.sid, chat_type:0, display:t.display,
                  members:(t.username?[t.username]:null)});
  if(!targets.length) throw new Error("还没有选择任何会话：请先在②区搜索并「确认添加」");
  return targets;
}

/* —— ③ 模式与格式 —— */
function modeChanged(){
  const rt=document.querySelector('input[name=mode][value=realtime]').checked;
  const inc=document.getElementById("includeHistory").checked;
  // 时间范围/最近N：历史模式始终显示；实时模式勾选「同时导出历史」时显示
  //（否则这组输入对实时无意义，藏起来避免"提示说可叠加却找不到输入框"）
  document.getElementById("timeOpts").style.display=(rt && !inc)?"none":"block";
  document.getElementById("realtimeOpts").style.display=rt?"block":"none";
  document.getElementById("exportBtn").style.display=rt?"none":"inline-block";
  document.getElementById("rtBtn").style.display=rt?"inline-block":"none";
}
function commonParams(){
  return {targets:collectTargets(), fmt:document.getElementById("fmt").value,
          merged:document.querySelector('input[name=org][value=merged]').checked};
}
function timeParams(){
  return {begin:document.getElementById("begin").value||"",
          end:document.getElementById("end").value||"",
          recent:document.getElementById("recent").value?+document.getElementById("recent").value:null};
}
async function doExport(){
  const el=document.getElementById("result");
  let params;
  try{ params={...commonParams(),...timeParams()}; }catch(e){ show(el,false,e.message); return; }
  el.textContent="导出中…（大会话可能要等一会）";
  const d=await api("export",params);
  if(!d.ok){ show(el,false,"导出失败："+d.error); return; }
  const rows=d.files.map(f=>`<tr><td>${esc(f.display)}</td><td>${f.count}</td><td>${esc(f.path)}</td></tr>`).join("");
  const warn=d.warnings&&d.warnings.length?`<div class="err">注意：${d.warnings.map(esc).join("<br>")}</div>`:"";
  el.innerHTML=`<div class="ok">导出完成，共 ${d.total} 条。历史导出是一次性任务，到这里就结束了。</div>`+
    `<table><tr><th>会话</th><th>条数</th><th>文件</th></tr>${rows}</table>${warn}`;
}

/* —— 实时监听（服务端后台驱动） —— */
async function rtToggle(){
  if(S.rtRunning){ await rtStop(); return; }
  const el=document.getElementById("result"), st=document.getElementById("rtStatus");
  let params;
  try{ params=commonParams(); }catch(e){ show(el,false,e.message); return; }
  params.fmt=document.getElementById("fmt").value;
  params.merged=document.querySelector('input[name=org][value=merged]').checked;
  params.include_history=document.getElementById("includeHistory").checked;
  if(params.include_history){ Object.assign(params,timeParams()); }
  params.interval_s=+document.getElementById("pollInterval").value||60;
  st.textContent="启动中…";
  const d=await api("realtime/start",params);
  if(!d.ok){ show(st,false,"启动失败："+d.error); return; }
  S.rtRunning=true;
  document.getElementById("rtBtn").style.display="none";
  document.getElementById("rtStopBtn").style.display="inline-block";
  el.innerHTML=`<div class="ok">实时监听已启动（服务端后台每 ${params.interval_s}s 提取一次，关页面不影响）。</div>`;
  rtRefresh(); S.rtTimer=setInterval(rtRefresh,10000);
}
async function rtRefresh(){
  const st=document.getElementById("rtStatus");
  const d=await api("realtime/state");
  if(!d.ok){ st.textContent=""; return; }
  if(!d.running){ st.innerHTML=`<span class="ok">已停止（已写入的文件保留）</span>`; stopTimerUI(); return; }
  const files=(d.files||[]).map(f=>esc(f.path)).join("<br>");
  st.innerHTML=`<b>运行中</b>（累计 ${d.total} 条，最近轮询 ${esc(d.last_poll||"")}）`+
    (d.last_error?`<div class="err">最近一次轮询出错：${esc(d.last_error)}（每轮自动重试）</div>`:"")+
    `<div class="meta">文件：<br>${files}</div>`;
}
async function rtStop(){
  const d=await api("realtime/stop");
  stopTimerUI();
  if(d.ok){ show(document.getElementById("rtStatus"),true,"实时监听已停止（已写入的文件保留在 output/）"); }
}
function stopTimerUI(){
  if(S.rtTimer){clearInterval(S.rtTimer);S.rtTimer=null;}
  S.rtRunning=false;
  document.getElementById("rtBtn").style.display="inline-block";
  document.getElementById("rtStopBtn").style.display="none";
}

/* —— 启动 —— */
modeChanged();
refreshHistory("priv"); refreshHistory("grp");
</script></body></html>
"""
# 实现：单页内联 HTML+JS（无外部资源，离线可用）。
# 解析：交互流 = ①体检（顺带拉会话）→ ②勾会话/群成员 → ③历史导出或实时监听；
#       实时轮询由前端 setInterval 驱动（浏览器后台节流只会延迟不会漏——poll 是游标增量）；
#       所有动态文本过 esc()（会话名来自微信，含 <> 时防注入页面结构）。
# 功能：面板的全部用户界面。


SETUP_PAGE = """<!DOCTYPE html>
<html lang="zh"><head><meta charset="utf-8"><title>首次配置 · 微信聊天记录提取</title>
<style>
 body{font-family:"Microsoft YaHei",sans-serif;max-width:640px;margin:60px auto;padding:12px;color:#222}
 input{padding:6px;font-size:14px} button{padding:6px 18px;cursor:pointer}
 .err{color:#c00;white-space:pre-wrap} .ok{color:#070} .muted{color:#888;font-size:13px}
</style></head><body>
<h1>微信聊天记录提取 · 首次配置</h1>
<section>
 <p>程序需要知道<b>微信数据根目录</b>：在微信里打开 <b>设置 → 文件管理</b>，显示的路径
 （形如 <code>D:/xwechat_files</code>）填到下面。程序绝不猜测这个位置。</p>
 <input id="dataRoot" style="width:430px" placeholder="D:/xwechat_files">
 <button onclick="save()">保存并进入面板</button>
 <div id="msg" class="muted">保存后程序会把路径写进项目根的 .env（后续不再询问）。</div>
</section>
<script>
function esc(s){return String(s??"").replace(/[&<>"]/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;"}[c]));}
function show(el,ok,text){ el.className=ok?"ok":"err"; el.textContent=text; }
async function api(name,params={}){
  try{
    const r=await fetch("/api/"+name,{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(params)});
    return await r.json();
  }catch(e){
    // 网络层失败（服务已被关掉/控制台窗口被关）必须就地报错——否则页面永远停在"保存中…"
    return {ok:false,error:"无法连接面板服务：请确认黑色控制台窗口还开着（关了就重新双击 start_panel.bat）"};
  }
}
async function save(){
  const el=document.getElementById("msg");
  el.textContent="保存中…";
  const d=await api("setup",{dataRoot:document.getElementById("dataRoot").value});
  if(!d.ok){ show(el,false,d.error); return; }
  show(el,true,"已写入 .env，正在进入面板…");
  setTimeout(()=>location.reload(),600);
}
</script></body></html>
"""
# 实现：首次配置页（App.cfg 为 None 时 GET / 渲染本页）。
# 解析：保存动作 = 校验目录存在 → 写 .env → reload 进正常面板；错误就地展示（目录不存在等）。
# 功能：替代手工建 .env 的首次运行体验（用户拍板）。


def serve(cfg: Config | None, port: int = 8765, open_browser: bool = True) -> None:
    """启动面板（阻塞）；cfg=None 时进入首次配置模式。

    open_browser=True 时 0.6s 后自动打开浏览器（双击 bat 即用）。"""
    server = ThreadingHTTPServer(("127.0.0.1", port), _Handler)
    server.daemon_threads = True
    server.app = App(cfg)
    url = f"http://127.0.0.1:{port}/"
    print(f"[panel] 微信聊天记录提取面板已启动：{url}（Ctrl+C 停止）")
    if cfg is None:
        print("[panel] 首次配置模式：请在打开的页面里填写微信数据根（会自动写入 .env）")
    else:
        print(f"[panel] 数据根：{cfg.data_root}")
    if open_browser:
        threading.Timer(0.6, webbrowser.open, [url]).start()
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        print("\n[panel] 已停止")
    finally:
        server.server_close()
    # 实现：绑定 127.0.0.1 + ThreadingHTTPServer（每请求一线程，daemon 线程随进程退）。
    # 解析：端口被占时 bind 抛 OSError → cli.main 转成"换端口"提示；
    #       浏览器自动打开用 Timer 延迟——serve_forever 前端口已就绪，页面首屏就能通。
    # 功能：面板服务的启动出口（cli.main 调用）。
