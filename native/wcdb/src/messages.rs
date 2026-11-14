//! 消息读写函数组（docs/protocol/01-core-sessions-messages.md §11-§21）。
//!
//! schema 未定档（需 dbKey 用 <微信数据根> 校准，任务 #7），因此采取
//! "SELECT * + 候选列名归一"策略：原样保留数据库列，同时补齐协议要求的 snake_case
//! 输出键（local_id/server_id/create_time/... 与 computed_is_send），最大化兼容 TS
//! 消费方的候选字段回退。

use crate::ffi::*;
use crate::{push_log, with_account};
use crate::query::path_to_json_string;
use anyhow::Result;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use weflow_core::db::{list_tables, table_columns, Account};
use weflow_core::schema::{find_candidate, sanitize_ident, SessionTables};

// 逻辑字段 → 数据库真实列名候选（大小写不敏感，取第一个命中）
const C_LOCAL_ID: &[&str] = &["local_id", "localId", "id", "msg_local_id", "rowid"];
pub(crate) const C_SERVER_ID: &[&str] = &["server_id", "serverId", "svr_id", "svrid", "msg_server_id", "server_msg_id"];
pub(crate) const C_LOCAL_TYPE: &[&str] = &["local_type", "localType", "type", "msg_type", "message_type"];
const C_STATUS: &[&str] = &["status", "msg_status", "local_status"];
pub(crate) const C_CREATE_TIME: &[&str] = &["create_time", "createTime", "msg_create_time", "create_timestamp", "time", "msg_time", "timestamp"];
const C_SORT_SEQ: &[&str] = &["sort_seq", "sortSeq", "sequence", "seq", "msg_seq"];
const C_CONTENT: &[&str] = &["message_content", "messageContent", "content", "str_content", "msg_content"];
const C_COMPRESS: &[&str] = &["compress_content", "compressContent", "compressed_content"];
pub(crate) const C_IS_SEND: &[&str] = &["is_send", "isSend", "send", "is_sender"];
pub(crate) const C_SENDER: &[&str] = &["sender_username", "sender", "str_sender", "sender_id", "talker"];

// 协议输出键 → 候选源列
const FIELD_MAP: &[(&str, &[&str])] = &[
    ("local_id", C_LOCAL_ID),
    ("server_id", C_SERVER_ID),
    ("local_type", C_LOCAL_TYPE),
    ("status", C_STATUS),
    ("create_time", C_CREATE_TIME),
    ("sort_seq", C_SORT_SEQ),
    ("message_content", C_CONTENT),
    ("compress_content", C_COMPRESS),
    ("is_send", C_IS_SEND),
    ("sender_username", C_SENDER),
];

// 会话取数地址（见下方 ①②③ 说明）
pub(crate) struct TableRef {
    pub(crate) db_path: PathBuf,
    pub(crate) table: String,
    pub(crate) filter: Option<(String, String)>,
}
// ① 实现方式：三个字段——db_path 是消息库文件路径、table 是分表名、filter 是共享大表时
//    的 (会话列名, 会话 id) 等值过滤对；专表布局时 filter 为 None。
// ② 关键程序解析：微信消息有两种落库布局——按会话独立分表（专表）与多会话共表（靠
//    talker 列区分）；本结构把两种统一成"库+表+过滤"三元组，fetch/count 只需按有无
//    filter 拼 WHERE，上层完全不感知布局差异。
// ③ 功能：描述"到哪张表取该会话的消息"的最小寻址信息。

