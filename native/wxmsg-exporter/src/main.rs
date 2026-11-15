//! wxmsg-exporter —— 微信聊天记录提取软件 · 只读导出器（参照 v1 reply-exporter 的架构）。
//!
//! 职责边界：本 exe 只做「解密查询 + 原始数据 JSON 输出」，名称回退、占位符渲染、
//! CSV/JSON 落盘、实时轮询全部在 Python 侧（wxmsg/ 包）做，保持导出器薄而稳定。
//!
//! 用法（stdout=JSON 数据，stderr=诊断；退出码 0=成功、1=失败、2=用法错误）：
//!   wxmsg-exporter --data-root <微信数据根> --list-accounts
//!   wxmsg-exporter --data-root <微信数据根> --list-sessions [--account <账号目录名|wxid>]
//!   wxmsg-exporter ... --group-members --sid <群id>
//!   wxmsg-exporter ... --group-members-all        （全群成员索引：roster+发言者池回退）
//!   wxmsg-exporter ... --messages --sid <会话id> [--begin 秒] [--end 秒] [--recent N]
//!                      [--voice-out <语音落盘目录>]
//!   wxmsg-exporter --version
//! 可选 --store <store.json 路径>：覆盖密钥库位置（默认走 WEFLOW_KEY_STORE 或
//! %APPDATA%\weflow-key-provider\store.json，与 keyscan_readonly.py 的产出一致）。
//!
//! 全程只读：只 SELECT/PRAGMA，不写微信数据目录，不注入、不要 admin。
//! 密钥口径：Account::open 内部按 keystore 每库表解析密钥（微信 4.x 每库一钥），
//! 主 session 密钥只作回退；密钥绝不进 stdout/stderr（错误串已按此口径构造）。

use anyhow::{bail, Result};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use wcdb::contacts::resolve_group_members;
use wcdb::media::voice_in_table;
// 注意：wcdb-native 包的 [lib] name 是 "wcdb"（v1 给 Node/koffi 用的 DLL 名），
// 所以 Rust 导入名是 wcdb 而不是 wcdb_native——Cargo.toml 里依赖键仍是 wcdb-native。
// contacts::resolve_group_members：群成员双模型内省；media::voice_in_table：单张 voice 表内
// 按定位键取 silk blob 的 hex（逐库逐表的循环在本文件 voice_silk_bytes 里包装）。
use weflow_core::db::{list_tables, table_columns, Account};
use weflow_core::keystore::KeyStore;
use weflow_core::messages::{session_message_rows_recent_routed, stream_session_message_rows};
use weflow_core::schema::{find_candidate, sanitize_ident};
// 实现：依赖口径——anyhow 统一错误类型（main 返回 Result，Err 即 stderr + 退出码 1）；
// serde_json 的 json!/Map/Value 承载全部结构化输出；HashMap 做人名词表与 Name2Id 映射；
// Arc 让账号选择函数直接交付已打开的 Account，避免"探测一次 + 正式再开一次"的重复验钥。
// wcdb_native::contacts::resolve_group_members：群成员双模型内省（v1 wcdb 原实现，rlib 化后直调）。
// weflow_core::messages：时间窗流式读（stream_session_message_rows）与最近 N 条（*_recent_routed）。
// 功能：与 v1 reply-exporter 相同的依赖形状，保证行为可对照。

// ---- 跨版本列名适配层（自 v1 reply-exporter/src/main.rs:35-50 原样移植）----
// 四组候选列名常量 + find_candidate 构成「运行时内省 + 多候选列名」策略，微信改列名时只需往数组加别名。

const USER_COLS: &[&str] = &[
    "username", "user_name", "userName", "usrName", "UsrName", "name", "str_name",
    "strUsrName", "talker", "str_username",
];
// USER_COLS：用户名列候选别名，按优先级排列；覆盖下划线式/驼峰式/首字母大写式/str_ 前缀式/talker 等命名差异。
// 功能：让会话表/联系人表的用户名列探测不依赖硬编码单一列名。
const DISP_COLS: &[&str] = &[
    "nick_name", "nickName", "nickname", "NickName", "str_nick_name", "strNickName",
    "display_name", "displayName",
];
// DISP_COLS：展示名（昵称）列候选。会话表与联系人表的昵称列命名不同，所以候选集比 USER_COLS 更宽。
const REMARK_COLS: &[&str] = &["con_remark", "conRemark", "remark", "Remark", "str_remark"];
const ALIAS_COLS: &[&str] = &["alias", "Alias"];
// REMARK_COLS（备注）与 ALIAS_COLS（微信号）候选集。备注只有联系人库才有，微信号是用户自定义对外 id。
// 功能：四组常量一起构成跨版本列名适配层。

/// 会话表的排序时间戳列候选（自 v1 wcdb-native sessions.rs:18-19 的候选口径移植）：
/// sort_timestamp 优先（微信 4.x 主会话表的活跃排序列），last_timestamp/create_time 兜底。
const SORT_TS_COLS: &[&str] = &[
    "sort_timestamp", "sortTimestamp", "sort_time", "sortTime", "ntimestamp",
    "update_time", "order_time", "last_timestamp", "lastTimestamp", "last_msg_time",
    "lastMsgTime", "create_time", "createTime",
];
// 实现：把 wcdb sessions.rs 的 C_SORT_TS 与 C_LAST_TS 两组合并成一组按优先级排列的候选。
// 解析：--list-sessions 用它给会话列表带出活跃时间并 DESC 排序，面板才能"最近的会话排前面"；
//       全部落空则不排序（行序交由库），Python 侧按名称排序兜底。
// 功能：定位会话表的活跃时间列。

// 说明：NameInfo.username 字段与 best() 方法只被单元测试与内部诊断消费——本项目导出器
// 统一输出原始名称字段（展示回退在 Python 侧），best() 保留是给将来可能的诊断子命令用。
#[allow(dead_code)]
#[derive(Default, Clone)]
struct NameInfo {
    username: String,
    nickname: String,
    remark: String,
    alias: String,
    session_display: String,
}
// NameInfo：一个人的五种名字（自 v1 原样移植）。username 原始 id，nickname 昵称，remark 备注，
// alias 微信号，session_display 会话表显示名（群名通常只在这里有）。
// 功能：承载 load_names 的合并结果，供 --list-sessions 输出四个名称字段。

impl NameInfo {
    #[allow(dead_code)] // 仅测试/未来诊断消费，见上方说明
    fn best(&self) -> &str {
        for s in [&self.remark, &self.nickname, &self.session_display, &self.alias] {
            if !s.trim().is_empty() {
                return s;
            }
        }
        &self.username
    }
    // best()：按「备注 > 昵称 > 会话显示名 > 微信号」返回第一个非空值，全空兜底 username
    // （自 v1 原样移植——注意本项目 Python 侧的回退口径是 备注>昵称>微信号，两边不混用：
    // 导出器只交原始字段，展示回退由 Python session.py 统一实现）。
    // 功能：v1 内部诊断展示用；--list-sessions 输出的是原始字段而非 best()。
}

fn ci_get<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    m.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}
// ci_get（自 v1 原样移植）：在 serde_json Map 上忽略 ASCII 大小写找键，返回值的借用。
// 功能：屏蔽微信列名大小写差异。

fn as_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}
// as_str（自 v1 原样移植）：Option<&Value> → String；String 克隆、Number to_string、其余空串。
// 功能：安全取文本字段，绝不 panic。

fn as_i64(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}
// as_i64（自 v1 原样移植）：Number as_i64（浮点/超范围回退 0）、String trim+parse（失败 0）、其它 0。
// 功能：安全取时间戳/类型等整数字段；失败静默 0 由上层按"0=不参与"语义消费。

fn is_session_db(db: &Path) -> bool {
    db.file_stem().map(|s| s.eq_ignore_ascii_case("session")).unwrap_or(false)
}
// is_session_db（自 v1 原样移植）：按 file_stem 忽略大小写判是否 session 库。
// 功能：load_names 据此决定展示名写进 session_display 还是 nickname。

/// 通用查询（自 v1 原样移植）：给定表 + 一组列名，返回每行 Option<String>。
fn query_rows(account: &Account, db: &Path, table: &str, cols: &[String]) -> Result<Vec<Vec<Option<String>>>> {
    let col_list = cols
        .iter()
        .map(|c| sanitize_ident(c).map(|c| format!("\"{c}\"")))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    let sql = format!("SELECT {col_list} FROM \"{}\"", sanitize_ident(table)?);
    // 实现：每个列名/表名先过 sanitize_ident 白名单再双引号包裹，collect Result 后 ? 传播——
    // 任一标识符非法就整条查询失败，不会拼出半条 SQL（注入防线）。
    // 功能：通用「按列名清单取全部行的文本值」查询。
    account.with_conn(db, move |conn| {
        let mut stmt = conn.prepare(&sql)?;
        let n = cols.len();
        let mut out = Vec::new();
        let mut rows = stmt.query([])?;
        while let Some(r) = rows.next()? {
            let mut row = Vec::with_capacity(n);
            for i in 0..n {
                row.push(r.get::<_, Option<String>>(i).unwrap_or(None));
            }
            out.push(row);
        }
        Ok(out)
    })
    // 实现：with_conn 借出缓存连接（按库路径复用），prepare 一次、query 无参数（列名已全部拼进 SQL）。
    // 解析：逐行按列序号取 Option<String>，get 失败 unwrap_or(None)——非文本列（INTEGER/BLOB/NULL）
    // 取不到就是 None，绝不 panic；迭代中途出错会让整个查询返回 Err 而不是半截结果。
    // 功能：load_names/pick_name_table 的数据源。
}

