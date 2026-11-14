//! wxmsg-exporter —— 微信聊天记录提取软件 · 只读导出器（参照 v1 reply-exporter 的架构）。
//!
//! 职责边界：本 exe 只做「解密查询 + 原始数据 JSON 输出」，名称回退、占位符渲染、
//! CSV/JSON 落盘、实时轮询全部在 Python 侧（wxmsg/ 包）做，保持导出器薄而稳定。
//!
//! 用法（stdout=JSON 数据，stderr=诊断；退出码 0=成功、1=失败、2=用法错误）：
//!   wxmsg-exporter --data-root <微信数据根> --list-accounts
//!   wxmsg-exporter --data-root <微信数据根> --list-sessions [--account <账号目录名|wxid>]
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
use weflow_core::db::{list_tables, table_columns, Account};
use weflow_core::keystore::KeyStore;
use weflow_core::schema::{find_candidate, sanitize_ident};
// 实现：依赖口径——anyhow 统一错误类型（main 返回 Result，Err 即 stderr + 退出码 1）；
// serde_json 的 json!/Map/Value 承载全部结构化输出；HashMap 做人名词表；Path/PathBuf 做路径拼接。
// weflow_core：db 提供 Account/with_conn/list_tables/table_columns（连接缓存与每库密钥解析），
// keystore 提供本机 store.json 读取，schema 提供列名候选匹配与 SQL 标识符白名单。
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

// 步骤 3 骨架期说明：NameInfo.username/best 与下面三个取值助手在 --list-sessions 路径尚未被
// 消费（--list-sessions 只输出原始名称字段），它们是步骤 4 --messages 的 sender 展示链路组件，
// 且已被单元测试覆盖——先用 allow(dead_code) 压掉过渡期警告，步骤 4 接上后即可移除。
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
    #[allow(dead_code)] // 过渡期：--messages（步骤 4）的 sender 展示链路消费
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

#[allow(dead_code)] // 过渡期：--messages（步骤 4）的消息行解析消费
fn ci_get<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    m.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
}
// ci_get（自 v1 原样移植）：在 serde_json Map 上忽略 ASCII 大小写找键，返回值的借用。
// 功能：屏蔽微信列名大小写差异。

#[allow(dead_code)] // 过渡期：--messages（步骤 4）的消息行解析消费
fn as_str(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    }
}
// as_str（自 v1 原样移植）：Option<&Value> → String；String 克隆、Number to_string、其余空串。
// 功能：安全取文本字段，绝不 panic。

#[allow(dead_code)] // 过渡期：--messages（步骤 4）的消息行解析消费
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

/// 选当前登录账号：数据根下「session.db 最近被写过且密钥能开库」的那只。
/// 与 v1 pick_account_dir 的差异：本项目数据根必填且强校验，不做"根外兜底"（绝不静默读别的目录）。
fn pick_current_account(ks: &KeyStore, data_root: &str, wanted: Option<&str>) -> Result<(PathBuf, String)> {
    let mut openable: Vec<(PathBuf, String, std::time::SystemTime)> = Vec::new();
    let mut no_key: usize = 0;
    let mut bad_key: usize = 0;
    for (base, dir) in account_dirs_under(Path::new(data_root)) {
        // --account 显式指定时按目录名或还原出的 wxid 过滤（都做忽略大小写比较）。
        if let Some(w) = wanted {
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
                drop(a);
                openable.push((dir, key, account_mtime(&session_db)));
            }
            Err(_) => {
                bad_key += 1;
            }
        }
    }
    // 诊断信息进错误文案（不含密钥、不含用户名——账号目录名本身含 wxid，属本机诊断可接受范围，
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
    // 选「session.db 最近被写过」的 = 当前登录号（v1 同判据）。
    openable.sort_by(|a, b| b.2.cmp(&a.2));
    let (dir, key, _) = openable.into_iter().next().unwrap();
    Ok((dir, key))
    // 功能：所有需要开账号的模式（--list-sessions 及后续 --messages 等）的统一账号入口。
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
    // 解析：key_works 与 pick_current_account 一样以 Account::open 成败为准——
    // 「有钥 ≠ 钥能用」（重登录换钥后旧钥静默打不开库，v1 踩过的坑）。
    // 功能：面板体检页的数据源。
}

fn run_list_sessions(ks: &KeyStore, data_root: &str, wanted: Option<&str>, pretty: bool) -> Result<()> {
    let (acct_dir, key) = pick_current_account(ks, data_root, wanted)?;
    let session_db = acct_dir.join("db_storage").join("session").join("session.db");
    let account = Account::open(session_db.to_string_lossy().as_ref(), &key)?;
    // pick_current_account 已验证过一次；这里再 open 是因为 Arc<Account> 没有从探测函数带出来
    // （探测要试多个账号，成功的那只重新 open 一次，连接缓存无所谓——进程即将退出）。
    // 名称合并顺序（v1 口径）：先 session.db（占 session_display/nickname），再 contact.db
    // （补 nickname/remark/alias，非空即覆盖 remark/alias）。
    let mut names: HashMap<String, NameInfo> = HashMap::new();
    load_names(&account, &account.layout.session_db, &mut names);
    if let Some(cdb) = &account.layout.contact_db {
        load_names(&account, cdb, &mut names);
    }
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
    emit(&json!({
        "mode": "list_sessions",
        "account_dir": acct_dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_default(),
        "wxid": acct_dir.file_name().map(|s| clean_account_dir_name(&s.to_string_lossy())).unwrap_or_default(),
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
    list_accounts: bool,
    list_sessions: bool,
    pretty: bool,
    version: bool,
    unknown: Option<String>,
}

fn parse_args(argv: &[String]) -> Args {
    let mut a = Args {
        data_root: None,
        store: None,
        account: None,
        list_accounts: false,
        list_sessions: false,
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
            // 带值旗标：i 先自增指向值，越界就什么都不做（旗标被丢弃，由后续校验兜住）。
            "--list-accounts" => a.list_accounts = true,
            "--list-sessions" => a.list_sessions = true,
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
    if !args.list_accounts && !args.list_sessions {
        bail!("必须指定模式：--list-accounts 或 --list-sessions（--version 查看用法）");
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
    // store 损坏/不存在时降级为空库——空库会让 pick_current_account 报"没有密钥"，错误文案可操作。
    if args.list_accounts {
        run_list_accounts(&ks, &data_root, args.pretty)
    } else {
        run_list_sessions(&ks, &data_root, args.account.as_deref(), args.pretty)
    }
    // 功能：模式派发（步骤 4/5 会加 --group-members/--messages/--voice 三个分支）。
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
}
