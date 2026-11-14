//! 会话函数组（docs/protocol/01-core-sessions-messages.md §8-§10）。
//!
//! get_sessions 读 session.db 的会话表；schema 未定档，用候选列名归一到协议 snake_case。

use crate::ffi::*;
use crate::query::query_rows_json;
use crate::{push_log, with_account};
use anyhow::{bail, Result};
use serde_json::{Map, Value};
use std::ffi::c_void;
use weflow_core::db::{list_tables, table_columns, Account};
use weflow_core::schema::find_candidate;

const C_USERNAME: &[&str] = &["username", "user_name", "userName", "usrName", "UsrName", "name", "str_name", "talker", "str_username"];
const C_TYPE: &[&str] = &["type", "session_type", "local_type", "ntype"];
const C_LOCAL_TYPE: &[&str] = &["local_type", "localType", "contact_type"];
const C_UNREAD: &[&str] = &["unread_count", "unreadCount", "unreadcount", "unread", "n_unread", "un_read_count"];
const C_SORT_TS: &[&str] = &["sort_timestamp", "sortTimestamp", "sort_time", "sortTime", "ntimestamp", "update_time", "order_time"];
const C_LAST_TS: &[&str] = &["last_timestamp", "lastTimestamp", "last_msg_time", "lastMsgTime", "create_time", "createTime"];
const C_SUMMARY: &[&str] = &["digest", "summary", "last_msg", "lastMsg", "str_digest", "content", "last_message"];
const C_LAST_TYPE: &[&str] = &["last_msg_type", "lastMsgType", "digest_type", "last_type"];
const C_MSG_COUNT: &[&str] = &["message_count", "messageCount", "msg_count", "msgCount", "total_count", "totalCount"];
const C_DISPLAY: &[&str] = &["display_name", "displayName", "nick_name", "nickname", "nickName", "remark", "str_nick_name", "str_remark"];
// ① 实现方式：每个协议字段配一组候选列名常量（下划线 / 驼峰 / 微信内部缩写变体并存），
//    供 find_candidate 在真实表的列名清单里逐个试探命中。
// ② 关键程序解析：session.db 的 schema 未定档，不同版本微信的列名写法不一（如
//    "unread_count"/"unreadCount"/"n_unread"），把差异集中收进常量数组而不是散落在
//    查询代码里，新增版本只需补数组元素；数组顺序即优先级，靠前者先命中。
// ③ 功能：会话表列名归一的"字典"——协议 snake_case 字段 → 各版本真实列名候选。

const FIELD_MAP: &[(&str, &[&str])] = &[
    ("username", C_USERNAME),
    ("type", C_TYPE),
    ("local_type", C_LOCAL_TYPE),
    ("unread_count", C_UNREAD),
    ("sort_timestamp", C_SORT_TS),
    ("last_timestamp", C_LAST_TS),
    ("summary", C_SUMMARY),
    ("last_msg_type", C_LAST_TYPE),
    ("message_count", C_MSG_COUNT),
    ("display_name", C_DISPLAY),
];
// ① 实现方式：把上面 10 组候选表打包成 (协议字段名, 候选列名组) 的静态映射表，
//    normalize_session 据此对每一行原始数据逐字段改名。
// ② 关键程序解析：协议输出只认这 10 个 snake_case 键；改名是"复制值到新键"，原始列
//    原样保留，便于排查时对照。新增协议字段要同步扩候选常量与这张表两处。
// ③ 功能：会话行"列名归一总表"——raw 行 → 协议会话对象的唯一映射依据。