/// 一张候选名表的查询计划（自 v1 原样移植）：user / disp / remark? / alias? 的列索引。
struct NameCols {
    table: String,
    wanted: Vec<String>,
    i_user: usize,
    i_disp: Option<usize>,
    i_remark: Option<usize>,
    i_alias: Option<usize>,
}
// NameCols：查询计划——wanted 是实际 SELECT 的列名有序列表，i_* 是各语义列在 wanted 里的位置。
// 功能：下游按索引取值，不必再关心列名差异。

/// 找一张同时含「用户名列 + 展示名列」的表（自 v1 原样移植），给出列索引。
fn pick_name_table(account: &Account, db: &Path) -> Result<Option<NameCols>> {
    let tables = account.with_conn(db, |conn| list_tables(conn))?;
    for t in &tables {
        let cols = match account.with_conn(db, |conn| table_columns(conn, t)) {
            Ok(c) => c,
            Err(_) => continue,
        };
        // 逐表取列名，失败静默 continue（虚拟表/损坏表取不到列名很常见）。
        let user = match find_candidate(&cols, USER_COLS) {
            Some(u) => u,
            None => continue,
        };
        let disp = match find_candidate(&cols, DISP_COLS) {
            Some(d) => d,
            None => continue,
        };
        let remark = find_candidate(&cols, REMARK_COLS);
        let alias = find_candidate(&cols, ALIAS_COLS);
        // 必须同时有用户名列与展示名列，缺任一 continue；备注/微信号可选。
        let mut wanted: Vec<String> = vec![user.clone(), disp.clone()];
        let push_uniq = |name: &Option<String>, wanted: &mut Vec<String>| {
            if let Some(n) = name {
                if !wanted.iter().any(|x| x.eq_ignore_ascii_case(n)) {
                    wanted.push(n.clone());
                }
            }
        };
        // push_uniq：可选列追加进 wanted 前按忽略大小写查重——某些库 remark 与 disp 可能同列，
        // 重复 SELECT 会让后面的位置索引算错。
        push_uniq(&remark, &mut wanted);
        push_uniq(&alias, &mut wanted);
        let pos = |name: &Option<String>| name.as_ref().and_then(|n| wanted.iter().position(|c| c.eq_ignore_ascii_case(n)));
        return Ok(Some(NameCols {
            table: t.clone(),
            i_user: wanted.iter().position(|c| c.eq_ignore_ascii_case(&user)).unwrap_or(0),
            i_disp: pos(&Some(disp)),
            i_remark: pos(&remark),
            i_alias: pos(&alias),
            wanted,
        }));
        // 返回第一个满足条件的表（list_tables 按名字升序，即字典序最靠前的那张）。
        // 功能：在 schema 未知的情况下自动找到「能当人名词表用」的表并给出列索引。
    }
    Ok(None)
    // 遍历完没有满足条件的表 → Ok(None)，调用方跳过该库。
}

/// 从某个库加载 username -> NameInfo（增量合并，自 v1 原样移植）。
fn load_names(account: &Account, db: &Path, map: &mut HashMap<String, NameInfo>) {
    let nc = match pick_name_table(account, db) {
        Ok(Some(nc)) => nc,
        _ => return,
    };
    // pick_name_table 返回 None/Err 直接 return——该库对词表无贡献，不影响其它库。
    let rows = match query_rows(account, db, &nc.table, &nc.wanted) {
        Ok(r) => r,
        Err(_) => return,
    };
    let from_session = is_session_db(db);
    let get = |row: &[Option<String>], i: Option<usize>| -> String {
        i.and_then(|i| row.get(i).and_then(|v| v.clone())).unwrap_or_default()
    };
    // from_session 标记本库是否 session.db（决定展示名写 session_display 还是 nickname）；
    // get 闭包做「列索引 → row.get → clone → unwrap_or_default」四级降级，缺列/NULL 都是空串。
    for row in rows {
        let username = get(&row, Some(nc.i_user));
        if username.trim().is_empty() {
            continue;
        }
        let disp = get(&row, nc.i_disp);
        let remark = get(&row, nc.i_remark);
        let alias = get(&row, nc.i_alias);
        let e = map.entry(username.clone()).or_insert_with(|| NameInfo { username: username.clone(), ..Default::default() });
        if !disp.trim().is_empty() {
            if from_session {
                if e.session_display.trim().is_empty() {
                    e.session_display = disp.clone();
                }
                if e.nickname.trim().is_empty() {
                    e.nickname = disp;
                }
            } else if e.nickname.trim().is_empty() {
                e.nickname = disp;
            }
        }
        // 展示名合并规则（v1 口径，调用顺序必须先 session.db 后 contact.db）：
        // session 展示名只为空时写 session_display，并在 nickname 为空时也填一份；
        // contact 展示名只在 nickname 为空时写。备注/微信号「非空即覆盖」（contact 优先级更高）。
        if !remark.trim().is_empty() {
            e.remark = remark;
        }
        if !alias.trim().is_empty() {
            e.alias = alias;
        }
    }
    // 功能：产出 username→NameInfo 词表，供 --list-sessions 输出会话/群名的四个名称字段。
}

/// 列出 session 库里的会话行：(sid, display, sort_ts)。
/// 以 v1 reply-exporter 的 list_session_pairs 为基础扩展：额外带出展示名列与活跃时间列
/// （两者都可选——SessionTable 可能没有昵称列，display 主要靠 contact.db 的 names 补）。
fn list_session_rows(account: &Account) -> Vec<(String, String, i64)> {
    let db = &account.layout.session_db;
    let tables = match account.with_conn(db, |c| list_tables(c)) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    // 选表策略与 v1 一致：优先表名恰为 SessionTable（忽略大小写），退化取第一个含用户名列的表。
    let mut target: Option<(String, String)> = None;
    for t in &tables {
        let cols = match account.with_conn(db, |c| table_columns(c, t)) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let user = match find_candidate(&cols, USER_COLS) {
            Some(u) => u,
            None => continue,
        };
        if t.eq_ignore_ascii_case("SessionTable") {
            target = Some((t.clone(), user));
            break;
        }
        if target.is_none() {
            target = Some((t.clone(), user));
        }
        // 名字恰好是 SessionTable 就立刻锁定；否则记住第一个含用户名列的表当退化候选。
        // 取列名失败静默 continue。功能：在不同版本表名变动下仍能找对会话表。
    }
    let Some((table, user)) = target else {
        return Vec::new();
    };
    // 定位到会话表后，再探测可选的展示名列与排序列（都过 find_candidate，落空则不 SELECT）。
    let cols = match account.with_conn(db, |c| table_columns(c, &table)) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let disp = find_candidate(&cols, DISP_COLS);
    let sort_col = find_candidate(&cols, SORT_TS_COLS);
    // 拼 SELECT："user"[, "disp"][, "sort"]；可选列不存在就不出现在 SQL 里。
    let mut select_cols: Vec<String> = vec![user.clone()];
    if let Some(d) = &disp {
        select_cols.push(d.clone());
    }
    if let Some(s) = &sort_col {
        select_cols.push(s.clone());
    }
    let rows = match query_rows(account, db, &table, &select_cols) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    // 组装输出：display/sort 取不到就是空串/0；空白 username 行丢弃。
    let mut out: Vec<(String, String, i64)> = rows
        .iter()
        .map(|r| {
            let sid = r.first().cloned().flatten().unwrap_or_default();
            let display = if disp.is_some() { r.get(1).cloned().flatten().unwrap_or_default() } else { String::new() };
            let sort_ts = if sort_col.is_some() {
                r.get(if disp.is_some() { 2 } else { 1 }).cloned().flatten().and_then(|s| s.trim().parse::<i64>().ok()).unwrap_or(0)
            } else {
                0
            };
            (sid, display, sort_ts)
        })
        .filter(|(sid, _, _)| !sid.trim().is_empty())
        .collect();
    // 有排序列就按活跃时间降序（最近的会话排前面，供面板直接展示）；
    // sort_ts 解析失败按 0 排最后（数值比较天然如此）。无排序列保持库内行序。
    if sort_col.is_some() {
        out.sort_by(|a, b| b.2.cmp(&a.2));
    }
    out
    // 功能：--list-sessions 的数据源（sid + 会话表内显示名 + 活跃时间）。
}