pub(crate) fn session_table_refs(account: &Account, session_id: &str) -> Result<Vec<TableRef>> {
    // 先取消息索引（分表路由器），把会话 id 展开成具体"库+表"引用列表
    let index = account.msg_index()?;
    let mut out = Vec::new();
    match index.session_tables(account, session_id)? {
        SessionTables::Located(list) => {
            // 专表布局：每个 (库序号, 表名) 天然只装本会话，无需过滤条件
            for (db_idx, table) in list {
                if let Some(db) = index.db_path(db_idx) {
                    out.push(TableRef { db_path: db.to_path_buf(), table, filter: None });
                }
            }
        }
        SessionTables::Shared { tables, talker_column } => {
            // 共表布局：同表混装多会话，须带 talker 列等值过滤才能定位本会话
            for (db_idx, table) in tables {
                if let Some(db) = index.db_path(db_idx) {
                    out.push(TableRef {
                        db_path: db.to_path_buf(),
                        table,
                        filter: Some((talker_column.clone(), session_id.to_string())),
                    });
                }
            }
        }
    }
    // 无 Msg_ 表 = 该会话合法地 0 条消息（历史已清空/仅临时内容/公众号等）。
    // 返回空引用列表，让上层得到 [] / count=0，而非 schema mismatch 错误。
    Ok(out)
    // ① 实现方式：msg_index 路由出 SessionTables 两种布局（Located 专表 / Shared 共表），
    //    逐个配上 db_path 收进 Vec；共表额外携带 (talker_column, session_id) 过滤对。
    // ② 关键程序解析：db_path 由 index.db_path(db_idx) 给出，取不到（库文件缺失/未挂载）
    //    时静默跳过该表——宁可少算不整体报错；filter 只存"列名+值"，SQL 拼接推迟到
    //    fetch_table/count 处统一做 sanitize；空列表按上面注释语义返回合法 0 条。
    // ③ 功能：把会话 id 解析为若干"库文件+表名+过滤条件"的取数地址（消息组总入口）。
}

pub(crate) fn ci_get<'a>(map: &'a Map<String, Value>, key_lower: &str) -> Option<&'a Value> {
    map.iter().find(|(k, _)| k.eq_ignore_ascii_case(key_lower)).map(|(_, v)| v)
    // ① 实现方式：线性扫描键集，eq_ignore_ascii_case 做 ASCII 大小写不敏感匹配，命中
    //    第一个同义键即返回其值的引用。
    // ② 关键程序解析：SELECT * 原样保留数据库列名，而真实库列名大小写混乱
    //    （Local_Id/localId/…），所以一切字段读取都必须大小写不敏感；候选键均为 ASCII
    //    标识符，O(n) 扫描对十来个键的开销可忽略。
    // ③ 功能：在保留原始列名的行对象里按大小写不敏感取字段（消息/统计两组共用）。
}

pub(crate) fn as_i64(v: &Value) -> i64 {
    match v {
        // Number 先试整型，失败退浮点截断（时间戳×1000 等经 JSON 序列化可能成浮点）
        Value::Number(n) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        // 字符串数值（老库把时间存文本）去首尾空白后 parse，失败归 0
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
    // ① 实现方式：三分支宽容转换——Number→i64（兜底 f64 截断）、String→trim+parse、
    //    其余类型一律 0。
    // ② 关键程序解析：SQLite 数值列经 JSON 序列化可能带小数位，as_i64 直接失败所以必须
    //    补 as_f64 兜底；解析失败统一归 0，让上层按"无值"处理而非报错——时间/序号/类型
    //    等所有数值字段读取都复用本函数，宽容性是刻意设计。
    // ③ 功能：把任意 JSON 值宽容转成 i64（消息组数值字段的统一入口）。
}

// Name2Id 名称列候选（该库 rowid → user_name 解析真实发送者）
const N2I_NAME: &[&str] = &["user_name", "username", "user_name_", "name"];

fn n2i_cache() -> &'static Mutex<HashMap<String, Arc<HashMap<i64, String>>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Arc<HashMap<i64, String>>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
    // ① 实现方式：函数内 static OnceLock 惰性建全局映射表，键=库文件路径（归一后小写），
    //    值=Arc 包裹的 rowid→user_name 表。
    // ② 关键程序解析：路径作键保证不同消息库各存一份映射；Arc 让多个归一行共享同一份
    //    而不深拷贝；Mutex 只护增删，命中方 clone Arc 后即可无锁读取。
    // ③ 功能：进程级 Name2Id 缓存槽，避免逐条消息归一时反复读 Name2Id 表。
}

