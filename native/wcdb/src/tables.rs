//! 表 / 快照 / 库列表 / 通用查询函数组（docs/protocol/04-tables-misc.md）。
//!
//! 覆盖：list_message_dbs、list_media_dbs、get_logs、exec_query、list_tables、
//! get_table_schema、message_table_columns、message_table_time_range、message_meta、
//! message_tables、message_table_stats、message_dates、media_schema_summary、
//! get_db_status、export/import_table_snapshot(_with_schema)。
//! 监控管道与 cloud_* 见 misc.rs。

use crate::ffi::*;
use crate::{take_logs_json, with_account};
use anyhow::{bail, Result};
use rusqlite::types::Value as SqlValue;
use serde_json::{json, Map, Value};
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use weflow_core::db::Account;
use weflow_core::schema::{detect_time_column, sanitize_ident, SessionTables};

use crate::query::{
    exec_query_json, path_to_json_string, query_rows_json, resolve_db_path,
};

// ── 会话消息表路由辅助 ──

struct TableRef {
    db_path: PathBuf,
    table: String,
    /// 共享表模式下的 (talker 列, sessionId) 过滤；专属分表为 None。
    filter: Option<(String, String)>,
}
// ① 实现方式：纯数据结构体，三个字段——db_path 定位库文件、table 定位表名、filter 承载
//    共享表的过滤条件；实例全部由 session_table_refs 按索引路由结果填充。
// ② 关键程序解析：filter 的 Some/None 对应 SessionTables 的 Shared/Located 两种拓扑——
//    None 表示专属分表（整表即该会话，无需过滤）；Some((列, 值)) 表示共享大表必须按
//    talker 列过滤，否则会把别人的消息读进来；下游 where_clause/detect_time_col 都
//    围绕 filter 是否为 None 分叉。
// ③ 功能：会话消息表的统一"定位描述符"，是本文件所有按会话查询的中间表示。

fn session_table_refs(account: &Account, session_id: &str) -> Result<Vec<TableRef>> {
    // 取消息索引（懒构建缓存），再让索引做"会话 → 表"的路由
    let index = account.msg_index()?;
    let located = index.session_tables(account, session_id)?;
    let mut out = Vec::new();
    match located {
        // 模式 A（Located）：会话独占若干分表，逐表登记、不加过滤条件
        SessionTables::Located(list) => {
            for (db_idx, table) in list {
                // db_idx 换算真实库路径；换算失败（索引快照与布局失配）跳过该表
                if let Some(db) = index.db_path(db_idx) {
                    out.push(TableRef { db_path: db.to_path_buf(), table, filter: None });
                }
            }
        }
        // 模式 B（Shared）：全部消息大表共享，需带 (talker 列, sessionId) 过滤
        SessionTables::Shared { tables, talker_column } => {
            for (db_idx, table) in tables {
                if let Some(db) = index.db_path(db_idx) {
                    out.push(TableRef {
                        db_path: db.to_path_buf(),
                        table,
                        // 过滤值就是 session_id 本身，列名取索引抽样得到的 talker_column
                        filter: Some((talker_column.clone(), session_id.to_string())),
                    });
                }
            }
        }
    }
    Ok(out)
    // ① 实现方式：msg_index 拿分表索引 → session_tables 路由出 SessionTables，按 Located/
    //    Shared 两种形态分别收集 TableRef；Shared 时把 talker 列与 session_id 打包进 filter。
    // ② 关键程序解析：index.db_path 是 get 式安全下标，None 只在索引与布局失配时出现，
    //    这里静默跳过而不是报错（少读一张表好过整体失败）；Located 可以是空 Vec（该会话
    //    本机没有消息表），此时返回空清单属合法语义，消费方自然得到 0 条结果。
    // ③ 功能：把一个 session_id 解析成"去哪些库、查哪些表、按什么条件过滤"的清单，
    //    是 message_tables/stats/dates 三个会话级接口的公共路由入口。
}

/// 组装 `WHERE "col" = ?` 片段（列名经白名单校验）。
fn where_clause(t: &TableRef) -> Result<(String, Option<String>)> {
    // 有 filter（共享表）→ 生成 WHERE 片段；无 filter（专属分表）→ 空串，SQL 原样拼
    match &t.filter {
        // 列名不能走 ? 绑定（SQLite 标识符不支持参数化），必须先过 sanitize_ident 白名单；
        // 值保持 ? 占位符，由调用方 query_row/query_map 时绑定
        Some((col, val)) => Ok((format!(" WHERE \"{}\" = ?", sanitize_ident(col)?), Some(val.clone()))),
        None => Ok((String::new(), None)),
    }
    // ① 实现方式：对 TableRef.filter 分叉——Some 时 format 出 ` WHERE "列" = ?` 并携带
    //    绑定值；None 返回空串与 None。
    // ② 关键程序解析：返回的 (where_sql, param) 是一对配套产物——SQL 文本里的占位符个数
    //    与参数必须一致，stats/dates 里按 param 是否为 Some 切换 query_row 的单参/无参
    //    形态；sanitize_ident 的 `?` 失败会整体报错，绝不拼出含非法标识符的半条 SQL。
    // ③ 功能：把共享表的会话过滤条件统一翻译成安全的 WHERE 片段与绑定参数。
}

fn detect_time_col(account: &Account, t: &TableRef) -> Option<String> {
    // 读列清单失败（库打不开等）不报错，静默降级为"没有时间列"
    let cols = account.columns(&t.db_path, &t.table).ok()?;
    detect_time_column(&cols)
    // ① 实现方式：account.columns（PRAGMA table_info 结果的带缓存封装）取列名，再交给
    //    weflow_core 的 detect_time_column 按候选序识别时间列。
    // ② 关键程序解析：columns 出错用 ok()? 转成 None 而非传播错误——stats/dates 对
    //    "读不到列"的表各有降级路径（只统计 count / 直接跳过），不应因单表失败中断整体。
    // ③ 功能：为会话表探测时间戳列名，供 MIN/MAX 统计与按日期聚合的 SQL 使用。
}