/// 账号目录名清洗 → 本人 wxid（自 v1 clean_account_dir_name 原样移植）：
/// 去掉尾部 `_<4 位字母数字>` 哈希段（wxid_xxx_1234 → wxid_xxx）。
fn clean_account_dir_name(base: &str) -> String {
    if let Some(pos) = base.rfind('_') {
        let head = &base[..pos];
        let suffix = &base[pos + 1..];
        if !head.is_empty()
            && suffix.len() == 4
            && suffix.bytes().all(|b| b.is_ascii_alphanumeric())
        {
            return head.to_string();
        }
    }
    base.to_string()
    // 实现：rfind 最后一个下划线，前段非空、后段恰好 4 字节全 ASCII 字母数字才把后段当哈希尾巴去掉。
    // 解析：对齐 WeFlow 的同名规则（账号目录形如 wxid_xxx_1234）；suffix.len() 是字节数，
    // 4 个中文字符是 12 字节不会误删。功能：从目录名还原本人 <wxid>。
}

fn is_group(username: &str) -> bool {
    username.contains("@chatroom")
}
// is_group：username 含 @chatroom 即群聊（v1 原口径，contains 兼容后缀在中间的形态）。
// 功能：--list-sessions 的 is_group 字段与后续 --messages 的会话类型判定。

fn account_mtime(p: &Path) -> std::time::SystemTime {
    std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH)
}
// account_mtime（自 v1 原样移植）：stat 取 modified，失败回退 UNIX_EPOCH（排最后）。
// 功能：「最近还被写过 = 当前登录号」的判据（微信只写当前登录账号的 session.db）。

/// 解码 hex→bytes→zstd 帧为文本（自 v1 reply-exporter main.rs:1210 原样移植）。
/// 微信 4.x 的图片/语音等消息正文是 zstd 压缩 BLOB，weflow-core 读出时被 hex 编码成字符串；
/// 本函数把「确实是 zstd 帧的 hex 串」解回明文（XML/文本），其余输入一律 None（调用方回退原文）。
fn zstd_decode_hex(hexstr: &str) -> Option<String> {
    if hexstr.is_empty() || hexstr.len() % 2 != 0 || !hexstr.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    // 严格前置校验：非空、偶数长度、全 hex，任一不满足直接 None。这三条是后面字节切片不 panic 的前提。
    let raw: Vec<u8> = (0..hexstr.len() / 2)
        .map(|i| u8::from_str_radix(&hexstr[i * 2..i * 2 + 2], 16).ok())
        .collect::<Option<Vec<u8>>>()?;
    // hex 转 bytes 用 collect 成 Option<Vec<u8>> 再问号传播——任何字节解析失败整体 None，宁可放弃也不拿半截数据去解压。
    if !raw.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        return None;
    }
    // 再校验 zstd 帧魔数（28 B5 2F FD）：不是 zstd 就 None，调用方应回退用原文（明文 XML/文本不经过这里）。
    let mut dec = ruzstd::decoding::StreamingDecoder::new(&raw[..]).ok()?;
    let mut out = Vec::new();
    use std::io::Read;
    dec.read_to_end(&mut out).ok()?;
    Some(String::from_utf8_lossy(&out).to_string())
    // ruzstd 是纯 Rust 流式 zstd 解码器（不依赖系统 libzstd）；from_utf8_lossy 把非法字节替换成
    // U+FFFD 而不是失败。坑（继承 v1）：没有解压后大小上限，压缩炸弹风险由"只处理本机微信库"兜住。
    // 功能：给 Python 侧渲染层干净的 XML/文本正文（占位符/标题/引用提取都在 Python 做）。
}

/// 各消息分库的 Name2Id 映射（自 v1 load_name2id_all 原样移植）：分库文件名小写 stem →
/// (rowid → user_name)。微信 4.x 消息表没有 sender_username 列，群聊发送者只有
/// real_sender_id（Name2Id 表的 rowid），必须反查还原成 <wxid>。
fn load_name2id_all(account: &Account) -> HashMap<String, HashMap<i64, String>> {
    const N2I_NAME: &[&str] = &["user_name", "username", "user_name_", "name"];
    let mut all: HashMap<String, HashMap<i64, String>> = HashMap::new();
    // N2I_NAME：Name2Id 表「名称列」候选别名（按优先级）。
    for db in &account.layout.message_dbs {
        let stem = db.file_stem().map(|s| s.to_string_lossy().to_lowercase()).unwrap_or_default();
        if stem.is_empty() || all.contains_key(&stem) {
            continue;
        }
        // 遍历消息分库，file_stem 小写为键；同名分库只认第一个。
        let map = (|| -> Result<HashMap<i64, String>> {
            let tables = account.with_conn(db, |c| list_tables(c))?;
            let table = tables
                .iter()
                .find(|t| t.eq_ignore_ascii_case("Name2Id"))
                .or_else(|| tables.iter().find(|t| t.to_ascii_lowercase().contains("name2id")))
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("无 Name2Id 表"))?;
            let cols = account.with_conn(db, |c| table_columns(c, &table))?;
            let name_col = find_candidate(&cols, N2I_NAME).ok_or_else(|| anyhow::anyhow!("Name2Id 无名称列"))?;
            let sql = format!(
                "SELECT rowid, \"{}\" FROM \"{}\"",
                sanitize_ident(&name_col)?,
                sanitize_ident(&table)?
            );
            account.with_conn(db, move |conn| {
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
                Ok(rows.flatten().collect::<HashMap<i64, String>>())
            })
        })()
        .unwrap_or_default();
        // 立即执行闭包把整段查询包成 Result 再 unwrap_or_default：任何一步失败（无 Name2Id 表/
        // 无名称列/SQL 错）都退化成空 map，不让整条导出失败。表名先精确匹配 Name2Id 再模糊包含；
        // rowid 是 SQLite 伪列直接写死，名称列/表名过 sanitize_ident 后双引号包裹，无注入面。
        all.insert(stem, map);
    }
    all
    // 即使 map 为空也 insert(stem, 空 map)：下游 n2i.get(db_name) 能区分「这个分库查过但没映射」
    // 与「压根没这个分库」。功能：把 real_sender_id 还原成真实发送者 <wxid>。
}

/// 从语音消息正文（zstd 压缩 XML）取 `voicelength="…"`（毫秒）。取不到返回 None。
/// （自 v1 reply-exporter main.rs:1235 原样移植。）
fn voice_duration_ms(content: &str) -> Option<i64> {
    let xml = zstd_decode_hex(content).unwrap_or_else(|| content.to_string());
    let i = xml.find("voicelength=\"")? + "voicelength=\"".len();
    let rest = &xml[i..];
    let end = rest.find('"')?;
    rest[..end].trim().parse::<i64>().ok()
    // 实现：先试 zstd 解压（语音正文是压缩 XML），失败回退原文；朴素 find 定位 voicelength 属性、
    // 取到下一个双引号为止、trim 后 parse 成 i64。
    // 解析：只认小写属性名与双引号写法，变体取不到返回 None（时长就缺省，不影响导出）。
    // 功能：给导出的语音条目补上时长（毫秒），供面板展示。
}

/// 跨 media_*.db 的语音 silk 提取（包装 wcdb::media::voice_in_table，行为对齐 v1 reply-exporter
/// 的 voice_silk_bytes）：遍历所有 media 分库，筛表名含 voice 的表逐张尝试，首中即返。
/// 返回 (Option<silk 字节>, 路由标签)；路由标签语义与 v1 略有简化——
/// found=命中 / no_media_db=没有 media 分库 / no_voice_table=分库里没有 voice 系表 / not_found=表都在但没命中。
fn voice_silk_bytes(
    account: &Account,
    create_time: i64,
    local_id: i64,
    svr_id: i64,
    names: &[String],
) -> (Option<Vec<u8>>, &'static str) {
    let mut last: &'static str = "no_media_db";
    for db in &account.layout.media_dbs {
        let tables: Vec<String> = account
            .with_conn(db, |c| list_tables(c))
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.to_ascii_lowercase().contains("voice"))
            .collect();
        // 遍历所有 media_*.db，只读列出表名并筛含 voice 的表（VoiceInfo 等）。
        // last 记录最近一次路由串供诊断；初值 no_media_db。with_conn 失败 unwrap_or_default 给空表列表（静默）。
        if tables.is_empty() && last == "no_media_db" {
            last = "no_voice_table";
        }
        for vt in tables {
            match voice_in_table(account, db, &vt, names, create_time, local_id, svr_id) {
                Some(hexv) => {
                    // wcdb 的 voice_in_table 返回 hex 串（SQLite 端 hex() 或 Blob 现算）；
                    // 这里还原成字节，写盘与 ASR 都吃原始 silk。
                    return (hex::decode(&hexv).ok(), "found");
                }
                None => {
                    last = "not_found";
                }
            }
        }
    }
    (None, last)
    // 实现：与 v1 voice_silk_bytes 同构的「逐库逐表 + 首中即返」循环，底层查询复用 wcdb 内省实现
    // （定位键 svr_id > local_id > create_time 三级择优 + Name2Id 会话收窄 + 两段式宽松兜底）。
    // 解析：与 v1 的差异仅在路由标签粒度（wcdb 的 voice_in_table 不回吐表内失败原因），
    // 语音是否取到的判定口径完全一致。功能：--messages 的语音 blob 落盘数据源。
}

