//! 通用查询层：SQL 值→JSON 归一、任意 SQL 执行、库路径解析。
//!
//! 协议依据 `docs/protocol/04-tables-misc.md` §wcdb_exec_query：
//! 返回"行对象数组"（非"列名+行"两段式）；文本→string、整数/浮点→number、
//! NULL→null、BLOB→hex 字符串（消费方 decodeVoiceBlob 接受 hex/base64）。
//! 空结果集必须是 `[]`。

use anyhow::{bail, Result};
use rusqlite::types::ValueRef;
use rusqlite::{Connection, Params, Statement};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use weflow_core::db::Account;

/// 单个 SQLite 值 → JSON。BLOB 走 hex（协议约定消费方按 hex/base64 解码）。
pub fn value_ref_to_json(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Number(i.into()),
        ValueRef::Real(f) => serde_json::Number::from_f64(f)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(hex::encode(b)),
    }
    // ① 实现方式：按 SQLite 动态类型五路分发——Null→null、Integer→i64 数字、Real→f64
    //    （from_f64 对 NaN/±inf 返回 None，兜底成 null）、Text→from_utf8_lossy 容错转
    //    字符串、Blob→hex 小写字符串。
    // ② 关键程序解析：JSON 没有 NaN/Inf，from_f64 失败必须兜底 null 否则序列化出错；
    //    Text 用 lossy 是因为微信库可能混入非法 UTF-8（坏字节替换成 U+FFFD），不能因
    //    一列坏字节丢整行；BLOB 走 hex 是协议约定（消费方 decodeVoiceBlob 接受 hex/base64），
    //    语音等二进制由此通道出境。get_ref 零拷贝借用到值，这里才发生一次性拷贝。
    // ③ 功能：把任意 SQL 值归一成协议约定的 JSON 表示，是行对象序列化的原子单元。
}

/// 执行已 prepare 的语句，返回行对象数组（key = SQL 输出列名/别名，原样保留）。
pub fn statement_rows_json(stmt: &mut Statement, params: impl Params) -> Result<Vec<Value>> {
    let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
    let mut rows = stmt.query(params)?;
    let mut out = Vec::new();
    // ① 实现方式：先把输出列名快照成 Vec<String>（含别名、原样保留大小写），再 query
    //    绑定参数拿到行迭代器，预建空结果 Vec；prepare 已由调用方完成。
    // ② 关键程序解析：列名必须在迭代前取——rows.next() 期间 stmt 被可变借用，不能再调
    //    column_names()；列名即 JSON 对象 key，同名输出列（如 JOIN 两表同名列）会互相
    //    覆盖（Map::insert 后写赢前写），消费方需用别名消歧。`?` 把绑定/执行错误冒泡给
    //    上层 emit_json 统一记日志。
    // ③ 功能：为"行对象数组"组装准备列名骨架与行游标。
    while let Some(row) = rows.next()? {
        let mut obj = Map::with_capacity(names.len());
        for (idx, name) in names.iter().enumerate() {
            obj.insert(name.clone(), value_ref_to_json(row.get_ref(idx)?));
        }
        out.push(Value::Object(obj));
    }
    // ① 实现方式：逐行迭代——每行按列数预分配 Map，按索引 get_ref 取值经 value_ref_to_json
    //    归一后以列名为 key 插入，整行包成 Value::Object 追加进 out。
    // ② 关键程序解析：get_ref 零拷贝借用避免每列 String 分配；取值错误经 `?` 中断整轮
    //    冒泡（不是跳过坏行）；全量物化进内存，超大结果集没有 LIMIT 保护，靠 TS 侧自律。
    // ③ 功能：把结果集逐行转成 JSON 对象数组，空结果集得到空 Vec。
    Ok(out)
    // ① 实现方式：返回物化后的行数组。
    // ② 关键程序解析：空集也走 Ok(空 Vec)，由上游序列化成 "[]"，满足"rc==0 时 outJson
    //    非空"协议；rows/stmt 函数结束即 Drop，读游标不长期占用连接。
    // ③ 功能：交付行对象数组。
}

/// 无参查询便捷封装。
pub fn query_rows_json(conn: &Connection, sql: &str) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(sql)?;
    statement_rows_json(&mut stmt, [])
    // ① 实现方式：prepare 一次 SQL（无参数占位），委托 statement_rows_json 以空参数集执行。
    // ② 关键程序解析：静态 SQL 便捷封装，prepare 错误（语法错/表不存在）在此冒泡；
    //    stmt 是局部变量函数返回即 Drop，不泄漏语句句柄；带参查询应直接用
    //    statement_rows_json 走 ? 绑定，别拼字符串。
    // ③ 功能：给无参 SELECT（表清单、PRAGMA 类）提供一行式行数组查询。
}