// ── 库列表 ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_list_message_dbs(handle: i64, out_json: *mut *mut c_void) -> i32 {
    with_account(handle, |account| {
        // 遍历布局里的消息分库清单，逐个转成 TS 友好的正斜杠路径字符串
        let paths: Vec<Value> = account
            .layout
            .message_dbs
            .iter()
            .map(|p| Value::String(path_to_json_string(p)))
            .collect();
        // 包成 JSON 数组写出；列表来自布局快照，本函数不读磁盘、恒成功
        emit_json(out_json, Ok(Value::Array(paths).to_string()), ERR_GENERIC)
        // ① 实现方式：with_account 拿账号上下文，遍历 layout.message_dbs，用
        //    path_to_json_string 统一路径口径后收集成 JSON 数组，经 emit_json 写出。
        // ② 关键程序解析：layout 是 open 时 discover_layout 的快照，微信之后新建的分库
        //    这里看不到；path_to_json_string 只把反斜杠统一成正斜杠（非 canonicalize），
        //    避免 JSON 双重转义并让 TS 侧 basename/存在性检查口径一致；Ok 包裹保证走
        //    emit_json 的成功分支，rc 恒为 OK。
        // ③ 功能：向 TS 报告当前账号全部消息分库的文件路径清单。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_list_media_dbs(handle: i64, out_json: *mut *mut c_void) -> i32 {
    with_account(handle, |account| {
        // 遍历布局里的媒体分库清单，逐个转成 TS 友好的正斜杠路径字符串
        let paths: Vec<Value> = account
            .layout
            .media_dbs
            .iter()
            .map(|p| Value::String(path_to_json_string(p)))
            .collect();
        // 包成 JSON 数组写出；列表来自布局快照，本函数不读磁盘、恒成功
        emit_json(out_json, Ok(Value::Array(paths).to_string()), ERR_GENERIC)
        // ① 实现方式：与 wcdb_list_message_dbs 同构，只是数据源换成 layout.media_dbs。
        // ② 关键程序解析：媒体分库数量取决于微信版本（旧版常为单个 media_0.db，新版
        //    按媒体类型拆分），TS 不应假设长度；路径口径与其它接口一致（正斜杠字符串），
        //    返回值同样来自布局快照、无磁盘 IO。
        // ③ 功能：向 TS 报告当前账号全部媒体分库的文件路径清单。
    })
}

// ── 日志（无 handle） ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_logs(out_json: *mut *mut c_void) -> i32 {
    guard(|| {
        // 把环形日志缓冲序列化成 JSON 数组写入出参（只读快照，不清空缓冲）
        write_out(out_json, &take_logs_json());
        OK
        // ① 实现方式：guard 包裹防 panic 越界；take_logs_json 锁全局日志缓冲做只读
        //    序列化，write_out 落到出参槽，固定返回 OK。
        // ② 关键程序解析：本函数无 handle，是不依赖账号的少数接口，guard 足够、无需
        //    with_account；take_logs_json 名字带 take 但实际不清空，缓冲按 LOG_CAPACITY
        //    环形淘汰旧条目；锁 poisoned 或序列化失败都兜底返回 "[]"，维持"rc==0 时
        //    outJson 必非空"的协议不变量。
        // ③ 功能：把 native 侧累积的诊断日志（时间戳+消息）以 JSON 数组交给 TS。
    })
}

// ── 通用 SQL 逃生门 ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_exec_query(
    handle: i64,
    kind: *const c_char,
    path: *const c_char,
    sql: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        // 三个 C 字符串入参统一借读成 &str（NULL/坏编码按空串处理）
        let kind = cstr(kind);
        let path = cstr(path);
        let sql = cstr(sql);
        // 解析库路径、执行 SQL、行集转 JSON 全部委托 query.rs 的 exec_query_json
        emit_json(out_json, exec_query_json(account, kind, path, sql), ERR_GENERIC)
        // ① 实现方式：with_account 拿账号后 cstr 转换三个入参，直接把参数透传给
        //    exec_query_json，结果经 emit_json 统一收敛（成功写出、失败记日志返码）。
        // ② 关键程序解析：这是协议 04 的"通用 SQL 逃生门"——kind+path 定位单个目标库，
        //    SQL 由 TS 原样传入，native 侧不做语句类型白名单（只读约束靠调用方自律）；
        //    本函数只做参数搬运，任何错误都在 emit_json 的 Err 分支记入环形日志并返回 -1。
        // ③ 功能：在指定业务库上执行任意 SQL 并返回行对象数组 JSON（调试/兜底入口）。
    })
}