fn emit(v: &Value, pretty: bool) {
    let s = if pretty { serde_json::to_string_pretty(v).unwrap() } else { serde_json::to_string(v).unwrap() };
    println!("{s}");
}
// emit（自 v1 原样移植）：唯一的 stdout 出口。pretty 决定紧凑/缩进；
// Value 全由 json! 构造不含 NaN，序列化 unwrap 安全。
// 功能：stdout=数据 / stderr=诊断 的分离协议。

/// 枚举数据根下的账号目录（自 v1 login_probe 的枚举口径移植）：
/// 只认「子目录/db_storage/session/session.db 是文件」的目录为账号目录。
fn account_dirs_under(root: &Path) -> Vec<(String, PathBuf)> {
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for ent in entries.flatten() {
        let dir = ent.path();
        if !dir.is_dir() {
            continue;
        }
        let base = match dir.file_name().map(|s| s.to_string_lossy().to_string()) {
            Some(b) => b,
            None => continue,
        };
        let session_db = dir.join("db_storage").join("session").join("session.db");
        if !session_db.is_file() {
            continue;
        }
        out.push((base, dir));
        // 判据与 v1 login_probe 相同：session.db 存在即账号目录（all_users/缓存目录自然被跳过）。
    }
    out.sort();
    out
    // 功能：--list-accounts 与账号选择共用的枚举出口（排序保证输出可复现）。
}

/// 本机当前登录账号：数据根下所有「含 db_storage/session/session.db」的账号目录里，
/// session.db 最近被写的那只——微信只写当前登录号。纯 stat 判定，**不需要密钥**，
/// 所以密钥失效/未扫的新号也能被认出来（这正是换号防线的依据）。
fn current_login(data_root: &str) -> Option<(String, String)> {
    let mut best: Option<(std::time::SystemTime, String, String)> = None;
    for (base, dir) in account_dirs_under(Path::new(data_root)) {
        let session_db = dir.join("db_storage").join("session").join("session.db");
        let m = account_mtime(&session_db);
        if best.as_ref().map_or(true, |(t, _, _)| m > *t) {
            best = Some((m, base.clone(), clean_account_dir_name(&base)));
        }
    }
    best.map(|(_, base, wxid)| (base, wxid))
    // 实现：遍历账号目录取 session.db mtime 最大者（与 login_probe 同判据，但不要求有钥）。
    // 解析：返回 (账号目录名, 还原的 wxid)；数据根下没有任何账号目录时 None。
    // 功能：让每个模式的输出都能带上「数据来源号 vs 当前登录号」的一致性标记，
    //       Python 侧据此触发换号防线（自动重扫），杜绝拿旧号数据冒充当前号。
}

/// 换号防线字段：serving（本次打开的账号）是否就是当前登录号。
/// 三个模式的输出统一带 serving_wxid / current_login_wxid / serves_current_login。
fn login_guard_fields(data_root: &str, acct_dir: &Path) -> (String, String, bool) {
    let serving = acct_dir
        .file_name()
        .map(|s| clean_account_dir_name(&s.to_string_lossy()))
        .unwrap_or_default();
    let (cur_dir, cur_wxid) = current_login(data_root).unwrap_or_default();
    let serves = !cur_dir.is_empty()
        && acct_dir
            .file_name()
            .map(|s| s.eq_ignore_ascii_case(&cur_dir))
            .unwrap_or(false);
    (serving, cur_wxid, serves)
    // 实现：serving 取本次实际打开的账号目录还原 wxid；cur 取 stat 探测的当前登录号；
    //       一致性按目录名忽略大小写比较。
    // 解析：cur_dir 为空（数据根下没有任何账号在写）时 serves=False——Python 侧仅在
    //       current_login_wxid 非空时才触发重扫防线，避免"微信从未登录"被误判成换号。
    // 功能：换号防线的判定数据源。
}

/// 群成员候选的 sender_pool 回退：取该群最近 n 条消息，把发送者（real_sender_id→Name2Id
/// 反查）去重后作为成员候选。微信 4.1.15 实测：wcdb 的成员表双模型对所有群返回空
/// （无 error），发言者池是唯一可靠来源，故 roster 为空时必须走这里。
fn sender_pool(
    account: &Account,
    sid: &str,
    names: &HashMap<String, NameInfo>,
    n: usize,
) -> Result<Vec<Value>> {
    let n2i = load_name2id_all(account);
    let (rows, _route, _tf) = session_message_rows_recent_routed(account, sid, n)?;
    let mut seen: Vec<String> = Vec::new();
    for row in &rows {
        let m = match row.as_object() {
            Some(m) => m,
            None => continue,
        };
        let mut sender = as_str(ci_get(m, "sender_username"));
        if sender.is_empty() {
            let db_name = as_str(ci_get(m, "db_name")).to_lowercase();
            let rid = as_i64(ci_get(m, "real_sender_id").or_else(|| ci_get(m, "realSenderId")));
            if rid != 0 {
                if let Some(uname) = n2i.get(&db_name).and_then(|mp| mp.get(&rid)) {
                    sender = uname.clone();
                }
            }
        }
        // 发送者反查与 run_messages 的消息行同口径；空白跳过，按首次出现顺序去重。
        if !sender.is_empty() && !seen.contains(&sender) {
            seen.push(sender);
        }
    }
    Ok(seen
        .into_iter()
        .map(|u| {
            let info = names.get(&u);
            json!({
                "username": u,
                "group_display": Value::Null,   // 发言者池拿不到群内昵称，如实为 null
                "remark": info.map(|x| x.remark.clone()).unwrap_or_default(),
                "nickname": info.map(|x| x.nickname.clone()).unwrap_or_default(),
                "alias": info.map(|x| x.alias.clone()).unwrap_or_default(),
                "source": "sender_pool",
            })
        })
        .collect())
    // 实现：复用 --messages 的取数与发送者反查链路（最近 n 条，LIMIT 下推，快）。
    // 解析：发言者池 ≠ 全群名册——从没在窗口内发言的人不在池里；面板按 source 标注口径。
    // 功能：成员表失效时的成员搜索数据源（诊断实测：本机 4.1.15 上是主来源）。
}

/// --group-members-all：一次性遍历数据根下当前账号的**全部群聊**，产出成员搜索索引。
/// 每群先走 wcdb 双模型（roster），为空则回退 sender_pool（最近 POOL_N 条消息）。
fn run_group_members_all(ks: &KeyStore, data_root: &str, pretty: bool) -> Result<()> {
    const POOL_N: usize = 500;
    let (acct_dir, account, _) = pick_account_opened(ks, data_root, None, None)?;
    let names = build_names(&account);
    // 会话清单里筛群聊（群 = @chatroom）：只有会话里出现过的群才有本地消息，值得索引。
    let chatrooms: Vec<(String, String)> = list_session_rows(&account)
        .into_iter()
        .filter(|(sid, _, _)| is_group(sid))
        .map(|(sid, disp, _)| (sid, disp))
        .collect();
    let mut groups: Vec<Value> = Vec::with_capacity(chatrooms.len());
    let mut roster_ok = 0usize;
    let mut pool_used = 0usize;
    for (sid, disp) in &chatrooms {
        let (members, err) = match resolve_group_members(&account, sid) {
            Ok(v) if !v.is_empty() => {
                roster_ok += 1;
                (
                    v.into_iter()
                        .map(|m| {
                            let info = names.get(&m.username);
                            json!({
                                "username": m.username,
                                "group_display": m.display,
                                "remark": info.map(|x| x.remark.clone()).unwrap_or_default(),
                                "nickname": info.map(|x| x.nickname.clone()).unwrap_or_default(),
                                "alias": info.map(|x| x.alias.clone()).unwrap_or_default(),
                                "source": "roster",
                            })
                        })
                        .collect::<Vec<_>>(),
                    None,
                )
            }
            _ => {
                pool_used += 1;
                let pool = sender_pool(&account, sid, &names, POOL_N).unwrap_or_default();
                (
                    pool,
                    Some("member_table_empty_fallback_sender_pool".to_string()),
                )
            }
        };
        groups.push(json!({
            "sid": sid,
            "name": disp,
            "count": members.len(),
            "members": members,
            "error": err,
        }));
    }
    let (serving_wxid, current_login_wxid, serves_current_login) =
        login_guard_fields(data_root, &acct_dir);
    emit(&json!({
        "mode": "group_members_all",
        "account_dir": acct_dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        "serving_wxid": serving_wxid,
        "current_login_wxid": current_login_wxid,
        "serves_current_login": serves_current_login,
        "groups_total": chatrooms.len(),
        "roster_ok": roster_ok,
        "pool_used": pool_used,
        "groups": groups,
    }), pretty);
    Ok(())
    // 实现：选号 → 筛群 → 逐群 roster/sender_pool 双路 → 汇总输出（含统计）。
    // 解析：单进程单 contact.db 打开，47 个群实测毫秒级；回退群才开消息库（LIMIT 下推）。
    //       error 字段把回退原因带到前端（诊断不再被吞）。
    // 功能：面板「群聊+成员搜索」索引的数据源。
}