/// 在 session.db 中定位主会话表。
///
/// 不能只按表名匹配：WeChat 4.x 的真身叫 `SessionTable`，而
/// `SessionDeleteTable` / `SessionDraft` / `SessionUnread*` 等辅助表同样含 "session"，
/// 且字典序排在 `SessionTable` 之前——按名字选会挑到 1 行的删除表，导致会话列表几乎全丢。
/// 因此按"形状"评分：须有 username 列，有排序/时间戳列加分，行数多者优先。
fn locate_session_table(account: &Account) -> Result<String> {
    let db = account.layout.session_db.clone();
    let tables = account.with_conn(&db, |c| list_tables(c))?;
    let mut best: Option<(i64, String)> = None; // (score, table)
    for t in &tables {
        if !t.to_ascii_lowercase().contains("session") {
            continue;
        }
        let scored = account.with_conn(&db, |c| -> Result<Option<i64>> {
            let cols = table_columns(c, t)?;
            // 必须有 username 列（主会话表才有；否则可能是辅助表）
            if find_candidate(&cols, C_USERNAME).is_none() {
                return Ok(None);
            }
            let mut score = 1i64;
            // 精确/近精确名字强加分
            let lower = t.to_ascii_lowercase();
            if lower == "session" {
                score += 1_000_000;
            } else if lower == "sessiontable" || lower == "session_table" {
                score += 100_000;
            }
            // 辅助表降权（删除/草稿/未读/无联系人信息/统计等）
            for bad in ["delete", "draft", "unread", "nocontact", "noinfo", "stat", "hidden", "oplog", "del"] {
                if lower.contains(bad) {
                    score -= 50_000;
                }
            }
            // 有排序时间戳列加分（主会话表的标志）
            if find_candidate(&cols, C_SORT_TS).is_some() || find_candidate(&cols, C_LAST_TS).is_some() {
                score += 10_000;
            }
            // 行数兜底：正常主会话表远多于辅助表
            let cnt: i64 = c
                .query_row(&format!("SELECT COUNT(*) FROM \"{}\"", t.replace('"', "\"\"")), [], |r| r.get(0))
                .unwrap_or(0);
            score += cnt.min(100_000);
            Ok(Some(score))
        })?;
        if let Some(score) = scored {
            if best.as_ref().map_or(true, |(bs, _)| score > *bs) {
                best = Some((score, t.clone()));
            }
        }
    }
    match best {
        Some((_, t)) => Ok(t),
        None => bail!("session.db 中未找到主会话表"),
    }
    // ① 实现方式：遍历 session.db 全部表名，先按名字含 "session" 粗筛，再对每张候选表
    //    开连接读列名与行数打分，全程只读，取最高分者作为主会话表返回。
    // ② 关键程序解析：评分闭包里 username 列是硬门槛（缺失直接 Ok(None) 出局，拦住
    //    辅助表），其余为加权项——精确表名 +100 万 / +10 万、辅助表关键词逐个 -5 万
    //    （可叠加，如 "delete" 同时命中 "del"）、有排序/时间戳列 +1 万、行数封顶 +10 万；
    //    map_or(true, ...) 让首张入选表无条件占据 best，后续按分数择优替换。COUNT(*)
    //    失败按 0 行兜底（unwrap_or(0)），避免个别表统计失败拖垮整体；COUNT 的表名经
    //    replace 转义双引号防注入，返回的表名在调用方仍会再过 sanitize_ident。
    // ③ 功能：为 get_sessions / mark_all_sessions_read 定位 session.db 的主会话表。
}

fn ci_get<'a>(map: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    map.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)
    // ① 实现方式：线性遍历 JSON 对象的键值对，用 eq_ignore_ascii_case 做 ASCII 大小写
    //    不敏感的键比较，命中后 map 取出值的借用并以 Option 返回。
    // ② 关键程序解析：不走 serde_json 的索引取值（它区分大小写），因为微信库的列名有
    //    "username"/"userName"/"UsrName" 等多种写法；返回借用而非克隆，normalize_session
    //    里的 is_some 判断与取值都不产生数据拷贝。
    // ③ 功能：大小写不敏感的 JSON 字段查找，是列名归一的底层比较原语。
}