// ── 表清单 / 建表 SQL ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_list_tables(
    handle: i64,
    kind: *const c_char,
    db_path: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let kind = cstr(kind);
        let db_path = cstr(db_path);
        // 核心逻辑收进闭包，错误不直接外抛，方便下方"空数组兜底"
        let result = (|| -> Result<String> {
            // kind + 显式路径解析目标库（路径为空时回退该 kind 的默认库）
            let db = resolve_db_path(account, kind, db_path)?;
            // 在该库连接上列出全部表名（sqlite_master，按 name 升序）
            let tables = account.with_conn(&db, |conn| weflow_core::db::list_tables(conn))?;
            // 表名收集成 JSON 字符串数组
            Ok(Value::Array(tables.into_iter().map(Value::String).collect()).to_string())
        })();
        // kind 不适用/库不存在时返回空数组也算成功（备份流程跳过空表库）
        match result {
            Ok(s) => emit_json(out_json, Ok(s), ERR_GENERIC),
            Err(_) => emit_json(out_json, Ok("[]".to_string()), ERR_GENERIC),
        }
        // ① 实现方式：闭包内 resolve_db_path 定位库 → with_conn 调 list_tables 拿表名 →
        //    序列化成 JSON 数组字符串；外层 match 把 Err 兜底成 "[]" 而非错误码。
        // ② 关键程序解析：兜底是刻意的——备份流程会遍历所有候选库，个别库缺席/打不开
        //    不应中断整体，返回 "[]" 让上层按"空表库"跳过即可；错误细节已随 resolve/
        //    open 链路日志留痕，这里只保留协议要求的成功形态。
        // ③ 功能：列出指定库的全部表名，供备份前的表枚举与诊断展示。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_table_schema(
    handle: i64,
    kind: *const c_char,
    db_path: *const c_char,
    table_name: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        // 入参统一借读成 &str；kind + db_path 二参定位目标库
        let kind = cstr(kind);
        let db_path = cstr(db_path);
        let table = cstr(table_name);
        // 双通道：rc 与 data.success 都判失败，失败也写 outJson
        let result = (|| -> Result<String> {
            let db = resolve_db_path(account, kind, db_path)?;
            // 在目标库连接里查 sqlite_master 取该表的原建表 SQL
            let schema = account.with_conn(&db, |conn| {
                // type='table' + name 精确匹配；表名走 ? 绑定，无拼接注入面
                let mut stmt = conn
                    .prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name=?")?;
                // 表不存在时 next() 得 None → flatten 成 None → 兜底空串（不算错误）
                let sql: Option<String> = stmt
                    .query_map([table], |r| r.get::<_, Option<String>>(0))?
                    .next()
                    .transpose()?
                    .flatten();
                Ok(sql.unwrap_or_default())
            })?;
            // 成功形态：{ success:true, schema:"CREATE TABLE ..." }（表不存在则 schema 为空串）
            Ok(json!({ "success": true, "schema": schema }).to_string())
        })();
        match result {
            Ok(s) => {
                write_out(out_json, &s);
                OK
            }
            Err(e) => {
                // 失败形态：{ success:false, error:... }，rc 仍为 OK（双通道协议的失败侧）
                write_out(out_json, &json!({ "success": false, "error": e.to_string() }).to_string());
                OK
            }
        }
        // ① 实现方式：闭包内 resolve_db_path + with_conn 查 sqlite_master 拿建表 SQL，
        //    组装 {success,schema} JSON；外层 match 把 Err 也写进出参（success:false），
        //    两条路径 rc 都是 OK，成败靠 data.success 字段区分。
        // ② 关键程序解析：query_map→next→transpose→flatten 的组合把"查询错误/无行/值为
        //    NULL"统一收敛成 Option<String>，再 unwrap_or_default 兜空串，所以"表不存在"
        //    表现为 schema="" 而不是失败；失败分支的 error 是 anyhow 错误链全文（无密钥），
        //    供 TS 直接展示；FTS 虚表的建表 SQL 也可由本接口取到。
        // ③ 功能：查询指定表的 CREATE TABLE 原文，供快照导入重建表结构与 UI 展示。
    })
}