/// 选当前登录账号并直接交付已打开的 Account：数据根下「session.db 最近被写过且密钥能开库」的那只。
/// 与 v1 pick_account_dir 的差异：① 本项目数据根必填且强校验，不做"根外兜底"（绝不静默读别的目录）；
/// ② 直接返回 Arc<Account>，调用方不必重复开库验钥；③ 支持 wanted_sid——优先选「库里真有这个会话」
/// 的账号（v1 同判据），并把「sid 是否命中」带回去供输出层提示。
fn pick_account_opened(
    ks: &KeyStore,
    data_root: &str,
    wanted_dir: Option<&str>,
    wanted_sid: Option<&str>,
) -> Result<(PathBuf, Arc<Account>, bool)> {
    let mut openable: Vec<(PathBuf, Arc<Account>, std::time::SystemTime, bool)> = Vec::new();
    let mut no_key: usize = 0;
    let mut bad_key: usize = 0;
    for (base, dir) in account_dirs_under(Path::new(data_root)) {
        // --account 显式指定时按目录名或还原出的 wxid 过滤（都做忽略大小写比较）。
        if let Some(w) = wanted_dir {
            let w = w.trim().to_ascii_lowercase();
            let wxid = clean_account_dir_name(&base).to_ascii_lowercase();
            if base.to_ascii_lowercase() != w && wxid != w {
                continue;
            }
        }
        let session_db = dir.join("db_storage").join("session").join("session.db");
        let Some(key) = ks.session_key_for(&dir) else {
            no_key += 1;
            continue;
        };
        // Account::open 内部：布局探测 + keystore 加载 + 真开 session.db 验钥（失败即 Err）。
        // 开得成 = 密钥可用 + 库结构可读，一条判据同时覆盖"有钥"与"钥有效"。
        match Account::open(session_db.to_string_lossy().as_ref(), &key) {
            Ok(a) => {
                let has_sid = match wanted_sid {
                    Some(sid) => list_session_rows(&a)
                        .iter()
                        .any(|(u, _, _)| u.eq_ignore_ascii_case(sid)),
                    None => false,
                };
                // has_sid：只有传了 wanted_sid 才查会话清单比对（忽略大小写），判断这只账号
                // 的库里真有这个会话——v1 pick_account_dir 的同名口径，防"拿旧号数据答新号"。
                openable.push((dir, a, account_mtime(&session_db), has_sid));
            }
            Err(_) => {
                bad_key += 1;
            }
        }
    }
    // 诊断信息进错误文案（不含密钥——账号目录名本身含 wxid，属本机诊断可接受范围，
    // 与 v1 login_probe 的 note 口径一致）。
    if openable.is_empty() {
        let hint = if no_key > 0 && bad_key == 0 {
            format!("{no_key} 个账号没有密钥：请先运行 keyscan（面板体检按钮会自动做）")
        } else if bad_key > 0 && no_key == 0 {
            format!("{bad_key} 个账号的密钥打不开库：多半是重新登录换了钥，请重扫密钥")
        } else if no_key + bad_key > 0 {
            format!("{no_key} 个账号没密钥、{bad_key} 个密钥失效：请重扫密钥")
        } else {
            "数据根下没有发现任何微信账号目录（含 db_storage/session/session.db 的目录）".to_string()
        };
        bail!("没有可用的账号——{hint}");
    }
    // 选号优先级：① 传了 wanted_sid 时，在「库里真有这个会话」的账号中取 session.db
    // 最近被写的那只（= 当前登录号）。不能取"第一个命中"——账号目录按字典序遍历，
    // 同一个群/联系人在两个号里都存在时，旧号会稳定压过新号，重扫密钥也救不回来
    // （重试还是按同样的顺序挑同一个），真机验收踩过这个坑。
    if wanted_sid.is_some() {
        let mut hits: Vec<&(PathBuf, Arc<Account>, std::time::SystemTime, bool)> =
            openable.iter().filter(|x| x.3).collect();
        hits.sort_by(|a, b| b.2.cmp(&a.2));
        if let Some((dir, a, _, _)) = hits.first() {
            return Ok((dir.clone(), a.clone(), true));
        }
    }
    // ② 没传 sid 或没有账号包含它：按 session.db mtime 取最大（最近被写 = 当前登录号）。
    openable.sort_by(|x, y| y.2.cmp(&x.2));
    let (dir, a, _, has_sid) = openable.into_iter().next().unwrap();
    Ok((dir, a, has_sid))
    // 返回 (账号目录, 已验钥的 Account, sid 是否命中)。sid 没命中任何账号时仍返回 mtime 最新的
    // 一只 + has_sid=false——让 --messages 正常跑出 route=none/0 条，输出层的 sid_found 字段
    // 会把「会话不在这个号」讲清楚，而不是凭空报错。
    // 功能：--list-sessions/--group-members/--messages 的统一账号入口。
}

fn run_list_accounts(ks: &KeyStore, data_root: &str, pretty: bool) -> Result<()> {
    let mut items: Vec<Value> = Vec::new();
    for (base, dir) in account_dirs_under(Path::new(data_root)) {
        let session_db = dir.join("db_storage").join("session").join("session.db");
        let wxid = clean_account_dir_name(&base);
        // session_age_s：距今秒数（微信只写当前登录号的 session.db，越小越是当前号）。
        let age_s = std::fs::metadata(&session_db)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| std::time::SystemTime::now().duration_since(t).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(-1);
        // has_key 只查 keystore；key_works 真开一次库验证（v1 login_probe 的三级口径）。
        let has_key = ks.session_key_for(&dir).is_some();
        let works = has_key
            && ks.session_key_for(&dir)
                .map(|k| Account::open(session_db.to_string_lossy().as_ref(), &k).is_ok())
                .unwrap_or(false);
        items.push(json!({
            "account_dir": base,
            "wxid": wxid,
            "session_db_age_s": age_s,
            "has_key": has_key,
            "key_works": works,
        }));
    }
    emit(&json!({ "mode": "list_accounts", "accounts": items }), pretty);
    Ok(())
    // 实现：枚举账号目录，逐个产出 (目录名, wxid, 库 mtime 距今秒, 有钥?, 钥有效?)。
    // 解析：key_works 以 Account::open 成败为准——「有钥 ≠ 钥能用」
    // （重登录换钥后旧钥静默打不开库，v1 踩过的坑）。
    // 功能：面板体检页的数据源。
}

/// --group-members：列出某群的完整成员名单（含每个成员的备注/昵称/微信号与群内昵称）。
fn run_group_members(
    ks: &KeyStore,
    data_root: &str,
    wanted: Option<&str>,
    sid: &str,
    pretty: bool,
) -> Result<()> {
    if sid.trim().is_empty() {
        bail!("--group-members 需要配合 --sid <群 id>");
    }
    // 传 wanted_sid 让选号优先挑「库里真有这个群」的账号（多账号机器防拿错号）。
    let (acct_dir, account, sid_found) = pick_account_opened(ks, data_root, wanted, Some(sid))?;
    let names = build_names(&account);
    // 群成员双模型内省（wcdb 原实现）：Err 视为该库 schema 异常，按 0 成员降级但把原因带出去，
    // 让面板能区分「真的没成员」与「成员表解析失败」。
    let (members, members_error) = match resolve_group_members(&account, sid) {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(e.to_string())),
    };
    let (serving_wxid, current_login_wxid, serves_current_login) =
        login_guard_fields(data_root, &acct_dir);
    let items: Vec<Value> = members
        .into_iter()
        .map(|m| {
            let info = names.get(&m.username);
            json!({
                "username": m.username,
                "group_display": m.display,      // 群内昵称（群成员列表列/成员表），可空
                "remark": info.map(|n| n.remark.clone()).unwrap_or_default(),
                "nickname": info.map(|n| n.nickname.clone()).unwrap_or_default(),
                "alias": info.map(|n| n.alias.clone()).unwrap_or_default(),
            })
        })
        .collect();
    emit(&json!({
        "mode": "group_members",
        "sid": sid,
        "is_group": is_group(sid),
        "sid_found": sid_found,
        "account_dir": acct_dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        "serving_wxid": serving_wxid,
        "current_login_wxid": current_login_wxid,
        "serves_current_login": serves_current_login,
        "count": items.len(),
        "members": items,
        "error": members_error,
    }), pretty);
    Ok(())
    // 实现：选号（sid 优先）→ 名称词表 → resolve_group_members 双模型 → 每成员合并四个名称字段。
    // 解析：成员的 username 才是稳定键；display/remark/nickname/alias 都可能为空，
    // 展示名回退（备注>昵称>微信号>id）由 Python session.py 统一做。
    // 功能：面板群成员勾选列表的数据源。
}