/// 该消息库的 Name2Id：rowid → user_name（缓存；失败不缓存以便重试）。
fn name2id_map(account: &Account, db: &Path) -> Option<Arc<HashMap<i64, String>>> {
    let key = db.to_string_lossy().replace('\\', "/").to_lowercase();
    // 缓存键：库路径归一（\→/、统一小写），同一文件的不同写法命中同一项
    if let Ok(g) = n2i_cache().lock() {
        // 命中缓存直接 Arc::clone 共享，零 SQL 开销
        if let Some(hit) = g.get(&key) {
            return Some(Arc::clone(hit));
        }
    }
    // 列出该库全部表：先精确匹配 Name2Id，再放宽到"表名含 name2id"（大小写变体）
    let tables = account.with_conn(db, |c| list_tables(c)).ok()?;
    let table = tables
        .iter()
        .find(|t| t.eq_ignore_ascii_case("Name2Id"))
        .or_else(|| tables.iter().find(|t| t.to_ascii_lowercase().contains("name2id")))?
        .clone();
    let map: HashMap<i64, String> = account
        .with_conn(db, |c| -> Result<HashMap<i64, String>> {
            let cols = table_columns(c, &table)?;
            // 名称列同样按候选探测（user_name/username/…），缺列即构建失败
            let name_col =
                find_candidate(&cols, N2I_NAME).ok_or_else(|| anyhow::anyhow!("Name2Id 无名称列"))?;
            let sql = format!(
                "SELECT rowid, \"{}\" FROM \"{}\"",
                sanitize_ident(&name_col)?,
                sanitize_ident(&table)?
            );
            // 表名/列名均已 sanitize_ident 后入 SQL；整表读 (rowid, 名称) 二元组
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
            // flatten 丢弃取值失败的行：宽容收集，个别坏行不影响整表映射
            Ok(rows.flatten().collect())
        })
        .ok()?;
    // 只有构建成功才走到这里写缓存；失败路径已提前返回 None、不入缓存，便于下次重试
    let arc = Arc::new(map);
    if let Ok(mut g) = n2i_cache().lock() {
        g.insert(key, Arc::clone(&arc));
    }
    Some(arc)
    // ① 实现方式：缓存命中直接返回 → 未命中则 list_tables 定位 Name2Id 表 → with_conn
    //    读全表 (rowid, 名称列) 构建 HashMap → Arc 包裹写回全局缓存并返回。
    // ② 关键程序解析：rowid 是 SQLite 隐式主键，行的 real_sender_id 存的正是它，据此
    //    O(1) 解析出 user_name 补 sender_username；Arc 让同一库的所有行共享一份映射；
    //    任何一步失败（无表/无名称列/SQL 错）都 .ok()? 退 None——调用方只是拿不到
    //    发送者名，不会拖垮整体归一。
    // ③ 功能：取某消息库的 Name2Id 映射（rowid→user_name），带进程级缓存。
}

