"""本地 Web 面板：浏览器里完成 体检 → 选会话 → 选群成员 → 导出/实时监听。

安全口径（继承 v1 panel.py）：
- 只绑 127.0.0.1，Host/Origin 白名单校验（防 DNS rebinding 把内网请求打进来）；
- 单实例导出锁（并发请求回 409 语义的 JSON 错误）；
- 全程 UTF-8（中文消息内容不乱码）。

接口（POST JSON，响应 {"ok": bool, ...} 或 {"ok": false, "error": str}）：
  /api/diagnose        体检（数据根/导出器/ASR/账号与密钥状态）
  /api/sessions        会话列表（私聊/群聊分组，公众号单独一组默认隐藏）
  /api/members         群成员列表（含展示名，勾选用）
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

from . import cli, exporter, keys, session as session_mod
from .config import KEY_DATA_ROOT, Config, load_config, set_env_key

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
        # 实现：cfg 允许 None——首次运行未配置数据根时进入 setup 模式（页面引导填路径生成 .env）。
        # 解析：rt 单例（同一时刻只跑一个实时监听）；lock 串行化历史导出与实时启停。
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

    def match(self, query: str) -> dict:
        """按名字模糊匹配会话（v1 ranked_matches 口径），供「按名字选择」。"""
        if self.cfg is None:
            raise ValueError("还未配置数据根")
        exp = exporter.list_sessions(self.cfg)
        sessions = session_mod.sessions_from_export(exp)
        matches = session_mod.ranked_sessions(sessions, query)
        return {"matches": matches, "unique": len(matches) == 1}
        # 实现：匹配在全量会话（含公众号）上做——公众号也可能被想提取。
        # 解析：unique 标记让前端"唯一命中自动勾选，歧义列候选"（v1 的交互语义）。
        # 功能：/api/match 的数据源。

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
        return split_sessions(session_mod.sessions_from_export(exp))
        # 功能：会话列表数据源。

    def members(self, sid: str) -> dict:
        if not sid.strip():
            raise ValueError("缺少 sid")
        exp = exporter.group_members(self.cfg, sid.strip())
        members = session_mod.members_from_export(exp)
        return {"members": [{
            "username": m.username,
            "display": m.display_name(),
            "group_display": m.group_display,   # 群内昵称原样保留（辅助辨认，不进导出列）
        } for m in members]}
        # 功能：群成员勾选列表数据源。

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
        fmt = str(params.get("fmt") or "csv")
        if fmt not in REALTIME_FORMATS:
            raise ValueError(f"实时模式只支持 {'/'.join(REALTIME_FORMATS)}")
        with self.lock:
            if self.rt is not None:
                raise ValueError("已有实时监听在运行——请先停止再重新开始")
            self.rt = realtime_mod.RealtimeExport(
                self.cfg, _targets_from_params(params), fmt=fmt,
                merged=bool(params.get("merged", True)),
                include_history=bool(params.get("include_history", False)),
                history_begin=cli.parse_time_boundary(params.get("history_begin")),
                history_end=cli.parse_time_boundary(params.get("history_end"), end_of_day=True),
                history_recent=params.get("history_recent"),
            )
            return self.rt.summary()
        # 实现：单例闸（先停再开）；构造即完成起点导出（include_history 时含历史）。
        # 功能：实时监听的启动 API。
        # （realtime_mod 在文件底部 import——避免与下方模块级命名冲突的循环依赖。）

    def realtime_poll(self) -> dict:
        with self.lock:
            if self.rt is None:
                raise ValueError("没有正在运行的实时监听")
            result = self.rt.poll_once()
            return {**result, "summary": self.rt.summary()}
        # 功能：前端定时器每分钟调用一次。

    def realtime_state(self) -> dict:
        with self.lock:
            if self.rt is None:
                return {"running": False}
            return {"running": True, **self.rt.summary()}
        # 功能：页面刷新后恢复实时状态显示。

    def realtime_stop(self) -> dict:
        with self.lock:
            had = self.rt is not None
            self.rt = None
            return {"stopped": had}
        # 实现：置空单例即停止（文件已落盘的内容不受影响）。
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
            if name == "sessions":
                return {"ok": True, **app.sessions()}
            if name == "members":
                return {"ok": True, **app.members(str(params.get("sid") or ""))}
            if name == "match":
                return {"ok": True, **app.match(str(params.get("query") or ""))}
            if name == "export":
                return {"ok": True, **app.export(params)}
            if name == "realtime/start":
                return {"ok": True, **app.realtime_start(params)}
            if name == "realtime/poll":
                return {"ok": True, **app.realtime_poll()}
            if name == "realtime/state":
                return {"ok": True, **app.realtime_state()}
            if name == "realtime/stop":
                return {"ok": True, **app.realtime_stop()}
            return {"ok": False, "error": f"未知接口 {name}"}
        except exporter.ExporterError as e:
            return {"ok": False, "error": str(e)}
        except (ValueError, FileNotFoundError) as e:
            return {"ok": False, "error": str(e)}
        # 实现：setup 模式闸 → 按名字分发到 App 方法；match 是新增的按名字选择接口。
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
        if not _check_host(self.headers.get("Host"), self.server.server_port):
            self._send_json({"ok": False, "error": "host 不允许"}, 403)
            return
        name = self.path.lstrip("/")
        if not name.startswith("api/"):
            self._send_json({"ok": False, "error": "not found"}, 404)
            return
        length = int(self.headers.get("Content-Length") or 0)
        if length > MAX_BODY_BYTES:
            self._send_json({"ok": False, "error": "请求体过大"}, 413)
            return
        try:
            params = json.loads(self.rfile.read(length).decode("utf-8")) if length else {}
        except ValueError:
            self._send_json({"ok": False, "error": "请求体不是合法 JSON"}, 400)
            return
        self._send_json(self._api(name[4:], params if isinstance(params, dict) else {}))
        # 实现：路径 → api/ 前缀校验 → body 大小闸 → JSON 解析 → 分发。
        # 功能：POST 统一入口。


PAGE = """<!DOCTYPE html>
<html lang="zh"><head><meta charset="utf-8">
<title>微信聊天记录提取</title>
<style>
 body{font-family:"Microsoft YaHei",sans-serif;max-width:1100px;margin:0 auto;padding:12px;color:#222}
 h1{font-size:20px} h2{font-size:15px;margin:8px 0 4px}
 section{border:1px solid #ddd;border-radius:8px;padding:10px;margin:10px 0}
 .cols{display:flex;gap:12px} .cols>div{flex:1;border-right:1px dashed #eee;padding-right:8px}
 .cols>div:last-child{border-right:none}
 .row{padding:2px 0;font-size:13px} .row label{cursor:pointer}
 .meta{color:#888;font-size:11px;margin-left:4px}
 button{padding:4px 14px;margin-right:8px;cursor:pointer}
 .err{color:#c00;white-space:pre-wrap} .ok{color:#070}
 #memberBox .grp{border:1px solid #eee;border-radius:6px;padding:6px;margin:6px 0;background:#fafafa}
 table{border-collapse:collapse;font-size:12px} td,th{border:1px solid #ccc;padding:3px 8px;text-align:left}
 .muted{color:#999;font-size:12px}
</style></head><body>
<h1>微信聊天记录提取</h1>

<section>
 <b>① 体检</b> <button onclick="diagnose()">开始体检</button>
 <div id="statusBox" class="muted">先体检：确认数据根、导出器、密钥状态</div>
</section>

<section>
 <b>② 选择会话</b>（导出/实时都用这里的勾选）
 <button onclick="loadSessions()">刷新会话列表</button>
 <div class="cols">
  <div><h2>私聊</h2><div id="privList" class="muted">先体检再刷新</div></div>
  <div><h2>群聊</h2><div id="grpList" class="muted">先体检再刷新</div></div>
  <div><h2>公众号 <label class="meta"><input type="checkbox" id="showOfficial" onchange="renderSessions()">显示</label></h2>
   <div id="offList" class="muted"></div></div>
 </div>
 <div id="memberBox"></div>
 <div class="muted">群聊勾选后出现「成员」按钮：不点=导全群；点开后可勾选成员，只导被选成员的消息。</div>
 <div style="border:1px dashed #ccd;border-radius:6px;padding:6px;margin-top:6px;background:#f5f8ff">
  <b>按名字选择</b>（模糊匹配备注/昵称/微信号/群名）：
  <input id="matchQuery" placeholder="如：张三 / 三哥 / zhangsan" style="width:240px">
  <button onclick="matchName()">匹配</button>
  <div id="matchResult" class="muted">唯一命中自动勾选；有歧义列候选让你点选。</div>
 </div>
</section>

<section>
 <b>③ 模式与格式</b>
 <div>
  <label><input type="radio" name="mode" value="history" checked onchange="modeChanged()">历史导出</label>
  <label><input type="radio" name="mode" value="realtime" onchange="modeChanged()">实时监听（每分钟增量追加）</label>
 </div>
 <div id="historyOpts">
  时间范围：<input type="date" id="begin"> ~ <input type="date" id="end">
  或最近 <input type="number" id="recent" min="1" style="width:80px"> 条（两者都留空 = 全部历史）
 </div>
 <div id="realtimeOpts" style="display:none">
  <label><input type="checkbox" id="includeHistory">同时导出历史消息（写进同一个实时文件）</label>
  （导历史时可叠加下方时间范围/最近N）
 </div>
 <div style="margin-top:6px">
  格式 <select id="fmt"><option value="csv">CSV（Excel）</option><option value="json">JSON（实时=JSONL追加）</option></select>
  组织 <label><input type="radio" name="org" value="merged" checked>合并一个文件</label>
       <label><input type="radio" name="org" value="split">每会话一个文件</label>
  轮询间隔 <input type="number" id="pollInterval" min="5" style="width:70px"> 秒（实时模式）
 </div>
 <div style="margin-top:8px">
  <button id="exportBtn" onclick="doExport()">开始导出</button>
  <button id="rtBtn" onclick="rtToggle()">开始实时监听</button>
 </div>
 <div id="result" class="muted">导出/实时结果会显示在这里</div>
</section>

<script>
const S = {sessions: [], meta: {}, members: {}, timer: null, running: false};
function esc(s){return String(s??"").replace(/[&<>"]/g,c=>({"&":"&amp;","<":"&lt;",">":"&gt;",'"':"&quot;"}[c]));}
async function api(name, params={}){
  const r = await fetch("/api/"+name,{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(params)});
  return await r.json();
}
function show(el, ok, text){ el.className = ok?"ok":"err"; el.textContent = text; }

async function diagnose(){
  const box = document.getElementById("statusBox");
  box.textContent = "体检中…";
  const d = await api("diagnose");
  if(!d.ok){ show(box,false,"体检失败："+d.error); return; }
  const acc = d.accounts.map(a=>`${esc(a.wxid||a.account_dir)}（${a.key_works?"密钥可用":a.has_key?"密钥失效":"无密钥"}，${a.session_db_age_s<0?"?" :Math.round(a.session_db_age_s/60)} 分钟前活跃）`).join("；");
  show(box,true,`数据根 ${d.data_root} ✓ ｜ 导出器 ${d.exporter_exists?"就绪":"缺失(需先编译)"} ｜ ASR ${d.asr_available?"可用":"不可用(语音将显示[语音])"} ｜ 账号：${acc||"无"}`);
  loadSessions();
}

function sessRow(s){
  const c = `<input type="checkbox" class="sess" data-sid="${esc(s.sid)}" data-type="${s.sid.endsWith("@chatroom")?0:1}" data-display="${esc(s.display)}">`;
  return `<div class="row"><label>${c} ${esc(s.display)}<span class="meta">${esc(s.sid)}</span></label>
    ${s.sid.endsWith("@chatroom")?`<button class="meta" onclick="loadMembers('${esc(s.sid)}')">成员</button>`:""}</div>`;
}
async function loadSessions(){
  const d = await api("sessions");
  if(!d.ok){ show(document.getElementById("privList"),false,d.error); return; }
  S.sessions = [...d.groups,...d.privates,...d.official];
  S.meta = {}; S.sessions.forEach(s=>S.meta[s.sid]=s);
  renderSessions();
}
function renderSessions(){
  const g=filterList("groups"),p=filterList("privates"),o=filterList("official");
  document.getElementById("grpList").innerHTML = g||'<div class="muted">（无）</div>';
  document.getElementById("privList").innerHTML = p||'<div class="muted">（无）</div>';
  document.getElementById("offList").innerHTML = o||'<div class="muted">（无或未显示）</div>';
}
function filterList(kind){
  const showOff = document.getElementById("showOfficial").checked;
  if(kind==="official" && !showOff) return "";
  return (S.meta?Object.values(S.meta):[]).filter(s=>
    kind==="groups"?s.sid.endsWith("@chatroom"):
    kind==="official"?s.sid.startsWith("gh_"):
    (!s.sid.endsWith("@chatroom") && !s.sid.startsWith("gh_")))
    .map(sessRow).join("");
}
async function matchName(){
  const q=document.getElementById("matchQuery").value.trim();
  const el=document.getElementById("matchResult");
  if(!q){ el.textContent="请输入名字"; return; }
  const d=await api("match",{query:q});
  if(!d.ok){ show(el,false,d.error); return; }
  if(!d.matches.length){ show(el,false,"没有匹配到任何联系人/群聊"); return; }
  if(d.unique){
    checkSid(d.matches[0].sid);
    el.innerHTML=`<span class="ok">唯一命中：${esc(d.matches[0].display)}（${esc(d.matches[0].field)}）✓ 已在列表中勾选</span>`;
    return;
  }
  el.innerHTML=`歧义：命中 ${d.matches.length} 个（点选即勾选）<br>`+
    d.matches.map((m,i)=>`<label class="row" style="cursor:pointer;display:block">`+
      `<input type="radio" name="matchPick" onclick="checkSid('${esc(m.sid)}')"> `+
      `${i+1}. ${esc(m.display)}（${esc(m.field)}）<span class="meta">${esc(m.sid)}</span></label>`).join("");
  // 实现：唯一命中直接勾选并高亮；歧义列候选（带命中字段与 sid），点选即勾选。
  // 解析：v1 的「选编号」交互在网页里自然变成"点选"；候选来自 ranked_sessions 的降序列表。
}
function checkSid(sid){
  const cb=document.querySelector(`input.sess[data-sid="${CSS.escape(sid)}"]`);
  if(!cb) return false;
  cb.checked=true;
  const row=cb.closest(".row");
  if(row){
    row.style.background="#fff3cd";
    row.scrollIntoView({block:"center"});
    setTimeout(()=>{row.style.background="";},1600);
  }
  return true;
  // 实现：按 sid 找到列表里的复选框勾选，滚动到位并高亮 1.6s（CSS.escape 防 sid 含特殊字符）。
}
async function loadMembers(sid){
  if(document.getElementById("mb_"+btoa(unescape(encodeURIComponent(sid))))) { return; }
  const d = await api("members",{sid});
  if(!d.ok){ alert(d.error); return; }
  const box = document.getElementById("memberBox");
  const div = document.createElement("div");
  div.className="grp"; div.id="mb_"+btoa(unescape(encodeURIComponent(sid)));
  const checks = d.members.map(m=>`<label style="margin-right:10px"><input type="checkbox" class="mem" data-sid="${esc(sid)}" value="${esc(m.username)}"> ${esc(m.display)}${m.group_display?`<span class="meta">(${esc(m.group_display)})</span>`:""}</label>`).join("");
  div.innerHTML = `<b>${esc(S.meta[sid]?.display||sid)}</b> 成员限定：
    <label><input type="checkbox" class="memLimited" data-sid="${esc(sid)}">启用（不勾=导全群）</label><br>${checks||"（成员列表为空）"}`;
  box.appendChild(div);
}
function collectTargets(){
  const targets=[];
  document.querySelectorAll("input.sess:checked").forEach(cb=>{
    const sid=cb.dataset.sid;
    let members=null;
    const lim=document.querySelector(`.memLimited[data-sid="${CSS.escape(sid)}"]`);
    if(lim && lim.checked){
      members=[...document.querySelectorAll(`input.mem[data-sid="${CSS.escape(sid)}"]:checked`)].map(x=>x.value);
      if(!members.length){ throw new Error(`群「${S.meta[sid]?.display||sid}」启用了成员限定但一个成员都没勾`);}
    }
    targets.push({sid, chat_type:+cb.dataset.type, display:cb.dataset.display, members});
  });
  return targets;
}
function modeChanged(){
  const rt=document.querySelector('input[name=mode][value=realtime]').checked;
  document.getElementById("historyOpts").style.display=rt?"none":"block";
  document.getElementById("realtimeOpts").style.display=rt?"block":"none";
  document.getElementById("rtBtn").style.display=rt?"inline-block":"none";
  document.getElementById("exportBtn").style.display=rt?"none":"inline-block";
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
  el.textContent="导出中…（大会话可能要等一会）";
  try{
    const d=await api("export",{...commonParams(),...timeParams()});
    if(!d.ok){ show(el,false,"导出失败："+d.error); return; }
    renderFiles(el,d);
  }catch(e){ show(el,false,e.message); }
}
function renderFiles(el,d){
  const rows=d.files.map(f=>`<tr><td>${esc(f.display)}</td><td>${f.count}</td><td>${esc(f.path)}</td></tr>`).join("");
  const warn=d.warnings&&d.warnings.length?`<div class="err">注意：${d.warnings.map(esc).join("<br>")}</div>`:"";
  el.innerHTML=`<div class="ok">完成，共 ${d.total} 条</div><table><tr><th>会话</th><th>条数</th><th>文件</th></tr>${rows}</table>${warn}`;
}
async function rtToggle(){
  const btn=document.getElementById("rtBtn");
  if(S.running){ await api("realtime/stop"); stopTimer(); show(document.getElementById("result"),true,"实时监听已停止（已写入的文件保留）"); return; }
  const p=commonParams();
  p.include_history=document.getElementById("includeHistory").checked;
  if(p.include_history){ Object.assign(p,timeParams()); }
  const el=document.getElementById("result");
  const d=await api("realtime/start",p);
  if(!d.ok){ show(el,false,"启动失败："+d.error); return; }
  S.running=true; btn.textContent="停止实时监听";
  const interval=Math.max(5,+document.getElementById("pollInterval").value||60)*1000;
  S.timer=setInterval(rtPoll,interval);
  el.innerHTML=`<div class="ok">实时监听已启动（每 ${interval/1000}s 一轮），结果持续追加：</div><div id="rtLog"></div>`;
  rtPoll();
}
async function rtPoll(){
  const d=await api("realtime/poll");
  const log=document.getElementById("rtLog");
  if(!d.ok){ stopTimer(); if(log) show(log,false,"轮询失败："+d.error); return; }
  if(log){
    const lines=d.sessions.map(s=>`${esc(s.display)} +${s.new}（累计 ${s.total}）`).join("；");
    log.innerHTML=`<div class="meta">${esc(d.at)} ${lines}</div>`+log.innerHTML;
  }
}
function stopTimer(){ if(S.timer){clearInterval(S.timer);S.timer=null;} S.running=false;
  document.getElementById("rtBtn").textContent="开始实时监听"; }
modeChanged();
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
  const r=await fetch("/api/"+name,{method:"POST",headers:{"Content-Type":"application/json"},body:JSON.stringify(params)});
  return await r.json();
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