/// --messages：读取某会话的消息。两种取数方式（互斥）：
/// --recent N：最近 N 条（每分库 LIMIT 下推，快路）；
/// --begin/--end：秒级时间窗（0=不限，即全部历史），流式逐表读。
/// 可选 --voice-out <目录>：把结果集内语音消息（local_type==34）的 silk blob 落盘，
/// 消息行附 voice 元数据（文件名/字节数/时长/路由），供 Python 侧 ASR。
/// 输出为原始数据（content 尽力 zstd 解压；发送者经 Name2Id 反查），渲染在 Python 侧做。
fn run_messages(
    ks: &KeyStore,
    data_root: &str,
    wanted: Option<&str>,
    sid: &str,
    begin: i64,
    end: i64,
    recent: Option<usize>,
    voice_out: Option<&str>,
    pretty: bool,
) -> Result<()> {
    if sid.trim().is_empty() {
        bail!("--messages 需要配合 --sid <会话 id>");
    }
    let (acct_dir, account, sid_found) = pick_account_opened(ks, data_root, wanted, Some(sid))?;
    // 路由名诊断：流式路径的 route 从索引直接取；recent 路径的 route 由 *_routed 返回。
    // 微信改布局时这是「从哪一级路由退化」的第一现场证据（v1 同口径）。
    let stream_route = match recent {
        Some(_) => None,
        None => {
            let index = account.msg_index()?;
            let (_tables, route) = index.session_tables_routed(&account, &sid)?;
            Some(route)
        }
    };
    // 取行：recent 走 *_routed（新→旧），流式走 ascending=true（库内升序）。
    let (mut rows, route, time_fallback) = match recent {
        Some(n) => {
            let (rows, route, tf) = session_message_rows_recent_routed(&account, &sid, n)?;
            (rows, route, tf)
        }
        None => {
            let mut v: Vec<Value> = Vec::new();
            stream_session_message_rows(&account, &sid, begin, end, true, |row| {
                v.push(row);
                Ok(())
            })?;
            (v, stream_route.unwrap_or("none"), false)
        }
    };
    // recent 路径的行序是新→旧，统一翻成时间正序输出（与流式路径一致，Python 无需分叉处理）。
    rows.reverse();
    let n2i = load_name2id_all(&account);
    // my_id：本人 wxid（账号目录名还原）。群聊里 real_sender_id == my_id 的消息即本人发言，
    // Python 侧据此决定占位/展示；不在导出器里做方向判定（字段3私聊恒空，方向由需求定死）。
    let my_id = acct_dir
        .file_name()
        .map(|s| clean_account_dir_name(&s.to_string_lossy()))
        .unwrap_or_default();
    // 语音落盘目录：给了 --voice-out 才启用（不指定就完全不碰语音字节，与 v1 图片引擎同口径）。
    let voice_dir = voice_out.map(|d| {
        let _ = std::fs::create_dir_all(d);
        PathBuf::from(d)
    });
    let mut messages: Vec<Value> = Vec::with_capacity(rows.len());
    for row in &rows {
        let m = match row.as_object() {
            Some(m) => m,
            None => continue,
        };
        // row 是 weflow-core normalize_row 产出的 JSON Object（BLOB 列已 hex 化、列名已归一）。
        let local_type = as_i64(ci_get(m, "local_type"));
        let raw_content = as_str(ci_get(m, "message_content"));
        let content = zstd_decode_hex(&raw_content).unwrap_or(raw_content);
        // content 尽力解压：确实是 zstd 帧的 hex 串解回明文 XML/文本；明文或解不开的保持原样
        // （Python 渲染层按「type 1 + 内容仍像 hex」判定卡片类消息，口径与 v1 render_message 一致）。
        let db_name = as_str(ci_get(m, "db_name")).to_lowercase();
        let mut sender = as_str(ci_get(m, "sender_username"));
        if sender.is_empty() {
            let rid = as_i64(ci_get(m, "real_sender_id").or_else(|| ci_get(m, "realSenderId")));
            if rid != 0 {
                if let Some(uname) = n2i.get(&db_name).and_then(|mp| mp.get(&rid)) {
                    sender = uname.clone();
                }
            }
        }
        // 微信 4.x 消息表没有 sender_username 列，normalize_row 不保证补出来——
        // 用 real_sender_id（Name2Id 的 rowid）反查（v1 同款二级解析）；rid==0 保持空串。
        // 语音：local_type==34 且给了 --voice-out 时，从 media_*.db 的 VoiceInfo 取 silk blob 落盘
        // （只落原始 silk，绝不在导出器里解码/转写；silk→ASR 全部交下游 Python，音频不出本机）。
        let create_time = as_i64(ci_get(m, "create_time")); // UTC Unix 秒；语音定位键之一
        let mut voice: Option<Value> = None;
        if local_type == 34 {
            if let Some(dir) = &voice_dir {
                let lid = as_i64(ci_get(m, "local_id").or_else(|| ci_get(m, "localId")));
                let svr = as_i64(
                    ci_get(m, "server_id")
                        .or_else(|| ci_get(m, "svr_id"))
                        .or_else(|| ci_get(m, "svrid")),
                );
                // 语音定位键是 (create_time, local_id, server_id) 三元组，列名各版本不同用 or_else 链兜底；
                // 取不到给 0，voice_in_table 里 0 表示「该键不参与匹配」。
                let mut vn: Vec<String> = vec![sid.to_string()];
                if !sender.is_empty() {
                    vn.push(sender.clone());
                }
                vn.sort();
                vn.dedup();
                // vn = 会话名 + 发送者名，sort+dedup 后作为会话归属候选——限定语音必须属于本会话，
                // 防串会话取到别人的音频（v1 同口径，收窄在 wcdb voice_in_table 内做）。
                let (vsilk, vroute) = voice_silk_bytes(&account, create_time, lid, svr, &vn);
                voice = Some(match vsilk {
                    Some(b) => {
                        // 文件名用 (create_time, local_id, 会话段) 而非 v1 的自增序号：实时增量
                        // 模式每轮新起一次导出，序号会撞名；确定性文件名重复导出同一条时原地
                        // 覆盖，无害。会话段（sid 非字母数字替换为下划线）防「两个会话的消息
                        // 恰好同秒且同 local_id」的跨会话覆盖——那会把 A 会话的音频张冠李戴。
                        let safe_sid: String = sid
                            .chars()
                            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                            .collect();
                        let fname = format!("voice_{}_{}_{}.silk", create_time, lid, safe_sid);
                        let wrote = std::fs::write(dir.join(&fname), &b).is_ok();
                        json!({
                            "status": if wrote { "extracted" } else { "write_failed" },
                            "bytes": b.len(),
                            "file": if wrote { fname } else { String::new() },
                            "duration_ms": voice_duration_ms(&content),
                            "route": vroute,
                        })
                    }
                    None => json!({
                        "status": "not_local",
                        "file": String::new(),
                        "bytes": 0,
                        "duration_ms": voice_duration_ms(&content),
                        "route": vroute,
                        "note": "本机无该语音 silk blob（未播放/未下载）",
                    }),
                });
                // 未命中不中断导出：Python 渲染层看到 status != extracted 时回退 [语音] 占位符。
            }
        }
        messages.push(json!({
            "local_id": as_i64(ci_get(m, "local_id")),
            "create_time": create_time,                          // UTC Unix 秒，原样透传
            "sort_seq": as_i64(ci_get(m, "sort_seq")),          // 同秒内排序键
            "local_type": local_type,                            // 复合值：高32位=appmsg子类型
            "content": content,
            "sender_username": sender,
            "voice": voice,                                      // 仅语音消息且给了 --voice-out 时非 null
        }));
    }
    let (serving_wxid, current_login_wxid, serves_current_login) =
        login_guard_fields(data_root, &acct_dir);
    emit(&json!({
        "mode": "messages",
        "sid": sid,
        "is_group": is_group(sid),
        "sid_found": sid_found,
        "account_dir": acct_dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        "serving_wxid": serving_wxid,
        "current_login_wxid": current_login_wxid,
        "serves_current_login": serves_current_login,
        "my_id": my_id,
        "route": route,
        "time_fallback": time_fallback,
        "begin": begin,
        "end": end,
        "recent": recent,
        "count": messages.len(),
        "messages": messages,
    }), pretty);
    Ok(())
    // 实现：选号（sid 优先）→ 路由诊断 → recent/流式两条取数路径 → 每行六字段归一 → 输出。
    // 解析：时间窗过滤在 weflow-core 回调内做（[begin,end] 闭区间，0=不限）；行序统一为
    // 时间正序；跨表路由（shared 模式）下跨表顺序不保证全局有序——Python 导出层按
    // (create_time, sort_seq) 重排，这里只保证单表内有序（本机 4.x 每会话一张 md5 表，恒单表）。
    // 功能：历史导出与实时增量的统一取数入口。
}