/// 归一单行：保留原始列，补齐协议 snake_case 键与 computed_is_send。
pub(crate) fn normalize_row(
    raw: Value,
    db_path: &str,
    table: &str,
    my_id: Option<&str>,
    account: &Account,
    db: &Path,
) -> Value {
    // 非 object 行（理论少见）包成 {"value": ...}，保证输出恒为 JSON 对象
    let mut map = match raw {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("value".to_string(), other);
            m
        }
    };
    // 先快照键集再改 map，避免"边遍历边插入"的同对象借用冲突
    let keys: Vec<String> = map.keys().cloned().collect();
    for (proto, cands) in FIELD_MAP {
        if ci_get(&map, proto).is_some() {
            continue; // 原始列已叫协议名
        }
        // 按候选找源列
        let mut found: Option<Value> = None;
        for cand in *cands {
            if let Some(k) = keys.iter().find(|k| k.eq_ignore_ascii_case(cand)) {
                if let Some(v) = map.get(k) {
                    found = Some(v.clone());
                    break;
                }
            }
        }
        // 命中候选列：把源列值克隆一份挂到协议键名下（原列保留不动）
        if let Some(v) = found {
            map.insert(proto.to_string(), v);
        }
    }
    // 真实 schema 常无 sender_username/is_send，方向靠 real_sender_id 指向本库 Name2Id 解析
    let has_sender = matches!(ci_get(&map, "sender_username"), Some(Value::String(s)) if !s.is_empty());
    if !has_sender {
        let rid = ci_get(&map, "real_sender_id")
            .or_else(|| ci_get(&map, "realSenderId"))
            .map(as_i64)
            .unwrap_or(0);
        // rid 非 0 才查 Name2Id；let 链逐层降级（camelCase 变体→数值化→默认 0）
        if rid != 0 {
            if let Some(n2i) = name2id_map(account, db) {
                if let Some(uname) = n2i.get(&rid) {
                    map.insert("sender_username".to_string(), Value::String(uname.clone()));
                }
            }
        }
    }
    // computed_is_send：优先 is_send，否则用 sender==my_id 推断
    let computed = match ci_get(&map, "is_send") {
        Some(v) => v.clone(),
        None => match (ci_get(&map, "sender_username"), my_id) {
            (Some(Value::String(s)), Some(me)) if !me.is_empty() => {
                Value::Number(((s == me) as i64).into())
            }
            _ => Value::Null,
        },
    };
    map.insert("computed_is_send".to_string(), computed);
    // 追加来源元数据（库路径/表名/库文件名），供 TS 写回定位与分文件导出使用
    map.insert("db_path".to_string(), Value::String(db_path.to_string()));
    map.insert("table_name".to_string(), Value::String(table.to_string()));
    if let Some(name) = PathBuf::from(db_path)
        .file_stem()
        .and_then(|s| s.to_str())
    {
        map.insert("db_name".to_string(), Value::String(name.to_string()));
    }
    Value::Object(map)
    // ① 实现方式：SELECT * 原始列全保留 → FIELD_MAP 按"协议键←候选源列"补 snake_case
    //    输出键 → 缺 sender_username 时用 real_sender_id 查本库 Name2Id 补齐 → 计算
    //    computed_is_send → 追加 db_path/table_name/db_name 来源三元组。
    // ② 关键程序解析：原始列与协议键并存是"候选列回退"策略的核心——TS 拿不到标准键时
    //    还有原始列可用；computed_is_send 优先吃 is_send 列，否则 sender==my_id 推断，
    //    都没有则 Null（消费方按未知处理）；db_name 取库文件词干，便于日志与导出归档。
    // ③ 功能：把任一消息库的原始行归一成协议形状（方向判定、发送者、来源定位齐备）。
}

pub(crate) fn sort_key(v: &Value) -> i128 {
    let map = match v { Value::Object(m) => m, _ => return 0 };
    if let Some(sq) = ci_get(map, "sort_seq") {
        // sort_seq 优先；0 视为缺失（部分老库该列恒 0），回退 create_time
        let n = as_i64(sq);
        if n != 0 {
            return n as i128;
        }
    }
    if let Some(ct) = ci_get(map, "create_time") {
        // create_time 兜底：秒×1000 抬到与 sort_seq（≈毫秒序号）同量级再比
        return as_i64(ct) as i128 * 1000;
    }
    0
    // ① 实现方式：i128 排序键——sort_seq 非 0 直接采用，否则 create_time*1000，都没有为 0。
    // ② 关键程序解析：用 i128 防大序号与 *1000 相乘溢出；两列混合排序必须同量级，否则
    //    时间兜底行会整体沉底/浮顶，跨表合并顺序失真；键为 0 的行按排序方向稳定落在
    //    最前或最后，不会参与无序抖动。
    // ③ 功能：为跨分表合并后的行提供统一可比较的排序键（§11/§12 与游标 collect_rows 共用）。
}