// ── 消息表列 / 时间范围 / 元数据 ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_table_columns(
    handle: i64,
    db_path: *const c_char,
    table_name: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let db_path = cstr(db_path);
        let table = cstr(table_name);
        let result = (|| -> Result<String> {
            // 本接口不做 kind 路由，入参路径直接当作库文件
            let db = PathBuf::from(db_path);
            // 列内省走 account.columns（PRAGMA table_info 结果的带缓存封装）
            let cols = account.columns(&db, table)?;
            // 列名收集成 JSON 字符串数组（保留数据库原始大小写）
            Ok(Value::Array(cols.iter().map(|c| Value::String(c.clone())).collect()).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：db_path 经 PathBuf 直转后调 account.columns 读列名，收集为 JSON
        //    字符串数组，emit_json 统一收敛。
        // ② 关键程序解析：与 wcdb_list_tables 不同，这里没有 Err 兜底——库打不开/表名
        //    非法会以 rc=ERR_GENERIC 上抛；注意"表不存在"时 columns 返回空 Vec 且会被
        //    缓存，表现为成功返回 []，调用方需自行判断空数组语义（先 get_table_schema
        //    或 list_tables 确认表存在更稳妥）。
        // ③ 功能：查询指定消息表的全部列名，供 TS 做字段适配与展示。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_table_time_range(
    handle: i64,
    db_path: *const c_char,
    table_name: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        // 入参转 &str；db_path 直接当作库文件（无 kind 路由）
        let db_path = cstr(db_path);
        let table = cstr(table_name);
        let result = (|| -> Result<String> {
            let db = PathBuf::from(db_path);
            // 表名/列名都要过白名单后才能进 SQL 文本（标识符无法参数化）
            let safe_table = sanitize_ident(table)?;
            let cols = account.columns(&db, table)?;
            // 找不到可识别的时间列 → 返回全零占位（TS 按"无时间范围"处理）
            let Some(tc) = detect_time_column(&cols) else {
                return Ok(json!({ "column": "", "first_ts": 0, "last_ts": 0 }).to_string());
            };
            let safe_tc = sanitize_ident(&tc)?;
            // 一条聚合 SQL 同时取最早/最晚时间戳，避免扫两遍表
            let sql = format!("SELECT MIN(\"{safe_tc}\"), MAX(\"{safe_tc}\") FROM \"{safe_table}\"");
            // 空表时 MIN/MAX 返回 NULL → 用 Option<i64> 接
            let (first, last) = account.with_conn(&db, |conn| -> Result<(Option<i64>, Option<i64>)> {
                Ok(conn.query_row(&sql, [], |r| {
                    Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, Option<i64>>(1)?))
                })?)
            })?;
            // NULL 兜底为 0，与"无时间列"的占位形态对齐
            Ok(json!({
                "column": tc,
                "first_ts": first.unwrap_or(0),
                "last_ts": last.unwrap_or(0),
            })
            .to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：列内省 + detect_time_column 探测时间列，命中则拼 MIN/MAX 聚合 SQL
        //    做单行查询，结果组装 {column, first_ts, last_ts}；未命中直接给全零占位。
        // ② 关键程序解析：column 字段返回**原始列名**（供 TS 展示），SQL 里用的是白名单
        //    校验后的 safe_tc；MIN/MAX 在空表/全 NULL 列上得 NULL，unwrap_or(0) 统一成 0，
        //    与"column 为空串"形成两档"无范围"语义（有列但没数据 / 连列都没有）。
        // ③ 功能：给出消息表的时间跨度（最早/最晚时间戳），供时间轴与预览界面定位范围。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_meta(
    handle: i64,
    db_path: *const c_char,
    table_name: *const c_char,
    limit: i32,
    offset: i32,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        // 入参转 &str；本接口不做 kind 路由，db_path 直接当作库文件
        let db_path = cstr(db_path);
        let table = cstr(table_name);
        let result = (|| -> Result<String> {
            let db = PathBuf::from(db_path);
            // 表名过白名单；limit/offset 钳制为非负（负数会被 SQLite 拒绝或误解）
            let safe_table = sanitize_ident(table)?;
            let lim = limit.max(0);
            let off = offset.max(0);
            // SELECT * 分页取整行，行集经 query_rows_json 统一转行对象数组
            let sql = format!("SELECT * FROM \"{safe_table}\" LIMIT {lim} OFFSET {off}");
            let rows = account.with_conn(&db, |conn| query_rows_json(conn, &sql))?;
            Ok(Value::Array(rows).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：sanitize_ident 校验表名后拼带 LIMIT/OFFSET 的 SELECT *，在
        //    with_conn 里执行并经 query_rows_json 转成行对象数组 JSON。
        // ② 关键程序解析：limit/offset 是 TS 传入的整数，只能进 SQL 文本，钳制非负挡住
        //    协议外的意外取值；SELECT * 的输出列名即行对象 key；无任何 WHERE，属裸翻页
        //    的元数据探查接口，空结果集序列化为 "[]"。
        // ③ 功能：按页读取消息表的原始行（全部列），供 TS 做元数据预览与排障。
    })
}

// ── 会话消息表清单 / 统计 / 日期 ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_tables(
    handle: i64,
    session_id: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        let result = (|| -> Result<String> {
            // 会话路由出库/表清单（本接口只输出定位，不输出过滤条件）
            let refs = session_table_refs(account, sid)?;
            // 每个 TableRef 压缩成 {db_path, table_name} 对象
            let arr: Vec<Value> = refs
                .iter()
                .map(|t| {
                    json!({
                        "db_path": path_to_json_string(&t.db_path),
                        "table_name": t.table,
                    })
                })
                .collect();
            // 空清单（该会话本机无消息表）也照常输出 []
            Ok(Value::Array(arr).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：session_table_refs 路由后逐项 map 成 {db_path, table_name} JSON
        //    对象，包成数组写出。
        // ② 关键程序解析：路由失败（如"未发现 message_*.db 分库"）会整体报错而非空数组；
        //    空数组只出现在"路由成功但该会话无消息表"的合法场景；db_path 经
        //    path_to_json_string 统一正斜杠口径，与其它接口的路径输出一致。
        // ③ 功能：回答"这个会话的消息存在哪些库的哪些表里"，供诊断与备份定位。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_table_stats(
    handle: i64,
    session_id: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        let result = (|| -> Result<String> {
            // 路由出该会话的全部库/表，逐表做聚合统计
            let refs = session_table_refs(account, sid)?;
            let mut arr: Vec<Value> = Vec::with_capacity(refs.len());
            for t in &refs {
                // 共享表生成 `WHERE "talker列" = ?`，专属分表得到空 WHERE
                let (where_sql, param) = where_clause(t)?;
                // 表名过白名单后才允许进 SQL 文本
                let safe_table = sanitize_ident(&t.table)?;
                // 探测时间列；None 时退化为只统计行数
                let tc = detect_time_col(account, t);
                let (count, first, last) = account.with_conn(&t.db_path, |conn| -> Result<(i64, i64, i64)> {
                    if let Some(tc) = &tc {
                        // 有时间列：一条聚合 SQL 同时取 COUNT/MIN/MAX，避免扫三遍表
                        let safe_tc = sanitize_ident(tc)?;
                        let sql = format!(
                            "SELECT COUNT(*), MIN(\"{safe_tc}\"), MAX(\"{safe_tc}\") FROM \"{safe_table}\"{where_sql}"
                        );
                        let mut stmt = conn.prepare(&sql)?;
                        // 共享表带 talker 参数绑定；专属分表无参——与 (where_sql,param) 配套切换
                        let row = if let Some(p) = &param {
                            stmt.query_row([p.as_str()], |r| {
                                Ok((
                                    r.get::<_, i64>(0)?,
                                    // MIN/MAX 在空表上为 NULL，unwrap_or(0) 兜底
                                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                                ))
                            })?
                        } else {
                            // 专属分表：同一行映射逻辑，仅不带绑定参数
                            stmt.query_row([], |r| {
                                Ok((
                                    r.get::<_, i64>(0)?,
                                    r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                                    r.get::<_, Option<i64>>(2)?.unwrap_or(0),
                                ))
                            })?
                        };
                        Ok(row)
                    } else {
                        // 无时间列：只统计 COUNT(*)，时间跨度填 0
                        let sql = format!("SELECT COUNT(*) FROM \"{safe_table}\"{where_sql}");
                        let mut stmt = conn.prepare(&sql)?;
                        // 同样按 param 有无切换单参/无参形态
                        let count = if let Some(p) = &param {
                            stmt.query_row([p.as_str()], |r| r.get::<_, i64>(0))?
                        } else {
                            stmt.query_row([], |r| r.get::<_, i64>(0))?
                        };
                        Ok((count, 0, 0))
                    }
                })?;
                // 每表一行统计：路径/表名/行数/时间跨度（last_time 为旧消费方兼容字段）
                arr.push(json!({
                    "db_path": path_to_json_string(&t.db_path),
                    "table_name": t.table,
                    "count": count,
                    "first_timestamp": first,
                    "last_timestamp": last,
                    // messagePushService 只读 last_time（防撤回候选表过滤），必须同时输出
                    "last_time": last,
                }));
            }
            Ok(Value::Array(arr).to_string())
        })();
        // 失败码特意用 ERR_SCHEMA_MISMATCH：统计失败多因表结构与预期不符（列探测/白名单）
        emit_json(out_json, result, ERR_SCHEMA_MISMATCH)
        // ① 实现方式：session_table_refs 路由 → 逐表 where_clause + sanitize_ident +
        //    detect_time_col → with_conn 内按"有/无时间列"两条分支做聚合查询，结果按
        //    {db_path, table_name, count, first/last_timestamp, last_time} 逐项入数组。
        // ② 关键程序解析：MIN/MAX 的 NULL（空表）兜底成 0；query_row 的单参/无参形态必须
        //    与 where_clause 返回值配套，否则参数错位；last_time 与 last_timestamp 同值是
        //    协议兼容要求（旧消费方只认 last_time）；任一表失败都整体报错（rc=-7），
        //    不做部分成功，错误码选 ERR_SCHEMA_MISMATCH 是 TS 侧会引导用户做结构修复。
        // ③ 功能：输出会话各消息表的行数与时间跨度统计，供防撤回候选过滤与备份预览。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_message_dates(
    handle: i64,
    session_id: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let sid = cstr(session_id);
        // 特殊：任何失败都按"无消息"处理，返回 success:true + []（协议 04）
        let result = (|| -> Result<Vec<String>> {
            // 路由出该会话的库/表清单，逐表聚合出现过的日期
            let refs = session_table_refs(account, sid)?;
            let mut dates: Vec<String> = Vec::new();
            for t in &refs {
                // 无时间列的表没有日期可聚合，直接跳过（不报错）
                let Some(tc) = detect_time_col(account, t) else { continue };
                let safe_tc = sanitize_ident(&tc)?;
                let safe_table = sanitize_ident(&t.table)?;
                let (where_sql, param) = where_clause(t)?;
                // strftime 按本地时区把秒级时间戳截断到天，DISTINCT 去掉表内重复日期
                let sql = format!(
                    "SELECT DISTINCT strftime('%Y-%m-%d', \"{safe_tc}\", 'unixepoch', 'localtime') \
                     FROM \"{safe_table}\"{where_sql}"
                );
                let mut got = account.with_conn(&t.db_path, |conn| -> Result<Vec<String>> {
                    let mut stmt = conn.prepare(&sql)?;
                    // 共享表带 talker 参数 / 分表无参；NULL 日期行（坏时间戳）被过滤
                    let rows = if let Some(p) = &param {
                        stmt.query_map([p.as_str()], |r| r.get::<_, Option<String>>(0))?
                            .flatten()
                            .filter_map(|s| s)
                            .collect()
                    } else {
                        // 专属分表：同一映射逻辑，仅无绑定参数
                        stmt.query_map([], |r| r.get::<_, Option<String>>(0))?
                            .flatten()
                            .filter_map(|s| s)
                            .collect()
                    };
                    Ok(rows)
                })?;
                // 单表结果并入总清单；单表失败会整体 bail，被下方 unwrap_or_default 吃掉
                dates.append(&mut got);
            }
            // 跨表日期必有重复，排序（dedup 的前置条件）后去重得全局有序清单
            dates.sort();
            dates.dedup();
            Ok(dates)
        })();
        // Err 一律兜底为空数组：本接口的失败语义就是"没有消息日期"
        let dates = result.unwrap_or_default();
        write_out(out_json, &Value::Array(dates.into_iter().map(Value::String).collect()).to_string());
        OK
        // ① 实现方式：路由 → 逐表 strftime 取 DISTINCT 日期 → 汇总 sort+dedup → 固定
        //    写出 JSON 字符串数组，rc 恒为 OK。
        // ② 关键程序解析：'unixepoch' 把整型秒级时间戳转日期，'localtime' 对齐用户时区，
        //    与 TS 侧日历口径一致；unwrap_or_default 把路由/SQL/内省的全部失败压成 []——
        //    协议 04 规定本接口不报错（日历渲染用途，报错反而中断界面）；空值经
        //    flatten+filter_map 静默跳过；write_out 而非 emit_json 正是因为"永不失败"。
        // ③ 功能：输出会话消息覆盖的全部日期（YYYY-MM-DD 有序去重清单），供日历标点。
    })
}

// ── 媒体库结构摘要 / DB 状态 ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_media_schema_summary(
    handle: i64,
    db_path: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let db_path = cstr(db_path);
        let result = (|| -> Result<String> {
            // 固定按 media 类型解析（路径为空时回退第一个媒体分库）
            let db = resolve_db_path(account, "media", db_path)?;
            // 列出媒体库全部表名
            let tables = account.with_conn(&db, |conn| weflow_core::db::list_tables(conn))?;
            let mut obj = Map::new();
            for t in &tables {
                // 逐表读列名；个别表内省失败只跳过，不中断整体摘要
                if let Ok(cols) = account.columns(&db, t) {
                    obj.insert(
                        t.clone(),
                        Value::Array(cols.iter().map(|c| Value::String(c.clone())).collect()),
                    );
                }
            }
            Ok(Value::Object(obj).to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：resolve_db_path("media") 定位媒体库 → list_tables 枚举表 → 逐表
        //    account.columns 填充 {表名: [列名]} 的 JSON 对象。
        // ② 关键程序解析：表级失败用 if let Ok 静默跳过（摘要是概览用途，个别表读不到
        //    不值得整体失败），只有 resolve/list_tables 级别失败才报错；columns 带缓存，
        //    全库内省的成本只付一次。
        // ③ 功能：给出媒体库"表 → 列清单"的结构摘要，供 TS 校验媒体表结构与诊断。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_db_status(handle: i64, out_json: *mut *mut c_void) -> i32 {
    with_account(handle, |account| {
        // 内容自定（TS 只写进诊断日志），给出账号库可打开性摘要
        let status = json!({
            "session_db": path_to_json_string(&account.layout.session_db),
            "message_dbs": account.layout.message_dbs.len(),
            "media_dbs": account.layout.media_dbs.len(),
            "has_contact": account.layout.contact_db.is_some(),
            // 锁 poisoned / 尚未初始化都收敛成 null，不让诊断接口报错
            "my_account_id": account.my_account_id.lock().ok().and_then(|g| g.clone()),
        });
        emit_json(out_json, Ok(status.to_string()), ERR_GENERIC)
        // ① 实现方式：把布局快照的关键字段（会话库路径、消息/媒体分库数量、联系库有无、
        //    my_account_id）拼成一个 JSON 对象写出，全程无磁盘 IO。
        // ② 关键程序解析：json! 由 Ok 包裹——只要句柄有效本函数恒成功；my_account_id 走
        //    lock().ok().and_then 克隆 Option<String>，poisoned 与 None 都自然序列化成
        //    null，不会 panic 也不传播错误；本接口的契约是"永远能给 TS 一份诊断信息"。
        // ③ 功能：输出账号库布局状态摘要，供 TS 诊断面板记录与排障。
    })
}

// ── 表快照 导出 / 导入 ──

const SNAP_FORMAT: &str = "wfsnap/1";
// 快照格式魔数：导出时写入 format 字段、导入时强校验，不一致即拒绝（防误灌任意 JSON）。

#[no_mangle]
pub unsafe extern "C" fn wcdb_export_table_snapshot(
    handle: i64,
    kind: *const c_char,
    db_path: *const c_char,
    table_name: *const c_char,
    output_path: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        // 四个 C 字符串入参统一借读成 &str
        let kind = cstr(kind);
        let db_path = cstr(db_path);
        let table = cstr(table_name);
        let output = cstr(output_path);
        // 核心导出逻辑在 export_snapshot，这里只负责把结果落到出参
        let result = export_snapshot(account, kind, db_path, table, output);
        match result {
            Ok(v) => {
                write_out(out_json, &v.to_string());
                OK
            }
            Err(e) => {
                // 失败也写 {success:false, error:...}，rc 恒为 OK（双通道协议）
                write_out(out_json, &json!({ "success": false, "error": e.to_string() }).to_string());
                OK
            }
        }
        // ① 实现方式：with_account + cstr 转参后委托 export_snapshot，按 Ok/Err 分别写
        //    成功摘要 JSON 或 {success:false,error}，两条路径 rc 都是 OK。
        // ② 关键程序解析：快照组不用 emit_json，因为协议要求失败时 TS 能拿到具体错误
        //    文本向用户展示（rc!=0 只给状态码不够）；错误串是 anyhow 错误链全文（无密钥）；
        //    本函数不含业务逻辑，行为完全由 export_snapshot 决定。
        // ③ 功能：把指定表整表导出为 wfsnap/1 快照文件，返回行数/列数摘要。
    })
}

fn export_snapshot(
    account: &Account,
    kind: &str,
    db_path: &str,
    table: &str,
    output: &str,
) -> Result<Value> {
    // 定位目标库；表名过白名单后才能进 SELECT 文本
    let db = resolve_db_path(account, kind, db_path)?;
    let safe_table = sanitize_ident(table)?;
    // 先取一份列清单：既决定每行切多少格，也进结果摘要
    let columns = account.columns(&db, table)?;
    let ncols = columns.len();
    let (schema, rows) = account.with_conn(&db, |conn| -> Result<(String, Vec<Vec<Value>>)> {
        // 从 sqlite_master 取原建表 SQL，导入端可据此重建表结构
        let schema = {
            let mut stmt = conn.prepare("SELECT sql FROM sqlite_master WHERE type='table' AND name=?")?;
            let sql: Option<String> = stmt
                .query_map([table], |r| r.get::<_, Option<String>>(0))?
                .next()
                .transpose()?
                .flatten();
            sql.unwrap_or_default()
        };
        // 全表扫描导出（无 WHERE/LIMIT，快照必须完整）
        let mut stmt = conn.prepare(&format!("SELECT * FROM \"{safe_table}\""))?;
        let mut rows = Vec::new();
        let mut raw = stmt.query([])?;
        // 逐行逐列转储：每行预分配 ncols 长度的 Vec，单元格经 sql_value_to_snap 归一
        while let Some(row) = raw.next()? {
            let mut vals = Vec::with_capacity(ncols);
            for i in 0..ncols {
                vals.push(sql_value_to_snap(row.get_ref(i)?));
            }
            rows.push(vals);
        }
        Ok((schema, rows))
    })?;
    let row_count = rows.len();
    let col_count = columns.len();
    // 行二维数组包成 JSON 数组、列名转字符串数组，组装 wfsnap/1 快照文档
    let rows_value = Value::Array(rows.into_iter().map(Value::Array).collect());
    let cols_value = Value::Array(columns.iter().map(|c| Value::String(c.clone())).collect());
    let snap = json!({
        "format": SNAP_FORMAT,
        "table": table,
        "schema": schema,
        "columns": cols_value,
        "rows": rows_value,
    });
    // 输出目录不存在时静默创建；创建失败留给下面的 write 报错
    if let Some(parent) = Path::new(output).parent() {
        std::fs::create_dir_all(parent).ok();
    }
    // 快照全文一次性写盘（整文档在内存里，超大表会占内存，协议已知取舍）
    std::fs::write(output, snap.to_string())?;
    Ok(json!({ "success": true, "rows": row_count, "columns": col_count }))
    // ① 实现方式：定位库 → 列内省 → 单连接内完成"取建表 SQL + 全表转储"，行数据按
    //    [行][列] 的 JSON 数组物化，最后组装快照文档写盘并返回计数摘要。
    // ② 关键程序解析：schema 查询与行扫描放在同一个 with_conn 回调里，两个视图取自
    //    同一时刻的连接（连接锁全程持有）；单元格转换走 sql_value_to_snap（BLOB→$blob
    //    hex），保证 JSON 可序列化且导入端可逆；输出路径由 TS 传入，create_dir_all 只
    //    兜目录缺失，写文件失败 `?` 上抛，由调用方写 success:false。
    // ③ 功能：把一张表完整导出为带格式魔数/建表 SQL/列名/行数据的快照 JSON 文件。
}

fn sql_value_to_snap(v: rusqlite::types::ValueRef) -> Value {
    match v {
        // NULL 原样进 JSON
        rusqlite::types::ValueRef::Null => Value::Null,
        rusqlite::types::ValueRef::Integer(i) => Value::Number(i.into()),
        // JSON 不支持 NaN/Inf，from_f64 失败兜底 null
        rusqlite::types::ValueRef::Real(f) => {
            serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
        }
        // 文本按 lossy 转（坏字节替换 U+FFFD，不因编码丢行）
        rusqlite::types::ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        // BLOB 不能直接进 JSON：包成 {"$blob": hex} 哨兵对象，导入端按此还原
        rusqlite::types::ValueRef::Blob(b) => json!({ "$blob": hex::encode(b) }),
    }
    // ① 实现方式：按 SQLite 五种动态类型分发到 JSON——Null/Integer 直转、Real 经
    //    from_f64（NaN/Inf 兜 null）、Text 走 from_utf8_lossy、BLOB 编成 "$blob" hex 哨兵。
    // ② 关键程序解析："$blob" 哨兵是快照往返无损的关键——JSON 没有二进制类型，导入端
    //    snap_to_sql_value 检测到该对象就 hex 解码还原 Blob；ValueRef 是零拷贝借用，
    //    本函数内一次性完成拷贝转 owned，不长期持有连接内存。
    // ③ 功能：把单元格值转成可序列化且可逆的快照 JSON 表示（导出侧的原子转换）。
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_import_table_snapshot(
    handle: i64,
    kind: *const c_char,
    db_path: *const c_char,
    table_name: *const c_char,
    input_path: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        // 无 schema 版：create_table_sql 固定传空串，导入时回退快照内嵌的建表 SQL
        let result = import_snapshot(
            account,
            cstr(kind),
            cstr(db_path),
            cstr(table_name),
            cstr(input_path),
            "",
        );
        write_import_result(out_json, result)
        // ① 实现方式：cstr 转换四个入参后直接委托 import_snapshot，结果统一经
        //    write_import_result 写出。
        // ② 关键程序解析：本入口给"目标表已存在/由 TS 自行建表"的场景用——不提供外部
        //    建表 SQL 时，导入器只在表缺失且快照内嵌 schema 非空时才建表（见
        //    import_snapshot）；cstr 借用仅限本次调用，不缓存。
        // ③ 功能：从 wfsnap/1 快照文件导入一张表（无外部建表 SQL 版本）。
    })
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_import_table_snapshot_with_schema(
    handle: i64,
    kind: *const c_char,
    db_path: *const c_char,
    table_name: *const c_char,
    input_path: *const c_char,
    create_table_sql: *const c_char,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        // 带 schema 版：外部建表 SQL 优先于快照内嵌值（用于按新结构重建旧表）
        let result = import_snapshot(
            account,
            cstr(kind),
            cstr(db_path),
            cstr(table_name),
            cstr(input_path),
            cstr(create_table_sql),
        );
        write_import_result(out_json, result)
        // ① 实现方式：与 wcdb_import_table_snapshot 相同，仅多转一个 create_table_sql
        //    入参并透传给 import_snapshot。
        // ② 关键程序解析：协议允许 TS 传空串——import_snapshot 会 trim 后判空，空串等价
        //    于走快照内嵌 schema，所以两个入口在本层没有行为差异，差异全部收敛在
        //    import_snapshot 的 schema_sql 选择逻辑里。
        // ③ 功能：从快照导入一张表，并允许 TS 指定建表 SQL（带 schema 版本）。
    })
}

fn write_import_result(out_json: *mut *mut c_void, result: Result<Value>) -> i32 {
    match result {
        Ok(v) => {
            // 成功：写出 {success:true, rows, inserted, ignored, malformed, columns} 摘要
            write_out(out_json, &v.to_string());
            OK
        }
        Err(e) => {
            // 失败：写出 {success:false, error}，rc 仍为 OK（双通道协议，error 供 UI 展示）
            write_out(out_json, &json!({ "success": false, "error": e.to_string() }).to_string());
            OK
        }
    }
    // ① 实现方式：对 import_snapshot 的 Result 分叉，两条路径都 write_out 并返回 OK，
    //    成败由 JSON 里的 success 字段区分。
    // ② 关键程序解析：不能复用 emit_json——emit_json 失败时不写出参，而快照导入协议
    //    要求 TS 能拿到具体错误文本直接展示；两个导入入口共用本收敛点，保证出参形态
    //    一致（与导出侧 wcdb_export_table_snapshot 的手写 match 同一套约定）。
    // ③ 功能：快照导入结果的统一出参收敛（成功摘要 / 失败原因）。
}

fn import_snapshot(
    account: &Account,
    kind: &str,
    db_path: &str,
    table: &str,
    input: &str,
    create_table_sql: &str,
) -> Result<Value> {
    // 读入快照全文并解析 JSON；读文件/解析失败直接上抛
    let raw = std::fs::read_to_string(input)?;
    let snap: Value = serde_json::from_str(&raw)?;
    // 格式魔数校验：不是 wfsnap/1 一律拒绝，防止把任意 JSON 当快照灌进库
    if snap.get("format").and_then(Value::as_str) != Some(SNAP_FORMAT) {
        bail!("快照格式不受支持");
    }
    // 提取列名清单（非字符串项被 filter_map 丢弃）；字段缺失按空清单处理
    let columns: Vec<String> = snap
        .get("columns")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    // 行数据克隆成 owned Vec（脱离快照借用便于逐行处理）；字段缺失按空行集处理
    let rows = snap.get("rows").and_then(Value::as_array).cloned().unwrap_or_default();
    let db = resolve_db_path(account, kind, db_path)?;
    // 表名过白名单，才允许出现在 INSERT 文本里
    let safe_table = sanitize_ident(table)?;

    // 建表 SQL 来源优先级：TS 显式传入 > 快照内嵌 schema > 空（不建表）
    let schema_sql = if !create_table_sql.trim().is_empty() {
        create_table_sql.to_string()
    } else {
        snap.get("schema").and_then(Value::as_str).unwrap_or_default().to_string()
    };

    // 列名逐个过白名单后加双引号（个别非标识符列名降级原样保留）；占位符按列数生成
    let col_sql: Vec<String> = columns.iter().map(|c| format!("\"{}\"", sanitize_ident(c).unwrap_or_else(|_| c.clone()))).collect();
    let placeholders = vec!["?"; columns.len()].join(",");
    // INSERT OR IGNORE：主键/唯一约束冲突时静默跳过（重复导入幂等、不中断）
    let insert_sql = format!(
        "INSERT OR IGNORE INTO \"{safe_table}\" ({}) VALUES ({placeholders})",
        col_sql.join(",")
    );

    account.with_conn(&db, |conn| -> Result<Value> {
        // 查 sqlite_master 判断表是否已存在（表名走 ? 绑定，无拼接）
        let exists: bool = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?",
            [table],
            |r| Ok(r.get::<_, i64>(0)? > 0),
        )?;
        // 表缺失且手上有建表 SQL 才建表；表已存在时绝不执行（防覆盖既有结构）
        if !exists && !schema_sql.trim().is_empty() {
            conn.execute_batch(&schema_sql)?;
        }
        // 三个计数：新插入 / 因约束冲突被 OR IGNORE 拦下 / 行形态不合法
        let mut inserted = 0i64;
        let mut ignored = 0i64;
        let mut malformed = 0i64;
        // 单事务包裹全部插入：函数中途因错误返回时，未提交事务随 tx Drop 自动回滚
        let tx = conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(&insert_sql)?;
            for row in &rows {
                // 行不是 JSON 数组 → 计 malformed 跳过
                let Some(arr) = row.as_array() else {
                    malformed += 1;
                    continue;
                };
                // 列数与表头不齐 → 计 malformed 跳过（错位插入比丢弃更危险）
                if arr.len() != columns.len() {
                    malformed += 1;
                    continue;
                }
                // JSON 值先转 SQLite 值，再统一取 ToSql 引用切片绑定
                let params: Vec<SqlValue> = arr.iter().map(snap_to_sql_value).collect();
                let refs: Vec<&dyn rusqlite::ToSql> =
                    params.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
                // Ok(1)=插入成功；Ok(0)=OR IGNORE 拦下；Err=坏值（只计 malformed 不中断整批）
                match stmt.execute(refs.as_slice()) {
                    Ok(1) => inserted += 1,
                    Ok(_) => ignored += 1,
                    Err(_) => malformed += 1,
                }
            }
        }
        // 全部行处理完才提交；提交失败整体报错（事务已回滚）
        tx.commit()?;
        Ok(json!({
            "success": true,
            "rows": rows.len(),
            "inserted": inserted,
            "ignored": ignored,
            "malformed": malformed,
            "columns": columns.len(),
        }))
    })
    // ① 实现方式：读文件 → 魔数校验 → 提取 columns/rows/schema → 拼参数化 INSERT →
    //    单事务内逐行执行并按结果分类计数，返回四项计数的 JSON 摘要。
    // ② 关键程序解析：execute 的 Err 被 match 吃掉而非 `?`——单行坏值只计 malformed，
    //    不回滚整批（"尽力导入"协议设计）；用 unchecked_transaction 是因为手上只有
    //    &Connection（非可变借用，事务不涉及嵌套语义）；建表前先判表存在，保证重复
    //    导入不会重建/清空既有表；execute_batch 支持多语句建表脚本；列名的
    //    unwrap_or_else 降级保留是快照自产自销场景下的宽限（导出端列名即库内真名）。
    // ③ 功能：把 wfsnap/1 快照安全灌回目标表，输出插入/跳过/坏行分类统计。
}