/// 从已打开的账号构建 username→NameInfo 词表（合并顺序 v1 口径：先 session.db 后 contact.db）。
fn build_names(account: &Account) -> HashMap<String, NameInfo> {
    let mut names: HashMap<String, NameInfo> = HashMap::new();
    load_names(account, &account.layout.session_db, &mut names);
    if let Some(cdb) = &account.layout.contact_db {
        load_names(account, cdb, &mut names);
    }
    names
    // 实现：先 session（占 session_display/nickname），再 contact（补 nickname/remark/alias，
    // 非空即覆盖 remark/alias）——顺序决定合并语义，不能颠倒。
    // 功能：--list-sessions/--group-members 共用的名称来源。
}

fn run_list_sessions(ks: &KeyStore, data_root: &str, wanted: Option<&str>, pretty: bool) -> Result<()> {
    let (acct_dir, account, _sid_hit) = pick_account_opened(ks, data_root, wanted, None)?;
    // --list-sessions 不关心特定会话，wanted_sid 传 None（纯 mtime 选号）。
    let names = build_names(&account);
    // 会话行：sid + 会话表内显示名 + 活跃时间（list_session_rows 已按活跃时间降序）。
    let rows = list_session_rows(&account);
    let sessions: Vec<Value> = rows
        .into_iter()
        .map(|(sid, display, sort_ts)| {
            let info = names.get(&sid).cloned().unwrap_or_else(|| NameInfo { username: sid.clone(), ..Default::default() });
            json!({
                "sid": sid,
                "is_group": is_group(&sid),
                "session_display": display,
                "remark": info.remark,
                "nickname": info.nickname,
                "alias": info.alias,
                "sort_ts": sort_ts,
            })
        })
        .collect();
    // 输出原始名称字段，展示名回退（备注>昵称>微信号>id）由 Python session.py 统一做——
    // 导出器只交数据，不做展示决策（职责边界见文件头注释）。
    // 换号防线字段：serves_current_login=False 时 Python 侧触发自动重扫，绝不拿旧号数据。
    let (serving_wxid, current_login_wxid, serves_current_login) =
        login_guard_fields(data_root, &acct_dir);
    emit(&json!({
        "mode": "list_sessions",
        "account_dir": acct_dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        "wxid": serving_wxid,
        "serving_wxid": serving_wxid,
        "current_login_wxid": current_login_wxid,
        "serves_current_login": serves_current_login,
        "count": sessions.len(),
        "sessions": sessions,
    }), pretty);
    Ok(())
    // 功能：面板会话列表页的数据源（私聊/群聊分组的原始材料）。
}

/// 命令行参数（手写解析，口径参照 v1：未知旗标显式报错，绝不静默吞掉——
/// 静默吞旗标的后果是把旗标的值当数据根读错目录）。
struct Args {
    data_root: Option<String>,
    store: Option<String>,
    account: Option<String>,
    sid: Option<String>,
    begin: i64,
    end: i64,
    recent: Option<usize>,
    voice_out: Option<String>,
    list_accounts: bool,
    list_sessions: bool,
    group_members: bool,
    group_members_all: bool,
    messages: bool,
    pretty: bool,
    version: bool,
    unknown: Option<String>,
}

fn parse_args(argv: &[String]) -> Args {
    let mut a = Args {
        data_root: None,
        store: None,
        account: None,
        sid: None,
        begin: 0,
        end: 0,
        recent: None,
        voice_out: None,
        list_accounts: false,
        list_sessions: false,
        group_members: false,
        group_members_all: false,
        messages: false,
        pretty: false,
        version: false,
        unknown: None,
    };
    let mut i = 0usize;
    while i < argv.len() {
        let s = argv[i].as_str();
        match s {
            "--data-root" => { i += 1; if i < argv.len() { a.data_root = Some(argv[i].clone()); } }
            "--store" => { i += 1; if i < argv.len() { a.store = Some(argv[i].clone()); } }
            "--account" => { i += 1; if i < argv.len() { a.account = Some(argv[i].clone()); } }
            "--sid" => { i += 1; if i < argv.len() { a.sid = Some(argv[i].clone()); } }
            "--voice-out" => { i += 1; if i < argv.len() { a.voice_out = Some(argv[i].clone()); } }
            // 带值旗标：i 先自增指向值，越界就什么都不做（旗标被丢弃，由后续校验兜住）。
            // begin/end 用 parse().unwrap_or(0)：0 就是"该端不限"的合法值，非法写法退化为不限。
            "--begin" => { i += 1; if i < argv.len() { a.begin = argv[i].trim().parse().unwrap_or(0); } }
            "--end" => { i += 1; if i < argv.len() { a.end = argv[i].trim().parse().unwrap_or(0); } }
            // recent 解析失败保持 None（走时间窗路径），解析成功且为 0 时按"取 0 条"处理
            // （session_message_rows_recent 对 n=0 有专门短路，返回空结果）。
            "--recent" => {
                i += 1;
                if i < argv.len() {
                    if let Ok(n) = argv[i].trim().parse::<usize>() {
                        a.recent = Some(n);
                    }
                }
            }
            "--list-accounts" => a.list_accounts = true,
            "--list-sessions" => a.list_sessions = true,
            "--group-members" => a.group_members = true,
            "--group-members-all" => a.group_members_all = true,
            "--messages" => a.messages = true,
            "--pretty" => a.pretty = true,
            "--version" | "-V" => a.version = true,
            _ => {
                if s.starts_with("--") {
                    // 未知旗标只记第一个——main 里显式报错退出，不静默忽略。
                    if a.unknown.is_none() {
                        a.unknown = Some(s.to_string());
                    }
                } else if a.data_root.is_none() {
                    // 兼容 v1 的位置参数写法：第一个裸参数当数据根。
                    a.data_root = Some(s.to_string());
                }
            }
        }
        i += 1;
    }
    a
    // 功能：把 argv 解析成结构化参数；未知旗标进 unknown 供 main 报错。
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&argv);
    if args.version {
        println!("wxmsg-exporter {}（数据层 weflow-core {}；实测适配微信 4.1.15.x Windows 版）",
                 env!("CARGO_PKG_VERSION"), weflow_core::version());
        std::process::exit(0);
    }
    if let Some(flag) = &args.unknown {
        eprintln!("[err] 未知参数 {flag}（用 --version 查看用法）");
        std::process::exit(2);
    }
    // --store：覆盖密钥库位置。keystore::store_path() 的最高优先级就是 WEFLOW_KEY_STORE
    // 环境变量，所以这里设环境变量即可让 Account::open 内部的 KeyStore::load_default 读到。
    if let Some(store) = &args.store {
        std::env::set_var("WEFLOW_KEY_STORE", store);
    }
    let result = run(args);
    if let Err(e) = result {
        eprintln!("[err] {e:#}");
        std::process::exit(1);
    }
    // 实现：version 短路 → 未知旗标拒绝 → 设 store 环境变量 → run 派发 → Err 进 stderr + 退出码 1。
    // 解析：错误用 {e:#} 输出整条 anyhow 链（各层 context），帮助定位是哪一步失败；
    // 密钥不进任何错误串（weflow-core 的错误文案已按此口径构造）。
    // 功能：main 的统一错误出口与退出码协议（0=成功 1=失败 2=用法错误）。
}

fn run(args: Args) -> Result<()> {
    if !args.list_accounts && !args.list_sessions && !args.group_members
        && !args.group_members_all && !args.messages {
        bail!("必须指定模式：--list-accounts / --list-sessions / --group-members / --group-members-all / --messages（--version 查看用法）");
    }
    let data_root = match &args.data_root {
        Some(r) if !r.trim().is_empty() => r.clone(),
        _ => bail!("必须指定微信数据根 --data-root <目录>"),
    };
    if !Path::new(&data_root).is_dir() {
        bail!("数据根目录不存在：请核对微信 设置→文件管理 里的路径");
    }
    // 数据根存在性在这里闸一次（v1 main 同口径），后面的模式函数不必重复检查。
    let ks = KeyStore::load_default();
    // KeyStore::load_default 读 store_path()（--store 已注入环境变量）；
    // store 损坏/不存在时降级为空库——空库会让 pick_account_opened 报"没有密钥"，错误文案可操作。
    if args.list_accounts {
        return run_list_accounts(&ks, &data_root, args.pretty);
    }
    if args.list_sessions {
        return run_list_sessions(&ks, &data_root, args.account.as_deref(), args.pretty);
    }
    if args.group_members {
        return run_group_members(
            &ks, &data_root, args.account.as_deref(),
            args.sid.as_deref().unwrap_or(""), args.pretty,
        );
    }
    if args.group_members_all {
        return run_group_members_all(&ks, &data_root, args.pretty);
    }
    run_messages(
        &ks, &data_root, args.account.as_deref(),
        args.sid.as_deref().unwrap_or(""),
        args.begin, args.end, args.recent,
        args.voice_out.as_deref(), args.pretty,
    )
    // 功能：模式派发（四个模式齐备）。
}