/// 取一张表的行（SELECT * + 可选 WHERE + ORDER BY 时间列 + LIMIT）。
pub(crate) fn fetch_table(
    account: &Account,
    t: &TableRef,
    extra_where: Option<(&str, Vec<String>)>,
    ascending: bool,
    limit: i64,
) -> Result<Vec<Value>> {
    // 列快照：用于探测本表的排序时间列（sort_seq 优先，缺则退 create_time）
    let cols = account.columns(&t.db_path, &t.table)?;
    let time_col = find_candidate(&cols, C_SORT_SEQ)
        .or_else(|| find_candidate(&cols, C_CREATE_TIME));
    // 表名 sanitize 后入 SQL；WHERE 片段与 ? 绑定参数成对收集，最后统一拼装
    let safe_table = sanitize_ident(&t.table)?;
    let mut wheres: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    // 共表过滤：talker 列等值定位本会话（专表 filter 为 None，跳过）
    if let Some((col, val)) = &t.filter {
        wheres.push(format!("\"{}\" = ?", sanitize_ident(col)?));
        params.push(val.clone());
    }
    // 调用方附加条件（列片段由调用方拼好，值仍走 ? 绑定通道）
    if let Some((col, vals)) = &extra_where {
        wheres.push(col.to_string());
        params.extend(vals.iter().cloned());
    }
    // 多条件 AND 连接；无条件则省略 WHERE 子句
    let where_sql = if wheres.is_empty() { String::new() } else { format!(" WHERE {}", wheres.join(" AND ")) };
    // 排序列经 sanitize 后拼 ORDER BY，方向随 ascending；未探测到列则按库自然序
    let order_sql = match &time_col {
        Some(tc) => format!(
            " ORDER BY \"{}\" {}",
            sanitize_ident(tc)?,
            if ascending { "ASC" } else { "DESC" }
        ),
        None => String::new(),
    };
    // limit<=0 表示不设上限（取全表，游标/统计路径用）
    let limit_sql = if limit > 0 { format!(" LIMIT {limit}") } else { String::new() };
    // SELECT *：schema 未定档策略，原样保留全部列交 normalize_row 归一
    let sql = format!("SELECT * FROM \"{safe_table}\"{where_sql}{order_sql}{limit_sql}");
    account.with_conn(&t.db_path, |conn| {
        // 参数按位置绑定（均为文本；SQLite 会做数值亲和比较）
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
        let rows = crate::query::statement_rows_json(&mut stmt, refs.as_slice())?;
        Ok(rows)
    })
    // ① 实现方式：探测排序列 → 拼 SELECT * + WHERE + ORDER BY + LIMIT → with_conn 执行
    //    → statement_rows_json 把每行转成 JSON 对象（列名原样保留）。
    // ② 关键程序解析：标识符全部 sanitize_ident、值全部 ? 绑定，注入面为零；文本绑定
    //    配合 SQLite 亲和可正确比较数值列；SQL 层不做 OFFSET 分页——多表合并分页由
    //    调用方按 per=offset+limit 预取实现（见 §11/§12 注释）。
    // ③ 功能：从单张消息表按条件、方向、上限取行，是消息组一切取数的底层。
}