fn snap_to_sql_value(v: &Value) -> SqlValue {
    match v {
        // JSON null → SQL NULL
        Value::Null => SqlValue::Null,
        // 布尔以 0/1 整数入库（SQLite 无独立布尔类型）
        Value::Bool(b) => SqlValue::Integer(*b as i64),
        // 整数优先 i64；其次浮点；两者皆失（超出范围的大数）兜 NULL
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                SqlValue::Integer(i)
            } else if let Some(f) = n.as_f64() {
                SqlValue::Real(f)
            } else {
                SqlValue::Null
            }
        }
        // 字符串直转 TEXT
        Value::String(s) => SqlValue::Text(s.clone()),
        // "$blob" 哨兵对象 → hex 解码还原 BLOB；hex 非法时兜空 BLOB（不报错）
        Value::Object(o) => {
            if let Some(hexstr) = o.get("$blob").and_then(Value::as_str) {
                SqlValue::Blob(hex::decode(hexstr).unwrap_or_default())
            } else {
                // 其它对象序列化成 TEXT 存储（内容不丢，只是类型降级）
                SqlValue::Text(v.to_string())
            }
        }
        // 数组同样序列化成 TEXT（快照端不会产出，防御性分支）
        Value::Array(_) => SqlValue::Text(v.to_string()),
    }
    // ① 实现方式：JSON 五类值反向映射到 rusqlite 的 SqlValue——与 sql_value_to_snap
    //    一一对应，导出侧的 "$blob" hex 哨兵在此解码还原二进制。
    // ② 关键程序解析：Number 先试 as_i64 再试 as_f64，与导出侧 Integer/Real 的分型对称；
    //    hex::decode 失败给空 Vec 而非报错（导出端生成的 hex 恒合法，这里防手改文件）；
    //    未知 Object/Array 降级为 TEXT JSON 串，保证任何行都不会因类型无解而中断，
    //    配合外层事务按行计数形成完整的降级链路。
    // ③ 功能：把快照 JSON 值转回可绑定的 SQLite 值（导入侧的原子转换）。
}