#[cfg(test)]
mod tests {
    use super::*;
    // 实现：单元测试集中在"纯函数 + 无需真库"的口径锚点上；
    //       开库/路由类逻辑由 weflow-core 自带测试与步骤 12 真机验收覆盖。

    #[test]
    fn clean_account_dir_name_strips_hash_tail() {
        assert_eq!(clean_account_dir_name("wxid_abc_1234"), "wxid_abc");
        assert_eq!(clean_account_dir_name("wxid_abc_ab12"), "wxid_abc");
        assert_eq!(clean_account_dir_name("wxid_abc"), "wxid_abc");
        assert_eq!(clean_account_dir_name("wxid_abc_12345"), "wxid_abc_12345"); // 5 位不是哈希尾
        assert_eq!(clean_account_dir_name("wxid_abc_12a_"), "wxid_abc_12a_");   // 空尾不切
        // 实现：哈希尾判定 = 恰好 4 位 ASCII 字母数字且前段非空（v1 原口径）。
        // 功能：钉死 wxid 还原规则，防止改坏后 --list-accounts 的 wxid 字段静默错位。
    }

    #[test]
    fn is_group_matches_chatroom() {
        assert!(is_group("9000000001@chatroom"));
        assert!(!is_group("wxid_abc"));
        // 功能：群聊判定锚点（@chatroom 含判）。
    }

    #[test]
    fn name_info_best_fallback_order() {
        let mut n = NameInfo { username: "wxid_x".into(), ..Default::default() };
        assert_eq!(n.best(), "wxid_x"); // 全空兜底 username
        n.nickname = "三哥".into();
        assert_eq!(n.best(), "三哥");
        n.remark = "张三".into();
        assert_eq!(n.best(), "张三"); // 备注 > 昵称
        // 实现：v1 的 best() 顺序是 备注>昵称>会话显示名>微信号>username。
        // 解析：注意这只是导出器内部诊断用；Python 侧展示回退（备注>昵称>微信号）是另一套口径。
        // 功能：防止移植时把 best 顺序写错。
    }

    #[test]
    fn parse_args_rejects_unknown_flag() {
        let a = parse_args(&["--data-root".to_string(), "X".to_string(), "--list-accounts".to_string(), "--typo".to_string()]);
        assert_eq!(a.unknown.as_deref(), Some("--typo"));
        let b = parse_args(&["D:/root".to_string(), "--list-sessions".to_string()]);
        assert_eq!(b.data_root.as_deref(), Some("D:/root"));
        assert!(b.list_sessions);
        // 实现：未知旗标进 unknown（main 报错退出码 2）；裸位置参数进 data_root。
        // 功能：钉住"未知旗标绝不静默吞掉"的 v1 口径。
    }

    #[test]
    fn session_rows_skip_blank_sid() {
        // list_session_rows 的过滤逻辑走真库才能测，这里只锚定 is_session_db 的判据。
        assert!(is_session_db(Path::new("D:/x/session/session.db")));
        assert!(!is_session_db(Path::new("D:/x/session/contact.db")));
        // 功能：load_names 展示名分流依据（session_display vs nickname）的判据锚点。
    }

    #[test]
    fn zstd_decode_hex_rejects_non_zstd() {
        // 非法输入四类：空串、奇数长度、非 hex 字符、非 zstd 魔数——全部 None。
        assert_eq!(zstd_decode_hex(""), None);
        assert_eq!(zstd_decode_hex("abc"), None);            // 奇数长度
        assert_eq!(zstd_decode_hex("zzzz"), None);           // 非 hex 字符
        let plain_hex: String = "hello world 明文XML".bytes().map(|b| format!("{b:02x}")).collect();
        assert_eq!(zstd_decode_hex(&plain_hex), None);       // 是合法 hex 但魔数不是 zstd 帧
        // 实现：前置校验（空/奇数/非hex）+ zstd 帧魔数（28 B5 2F FD）两道闸。
        // 解析：明文 XML（type 1/47/49 正文）必须原样保留，绝不能被这里误"解码"——
        //       测试用真实明文的 hex 化形态钉住"非 zstd → None"分支。
        // 功能：防止渲染层拿到被破坏的正文。
    }

    #[test]
    fn parse_args_messages_flags() {
        let a = parse_args(&[
            "--data-root".into(), "D:/r".into(),
            "--messages".into(), "--sid".into(), "wxid_x@chatroom".into(),
            "--begin".into(), "1700000000".into(), "--end".into(), "1700009999".into(),
        ]);
        assert!(a.messages && a.group_members == false);
        assert_eq!(a.sid.as_deref(), Some("wxid_x@chatroom"));
        assert_eq!(a.begin, 1700000000);
        assert_eq!(a.end, 1700009999);
        assert_eq!(a.recent, None);
        // 实现：--messages 路径的参数解析。
        // 功能：时间窗模式（begin/end，0=不限）与 recent 互斥取数的入口参数锚点。
    }

    #[test]
    fn parse_args_recent_mode() {
        let a = parse_args(&[
            "--data-root".into(), "D:/r".into(),
            "--messages".into(), "--sid".into(), "wxid_x".into(),
            "--recent".into(), "200".into(),
        ]);
        assert_eq!(a.recent, Some(200));
        assert_eq!(a.begin, 0); // recent 模式下 begin/end 应保持 0（未传）
        // 实现：--recent 解析成 usize；非法值保持 None（退回时间窗路径）。
        // 功能：最近 N 条模式的参数锚点。
    }

    #[test]
    fn voice_duration_ms_parses_compressed_and_plain() {
        // 明文 XML（未压缩正文）：直接取 voicelength。
        assert_eq!(voice_duration_ms(r#"<msg><voicevoip length="1" voicelength="3440" /></msg>"#), Some(3440));
        // 非法/缺失：None（时长缺省，不影响导出）。
        assert_eq!(voice_duration_ms("<msg>no attr</msg>"), None);
        assert_eq!(voice_duration_ms(""), None);
        // 实现：先试 zstd 解压再找 voicelength 属性的口径（v1 原样移植）。
        // 解析：zstd 压缩形态的真实字节需真库验证（步骤 12）；这里锚定明文路径与缺省路径，
        //       ruzstd 是纯解码器无法在单测里造压缩帧。
        // 功能：语音时长字段的解析锚点。
    }

    #[test]
    fn parse_args_voice_out() {
        let a = parse_args(&[
            "--data-root".into(), "D:/r".into(),
            "--messages".into(), "--sid".into(), "wxid_x".into(),
            "--voice-out".into(), "D:/tmp/voices".into(),
        ]);
        assert_eq!(a.voice_out.as_deref(), Some("D:/tmp/voices"));
        // 实现：--voice-out 带值旗标解析。
        // 功能：语音落盘开关的参数锚点。
    }

    #[test]
    fn current_login_picks_newest_session_db() {
        let root = std::env::temp_dir().join(format!("wxmsg_curlogin_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let old = root.join("wxid_old_1111").join("db_storage").join("session");
        let new = root.join("wxid_new_2222").join("db_storage").join("session");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&new).unwrap();
        // 顺序创建：后创建的 session.db mtime 更新 = 当前登录号
        std::fs::write(old.join("session.db"), b"x").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(30));
        std::fs::write(new.join("session.db"), b"x").unwrap();
        let (dir, wxid) = current_login(root.to_str().unwrap()).unwrap();
        assert_eq!(dir, "wxid_new_2222");
        assert_eq!(wxid, "wxid_new");
        // 非账号目录（无 session.db）不参与判定
        std::fs::create_dir_all(root.join("all_users")).unwrap();
        let (dir2, _) = current_login(root.to_str().unwrap()).unwrap();
        assert_eq!(dir2, "wxid_new_2222");
        let _ = std::fs::remove_dir_all(&root);
        // 实现：current_login 只看「含 session.db 的目录里 mtime 最新」——不需要密钥。
        // 解析：这是换号防线的判定源：新号就算没有钥也能被认出来（触发 Python 侧重扫）。
        // 功能：当前登录号探测的锚点。
    }
}