// ── §11 wcdb_get_messages ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_messages(
    handle: i64,
    username: *const c_char,
    limit: i32,
    offset: i32,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(username);
        // 立即闭包：业务逻辑集中返回 Result，错误统一交 emit_json 收敛
        let result = (|| -> Result<String> {
            let refs = session_table_refs(account, sid)?;
            // my_id：本账号 id（computed_is_send 推断用），未设置时为 None
            let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());
            // limit/offset 负值按 0 钳制（协议未定义负数，纯防御）
            let lim = limit.max(0) as i64;
            let off = offset.max(0) as i64;
            // 每表最多取 offset+limit 行，合并后按时间降序排序再切片
            let per = off + lim;
            let mut all: Vec<Value> = Vec::new();
            // 逐表降序预取 per=off+lim 行并归一（合并分页策略：每表各取同长度前缀）
            for t in &refs {
                let rows = fetch_table(account, t, None, false, per.max(1))?;
                let dbp = path_to_json_string(&t.db_path);
                for r in rows {
                    all.push(normalize_row(r, &dbp, &t.table, my_id.as_deref(), account, &t.db_path));
                }
            }
            // 合并后全局按 sort_key 降序（新→旧），再统一切片实现跨表 offset/limit
            all.sort_by(|a, b| sort_key(b).cmp(&sort_key(a)));
            let sliced: Vec<Value> = all.into_iter().skip(off as usize).take(lim.max(0) as usize).collect();
            Ok(Value::Array(sliced).to_string())
        })();
        emit_json(out_json, result, ERR_SCHEMA_MISMATCH)
        // ① 实现方式：with_account → 路由会话表 → 每表降序取 off+lim 行 → normalize_row
        //    归一 → 合并全局降序排序 → skip(off).take(lim) 切片 → JSON 数组经 emit_json 写出。
        // ② 关键程序解析：跨多张分表无法用单条 SQL 分页，故每表各取前 offset+limit 条——
        //    全局排序后的前 off+lim 条必然都在这些前缀里，切片结果与单表语义一致；
        //    schema/路由类失败统一返回 ERR_SCHEMA_MISMATCH(-7)，与"合法空结果 []"区分。
        // ③ 功能：§11 取某会话最新消息列表（offset+limit 分页，新→旧）。
    })
}

// ── §12 wcdb_get_messages_by_type ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_messages_by_type(
    handle: i64,
    session_id: *const c_char,
    local_type: i64,
    ascending: i32,
    limit: i32,
    offset: i32,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        // 立即闭包：业务逻辑集中返回 Result，错误统一交 emit_json 收敛
        let result = (|| -> Result<String> {
            let refs = session_table_refs(account, sid)?;
            // my_id 与 §11 同源；asc/lim/off/type_val 依次布尔化、钳负、转文本绑定值
            let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());
            let asc = ascending != 0;
            let lim = limit.max(0) as i64;
            let off = offset.max(0) as i64;
            let type_val = local_type.to_string();
            let mut all: Vec<Value> = Vec::new();
            for t in &refs {
                let cols = account.columns(&t.db_path, &t.table)?;
                // 无 local_type 列的表装不了本类型消息，直接跳过
                let Some(tc) = find_candidate(&cols, C_LOCAL_TYPE) else { continue };
                // 类型等值条件：列名 sanitize，值以 ? 绑定（文本形态，SQLite 亲和可比整型列）
                let cond = format!("\"{}\" = ?", sanitize_ident(&tc)?);
                // limit=0 表示不限量；为合并分页仍取足量
                let per = if lim > 0 { off + lim } else { 0 };
                let rows = fetch_table(account, t, Some((&cond, vec![type_val.clone()])), asc, per)?;
                let dbp = path_to_json_string(&t.db_path);
                for r in rows {
                    all.push(normalize_row(r, &dbp, &t.table, my_id.as_deref(), account, &t.db_path));
                }
            }
            // 按调用方要求的方向做全局排序
            all.sort_by(|a, b| if asc { sort_key(a).cmp(&sort_key(b)) } else { sort_key(b).cmp(&sort_key(a)) });
            // lim>0 才 take 定量；limit=0 只 skip(off) 取剩余全部（协议"不限量"语义）
            let sliced: Vec<Value> = if lim > 0 {
                all.into_iter().skip(off as usize).take(lim as usize).collect()
            } else {
                all.into_iter().skip(off as usize).collect()
            };
            Ok(Value::Array(sliced).to_string())
        })();
        emit_json(out_json, result, ERR_SCHEMA_MISMATCH)
        // ① 实现方式：与 §11 同构（逐表预取→归一→合并排序→切片），差异：WHERE 追加
        //    local_type=? 条件、排序方向随 ascending、limit=0 表示取剩余全部。
        // ② 关键程序解析：per=off+lim 的多表合并分页理由同 §11；排序下推给 SQLite
        //    （fetch_table 的 ORDER BY 按同方向取出），Rust 侧合并时只做归并级比较；
        //    单表无该类型列则整体跳过，不影响其他分表的结果。
        // ③ 功能：§12 按会话+消息类型取消息（方向可选、limit=0 不限量）。
    })
}