/// 按 kind + 显式路径解析目标库文件；路径为空时回退到 handle 账号目录下该 kind 的默认库
/// （协议 04：TS 找不到文件会传空串，DLL 需自行定位）。
pub fn resolve_db_path(account: &Account, kind: &str, path: &str) -> Result<PathBuf> {
    let trimmed = path.trim();
    if !trimmed.is_empty() {
        let p = PathBuf::from(trimmed);
        if p.is_file() {
            return Ok(p);
        }
        // 显式路径给了但文件不存在：仍返回它，让 open 报"数据库无法打开"
        return Ok(p);
    }
    // ① 实现方式：入参路径 trim 后非空即优先采用——转 PathBuf 后无论 is_file() 与否都
    //    原样返回（两个分支返回值相同）；空串才走 kind 默认库回退。
    // ② 关键程序解析：is_file() 命中与否都 return Ok(p)，这个 if 实际是**冗余死检查**
    //    （行为与直接 return 等价），留着只为表达"检查过"的意图；文件不存在时故意不拦，
    //    让后续 open_sqlcipher 报"数据库无法打开"而不是这里静默换库。协议 04：TS 找不到
    //    文件会传空串，由 DLL 自行定位。
    // ③ 功能：实现"显式路径优先、空路径回退默认库"的解析策略。
    let layout = &account.layout;
    let fallback = match kind.trim().to_ascii_lowercase().as_str() {
        "session" => Some(layout.session_db.clone()),
        "contact" => layout.contact_db.clone(),
        "emoticon" => layout.emoticon_db.clone(),
        "sns" => layout.sns_db.clone(),
        "hardlink" => layout.hardlink_db.clone(),
        "message" => layout.message_dbs.first().cloned(),
        "media" => layout.media_dbs.first().cloned(),
        _ => None,
    };
    // ① 实现方式：kind trim+转小写后精确匹配七类——session/contact/emoticon/sns/hardlink
    //    取单库字段，message/media 取分库 Vec 的 first()，未知 kind 得 None。
    // ② 关键程序解析：message/media 只取**第一个分库**（如 message_0.db），不是全部——
    //    通用查询入口一次只查一个库，跨分库聚合是 messages 模块的职责；layout 是 open 时
    //    discover_layout 的快照，微信后来新建的分库这里看不到；单库字段为 None（本机缺失）
    //    时与未知 kind 一样落空。
    // ③ 功能：把业务 kind 映射到账号布局里的默认库路径。
    match fallback {
        Some(p) => Ok(p),
        None => bail!("无法解析 kind={kind} 的默认库路径"),
    }
    // ① 实现方式：Some 直接返回路径；None（未知 kind 或该类库缺失）bail 错误，
    //    错误串只含 kind 名不含账号路径。
    // ② 关键程序解析：错误经 emit_json 记日志并上抛错误码；"contact 库缺失"与"kind 拼错"
    //    在这里不可区分、都是同一句 bail——排障时要看 layout summary 而不是这条错误。
    // ③ 功能：完成路径解析的最后收敛，失败给出可诊断的错误。
}

/// 通用 SQL 逃生门：解析库路径 → 执行 → 行对象数组 JSON 字符串。
pub fn exec_query_json(account: &Account, kind: &str, path: &str, sql: &str) -> Result<String> {
    let db = resolve_db_path(account, kind, path)?;
    let rows = account.with_conn(&db, |conn| query_rows_json(conn, sql))?;
    Ok(Value::Array(rows).to_string())
    // ① 实现方式：三步链——resolve_db_path 定位库 → with_conn 取（或建）连接并在内层
    //    Mutex 锁内执行 query_rows_json → 行数组包 Value::Array 序列化成 JSON 字符串。
    // ② 关键程序解析：这是给 TS 的"任意 SQL 逃生门"，sql 原样 prepare，**没有语句类型
    //    白名单**——只读约束靠调用方只传 SELECT/PRAGMA 自律（连接本身按读写口径打开，
    //    见 open_sqlcipher 的已知问题）；with_conn 内层 Mutex 不可重入，回调里不能再对
    //    同一库调 with_conn 否则死锁；任一步错误 `?` 冒泡由 emit_json 统一收敛。
    // ③ 功能：在指定业务库上执行通用查询并返回行对象数组 JSON。
}

/// 便捷：把库路径标准化为正斜杠绝对路径字符串（消费方直接 basename / 文件存在性检查）。
pub fn path_to_json_string(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
    // ① 实现方式：路径 lossy 转字符串（非 UTF-8 字节替换 U+FFFD）后把反斜杠统一成正斜杠。
    // ② 关键程序解析：不做 canonicalize/绝对化——doc 注释写"绝对路径"但实际只统一分隔符，
    //    相对路径进来就原样出去（实践中 layout 路径源自 TS 传入的绝对 session 路径，故恒绝对）；
    //    正斜杠让 TS 侧 basename/存在性检查口径跨平台一致，也避免 JSON 里双反斜杠转义。
    // ③ 功能：产出消费方友好的库路径字符串（各 JSON 出参里的路径统一口径）。
}