fn normalize_session(raw: Value) -> Value {
    let mut map = match raw {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("value".to_string(), other);
            m
        }
    };
    let keys: Vec<String> = map.keys().cloned().collect();
    for (proto, cands) in FIELD_MAP {
        // 协议字段已存在（原始行自带或前一轮写入）则跳过，避免覆盖真值。
        if ci_get(&map, proto).is_some() {
            continue;
        }
        // 按候选顺序在原始键里找 ASCII 大小写不敏感的匹配，命中即把值复制到协议键名并停止。
        for cand in *cands {
            if let Some(k) = keys.iter().find(|k| k.eq_ignore_ascii_case(cand)) {
                if let Some(v) = map.get(k) {
                    map.insert(proto.to_string(), v.clone());
                    break;
                }
            }
        }
    }
    // last_timestamp 缺失时回退 sort_timestamp（chatService 约定）
    let has_last = ci_get(&map, "last_timestamp").map(|v| !v.is_null()).unwrap_or(false);
    if !has_last {
        if let Some(st) = ci_get(&map, "sort_timestamp").cloned() {
            map.insert("last_timestamp".to_string(), st);
        }
    }
    Value::Object(map)
    // ① 实现方式：先把非对象的原始行兜底成 {"value": ...} 对象，再按 FIELD_MAP 逐协议
    //    字段做大小写不敏感改名，最后补 last_timestamp 回退，整体包回 Value::Object 返回。
    // ② 关键程序解析：改名是"复制值到新键"，原始列保留——TS 读协议键，原始键留作排查；
    //    has_last 用 !v.is_null() 判定，SQL NULL 不算"已有"，缺失时才用 sort_timestamp
    //    回退（chatService 约定，反向不补）；keys 先收集成 Vec 是为了在遍历候选时不再
    //    借用 map，避免与 map.insert 的可变借用冲突。
    // ③ 功能：把 session.db 的一行原始数据归一成协议 snake_case 会话对象。
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_sessions(handle: i64, out_json: *mut *mut c_void) -> i32 {
    with_account(handle, |account| {
        let result = (|| -> Result<String> {
            // 立即执行闭包（IIFE）：把带 ? 的各步错误统一收敛进 Result<String>，交给 emit_json 分发出参与状态码。
            let table = locate_session_table(account)?;
            let db = account.layout.session_db.clone();
            let cols = account.with_conn(&db, |c| table_columns(c, &table))?;
            // 排序列优先取 sort_timestamp，缺失回退 last_timestamp；两者皆无则不加 ORDER BY（行序交由库决定）。
            let sort_col = find_candidate(&cols, C_SORT_TS).or_else(|| find_candidate(&cols, C_LAST_TS));
            let order = match &sort_col {
                Some(sc) => format!(
                    " ORDER BY \"{}\" DESC",
                    weflow_core::schema::sanitize_ident(sc)?
                ),
                None => String::new(),
            };
            // 表名与排序列都过 sanitize_ident（白名单校验 + 引号转义）再拼进 SQL，防标识符注入。
            let sql = format!("SELECT * FROM \"{}\"{order}", weflow_core::schema::sanitize_ident(&table)?);
            let rows = account.with_conn(&db, |c| query_rows_json(c, &sql))?;
            let out: Vec<Value> = rows
                .into_iter()
                .map(normalize_session)
                .filter(|v| {
                    // username 为空的行丢弃（chatService 约定）
                    ci_get(v.as_object().unwrap_or(&Map::new()), "username")
                        .and_then(Value::as_str)
                        .map(|s| !s.is_empty())
                        .unwrap_or(false)
                })
                .collect();
            Ok(Value::Array(out).to_string())
        })();
        emit_json(out_json, result, ERR_NO_SESSION_TABLE)
    })
    // ① 实现方式：with_account 路由句柄后，定位主会话表 → SELECT * 全量拉取 → 逐行
    //    normalize_session 归一 → 过滤空 username → 序列化成 JSON 数组交给 emit_json。
    // ② 关键程序解析：IIFE 把所有 ? 收敛成 Result<String>，Ok 走 write_out 写出参返回
    //    OK，Err（定位失败/SQL 失败）统一上抛 ERR_NO_SESSION_TABLE(-6) 且不写出参；
    //    会话顺序已由 SQL 的 ORDER BY DESC 决定，Rust 侧不再重排。filter 里对非对象行
    //    用 unwrap_or(&Map::new()) 兜底成空表，username 缺失按"空"丢弃而不是 panic，
    //    满足 chatService"空 username 行不入列表"的约定。
    // ③ 功能：FFI 导出 wcdb_get_sessions——返回当前账号的完整会话列表（协议 01 §8）。
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_mark_all_sessions_read(handle: i64, out_error: *mut *mut c_void) -> i32 {
    with_account(handle, |account| {
        let result = (|| -> Result<()> {
            // 定位主会话表（评分逻辑同 get_sessions），找不到即报错，不做半截更新。
            let table = locate_session_table(account)?;
            let db = account.layout.session_db.clone();
            let cols = account.with_conn(&db, |c| table_columns(c, &table))?;
            // let-else：未读数列缺失属 schema 不符，直接报错退出，避免凭空拼一条无效 UPDATE。
            let Some(unread) = find_candidate(&cols, C_UNREAD) else {
                bail!("会话表无未读数列");
            };
            let sql = format!(
                "UPDATE \"{}\" SET \"{}\" = 0",
                weflow_core::schema::sanitize_ident(&table)?,
                weflow_core::schema::sanitize_ident(&unread)?
            );
            // 执行全表 UPDATE（无 WHERE 是刻意的——"全部已读"语义），不关心受影响行数。
            account.with_conn(&db, |c| {
                c.execute(&sql, [])?;
                Ok(())
            })?;
            Ok(())
        })();
        match result {
            Ok(()) => OK,
            Err(e) => {
                let msg = e.to_string();
                push_log(&format!("mark_all_sessions_read: {msg}"));
                write_out(out_error, &msg);
                ERR_GENERIC
            }
        }
    })
    // ① 实现方式：定位主会话表 → 找未读数列 → 拼一条 UPDATE ... SET 未读列 = 0 全表执行；
    //    成功返回 OK，失败则 push_log 留痕、write_out 写错误文本并返回 ERR_GENERIC。
    // ② 关键程序解析：与 get_sessions 的 emit_json 收敛不同，这里手写 match——写操作没有
    //    JSON 出参，失败要把错误串直接写进 out_error 供 TS 展示；表名/列名都先过
    //    sanitize_ident 再拼接，杜绝标识符注入；push_log 的错误详情只进进程内环形日志，
    //    不落盘。
    // ③ 功能：FFI 导出 wcdb_mark_all_sessions_read——把所有会话的未读数一次性清零（协议 01 §10）。
}