// ── §13 wcdb_get_message_count ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_count(
    handle: i64,
    username: *const c_char,
    out_count: *mut i32,
) -> i32 {
    with_account(handle, |account| {
        // 出参防御：NULL 直接失败；非 NULL 先清 0，保证失败路径调用方也读到 0
        if out_count.is_null() {
            return ERR_GENERIC;
        }
        unsafe { *out_count = 0 };
        let sid = cstr(username);
        // 立即闭包：逐表 COUNT 后累加，错误收敛给下方 match
        let result = (|| -> Result<i64> {
            let refs = session_table_refs(account, sid)?;
            let mut total = 0i64;
            for t in &refs {
                let safe_table = sanitize_ident(&t.table)?;
                // 共表布局拼 talker 等值 WHERE；专表无过滤直接 COUNT 全表
                let (where_sql, param) = match &t.filter {
                    Some((col, val)) => (format!(" WHERE \"{}\" = ?", sanitize_ident(col)?), Some(val.clone())),
                    None => (String::new(), None),
                };
                // COUNT(*) 走 SQLite 聚合，比 fetch 全行再归一轻得多
                let sql = format!("SELECT COUNT(*) FROM \"{safe_table}\"{where_sql}");
                let c = account.with_conn(&t.db_path, |conn| -> Result<i64> {
                    let mut stmt = conn.prepare(&sql)?;
                    // 有无绑定参数两个分支：共表带 talker 值，专表用空参数列表
                    let n = if let Some(p) = &param {
                        stmt.query_row([p.as_str()], |r| r.get::<_, i64>(0))?
                    } else {
                        stmt.query_row([], |r| r.get::<_, i64>(0))?
                    };
                    Ok(n)
                })?;
                total += c;
            }
            Ok(total)
        })();
        // 成功：i64 钳进 i32 正范围写回出参；失败：日志留痕并返回 -7
        match result {
            Ok(n) => {
                let clamped = n.clamp(0, i32::MAX as i64) as i32;
                unsafe { *out_count = clamped };
                OK
            }
            Err(e) => {
                push_log(&format!("get_message_count: {e}"));
                ERR_SCHEMA_MISMATCH
            }
        }
        // ① 实现方式：out_count 清零 → 逐表 SELECT COUNT(*)（共表带 talker 过滤）累加 →
        //    clamp 钳进 i32 写回出参；Err 分支 push_log 后返回 ERR_SCHEMA_MISMATCH。
        // ② 关键程序解析：i64→i32 用 clamp 而非 as 直转，防超 21 亿条时回绕成负数（纯
        //    防御）；无消息表的会话路由产出空引用列表、自然计 0，走不到错误分支；
        //    -7 与 fetch 组一致表达"schema/路由层面失败"，TS 提示口径统一。
        // ③ 功能：§13 统计某会话消息总数（跨分表求和）。
    })
}

// ── §14/§15 单条查询（by local_id / by server_id） ──

fn find_one(account: &Account, sid: &str, cands: &[&str], value: &str) -> Result<Option<Value>> {
    // 与列表接口同一路由；my_id 供归一时推断方向
    let refs = session_table_refs(account, sid)?;
    let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());
    for t in &refs {
        let cols = account.columns(&t.db_path, &t.table)?;
        // 该表没有目标 id 列（候选见 C_LOCAL_ID/C_SERVER_ID）则跳过
        let Some(idc) = find_candidate(&cols, cands) else { continue };
        // id 等值条件：列名 sanitize、值 ? 绑定；LIMIT 1 只取首行
        let cond = format!("\"{}\" = ?", sanitize_ident(&idc)?);
        let rows = fetch_table(account, t, Some((&cond, vec![value.to_string()])), false, 1)?;
        // 命中即归一并返回（跨表 id 异常冲突时按路由顺序先到先得，结果确定可复现）
        if let Some(r) = rows.into_iter().next() {
            let dbp = path_to_json_string(&t.db_path);
            return Ok(Some(normalize_row(r, &dbp, &t.table, my_id.as_deref(), account, &t.db_path)));
        }
    }
    Ok(None)
    // ① 实现方式：遍历会话全部表引用 → 探测 id 列 → WHERE id=? LIMIT 1 → 命中即归一为
    //    Some 返回，全部落空返回 Ok(None)。
    // ② 关键程序解析：local_id 只在单表内唯一、server_id 跨表唯一，先到先得策略在异常
    //    冲突时仍有确定结果；无 id 列的表跳过而非报错（路由结果里可能混入非消息表）。
    // ③ 功能：§14/§15 共用的"按 id 在会话各分表中定位单条消息"。
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_by_id(
    handle: i64,
    session_id: *const c_char,
    local_id: i32,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        // 立即闭包：未命中是合法结果（OK + "{}"），只有内部错误才 Err
        let result = (|| -> Result<String> {
            // i32 local_id 转文本走 fetch_table 统一绑定通道（SQLite 亲和正确比较）
            match find_one(account, sid, C_LOCAL_ID, &local_id.to_string())? {
                Some(row) => Ok(row.to_string()),
                None => Ok("{}".to_string()),
            }
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：find_one 按 C_LOCAL_ID 候选列查 local_id；命中输出整行归一 JSON，
        //    未命中输出 "{}" 且 rc 仍为 0。
        // ② 关键程序解析："查无此消息"按协议不是错误——TS 用同一解析路径处理命中/未命中；
        //    失败码用 ERR_GENERIC(-1)，单条查询不涉及分页/批量语义，与 -7 场景区分。
        // ③ 功能：§14 按本地 id 取单条消息。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_by_svrid(
    handle: i64,
    session_id: *const c_char,
    svrid: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        // svrid 去首尾空白；TS 常用 "0" 占位"无服务端 id"，与空串一并按未提供处理
        let svr = cstr(svrid).trim().to_string();
        let result = (|| -> Result<String> {
            // 哨兵值直接短路返回空对象，避免无意义的全分表扫描
            if svr.is_empty() || svr == "0" {
                return Ok("{}".to_string());
            }
            // server_id 候选列覆盖 svrid/svr_id 等真实命名变体
            match find_one(account, sid, C_SERVER_ID, &svr)? {
                Some(row) => Ok(row.to_string()),
                None => Ok("{}".to_string()),
            }
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：svrid 清洗后空/"0" 直接空对象；否则 find_one 按 C_SERVER_ID 候选
        //    列查找，命中/未命中语义与 §14 相同。
        // ② 关键程序解析：server id 是服务端消息号，跨分表全局唯一，是同步/撤回场景的
        //    稳定主键；字符串入参全程不做数值化，避免大数（>2^53）经 f64 精度丢失。
        // ③ 功能：§15 按服务端消息 id（svrid）取单条消息。
    })
}
