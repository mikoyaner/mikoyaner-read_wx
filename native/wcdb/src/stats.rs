//! 统计与游标函数组（docs/protocol/02-stats-cursors-reports.md）。
//!
//! 覆盖必需符号：wcdb_get_aggregate_stats、wcdb_open_message_cursor、
//! wcdb_fetch_message_batch、wcdb_close_message_cursor。
//!
//! 游标策略：open 时把会话在 [begin,end] 内的全部消息物化为已排序的 Vec<Value>，
//! 存入全局游标表；fetch 顺序切片返回。这与 wcdb_get_messages 的合并-排序模型一致，
//! 且天然满足「跨批次单调推进」「batch_size=1」「空数组+has_more」等协议约束。

use crate::ffi::*;
use crate::messages::{
    as_i64, ci_get, fetch_table, normalize_row, session_table_refs, sort_key, TableRef,
};
use crate::query::path_to_json_string;
use crate::{push_log, with_account};
// 外部 crate：anyhow 错误链 / chrono 时区处理 / serde_json 序列化
use anyhow::Result;
use chrono::{DateTime, Local, TimeZone, Utc};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::ffi::{c_char, c_void};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use weflow_core::db::Account;

// ── 全局游标表 ──

struct Cursor {
    rows: Vec<Value>,
    pos: usize,
    batch: usize,
}
// ① 实现方式：物化快照三件套——rows 是 open 时排序好的全量行、pos 是下一批起始下标、
//    batch 是每批条数；fetch 只做切片推进，不再碰 SQL。
// ② 关键程序解析：快照语义 = open 到 close 之间数据恒定，不随后续写入变化（协议
//    "跨批次单调推进"的结构前提）；rows 持有全部 JSON 深拷贝，大会话内存开销可观，
//    靠 close 与 shutdown（clear_cursors）释放；pos 到达 rows.len() 即取完。
// ③ 功能：一条消息游标的全部状态（数据 + 进度 + 批量）。

fn cursors() -> &'static Mutex<HashMap<i64, Cursor>> {
    static C: OnceLock<Mutex<HashMap<i64, Cursor>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
    // ① 实现方式：函数内 static OnceLock 惰性建全局 HashMap<游标 id, Cursor>。
    // ② 关键程序解析：游标 id 由 NEXT_CURSOR 全进程唯一自增，HashMap 天然支撑并发多游标；
    //    锁 poisoned 时 open/fetch/close 各按己方策略降级（-1 / 静默跳过），panic 不外泄。
    // ③ 功能：进程级游标登记表，open/fetch/close/shutdown 四方共享。
}

// 游标 id 发号器：从 1 单调自增（0 保留为"无效游标"哨兵，与出参初始化值呼应）
static NEXT_CURSOR: AtomicI64 = AtomicI64::new(1);

/// 物化会话消息：跨全部分表取行 → 归一 → 时间过滤 → 排序。
fn collect_rows(
    account: &Account,
    sid: &str,
    begin: i64,
    end: i64,
    ascending: bool,
) -> Result<Vec<Value>> {
    // 路由出会话全部表引用（专表/共表统一为 TableRef）
    let refs: Vec<TableRef> = session_table_refs(account, sid)?;
    // my_id 只取一次复用，供逐行归一时推断方向
    let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());
    let mut all: Vec<Value> = Vec::new();
    for t in &refs {
        // limit=0 → 取全表，时间过滤在 Rust 侧做（避免依赖 SQL 时间列探测）
        let rows = fetch_table(account, t, None, ascending, 0)?;
        let dbp = path_to_json_string(&t.db_path);
        for r in rows {
            let row = normalize_row(r, &dbp, &t.table, my_id.as_deref(), account, &t.db_path);
            // 时间过滤：begin/end 任一为 0 表示该侧不限（调用方未提供）
            let ct = row_create_time(&row);
            if begin > 0 && ct < begin {
                continue;
            }
            if end > 0 && ct > end {
                continue;
            }
            all.push(row);
        }
    }
    // 跨表合并后按统一 sort_key 排序，方向随 ascending（游标与统计共用同一顺序基准）
    all.sort_by(|a, b| {
        if ascending {
            sort_key(a).cmp(&sort_key(b))
        } else {
            sort_key(b).cmp(&sort_key(a))
        }
    });
    Ok(all)
    // ① 实现方式：路由表引用 → 逐表 fetch_table(limit=0 全取) → 逐行 normalize_row →
    //    row_create_time 双侧时间过滤 → 合并按 sort_key 排序返回。
    // ② 关键程序解析：时间过滤刻意放 Rust 而非 SQL——各库时间列名不一（sort_seq/
    //    create_time 候选），SQL 侧探测易漏列导致整表被误滤空；limit=0 全取是物化快照
    //    语义的前提（open 一次拿全量，fetch 只切片），用内存换协议正确性。
    // ③ 功能：物化"某会话在 [begin,end] 时间窗内的全部已排序消息"，游标与统计共用。
}

fn row_create_time(row: &Value) -> i64 {
    match row {
        Value::Object(m) => ci_get(m, "create_time").map(as_i64).unwrap_or(0),
        _ => 0,
    }
    // ① 实现方式：从归一后的行取 create_time（大小写不敏感）经 as_i64；缺列/非对象得 0。
    // ② 关键程序解析：0 表示"无时间"——collect_rows 的时间过滤（begin>0 时滤掉）与统计
    //    循环（ct<=0 不进桶）对 0 的口径一致；as_i64 的宽容性覆盖字符串时间列。
    // ③ 功能：取行的创建时间（秒），时间过滤与直方图分桶的统一入口。
}

fn row_is_send(row: &Value, my_id: Option<&str>) -> bool {
    // 非 object 行按"接收"处理（防御分支，归一后理论不会出现）
    let m = match row {
        Value::Object(m) => m,
        _ => return false,
    };
    // 首选 computed_is_send：Number/Bool/数字字符串三种形态都接受（非 0 即发送）
    if let Some(v) = ci_get(m, "computed_is_send") {
        match v {
            Value::Number(n) => return n.as_i64().unwrap_or(0) != 0,
            Value::Bool(b) => return *b,
            Value::String(s) => {
                if let Ok(n) = s.trim().parse::<i64>() {
                    return n != 0;
                }
            }
            _ => {}
        }
    }
    // 兜底：sender_username 与 my_id 全等判发送；任一缺失一律按接收
    match (ci_get(m, "sender_username"), my_id) {
        (Some(Value::String(s)), Some(me)) if !me.is_empty() => s == me,
        _ => false,
    }
    // ① 实现方式：两级判定——先读 normalize_row 算好的 computed_is_send（宽容三种 JSON
    //    形态），缺失时退回 sender_username==my_id 全等比较。
    // ② 关键程序解析：与 normalize_row 的推断逻辑同源但更宽容（多收字符串形态），统计
    //    侧直接消费归一数据；所有失败路径统一 false（按接收计），保证恒等式
    //    total==sent+received 恒成立，行不会被丢出统计。
    // ③ 功能：判定一条消息是发出（true）还是接收（false），方向统计的唯一依据。
}

fn row_local_type(row: &Value) -> i64 {
    match row {
        Value::Object(m) => ci_get(m, "local_type").map(as_i64).unwrap_or(1),
        _ => 1,
    }
    // ① 实现方式：取 local_type（大小写不敏感）经 as_i64；缺列/非对象兜底 1。
    // ② 关键程序解析：1 是微信文本消息最常见的类型码，未知行兜底进主流桶而不是 0，
    //    避免 typeCounts 出现意义不明的 0 类；ci_get 兼容 localType 等大小写变体。
    // ③ 功能：取消息类型码，供 typeCounts 分类计数。
}

// ── wcdb_open_message_cursor ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_open_message_cursor(
    handle: i64,
    session_id: *const c_char,
    batch_size: i32,
    ascending: i32,
    begin_timestamp: i32,
    end_timestamp: i32,
    out_cursor: *mut i64,
) -> i32 {
    with_account(handle, |account| {
        // 出参防御：NULL 直接失败；非 NULL 先写 0（0=无效游标哨兵，失败路径调用方读到 0）
        if out_cursor.is_null() {
            return ERR_GENERIC;
        }
        unsafe { *out_cursor = 0 };
        let sid = cstr(session_id);
        // 参数钳制：负时间戳归 0（=该侧不限）、批量最小 1（防 0 批量空转）、asc 布尔化
        let begin = begin_timestamp.max(0) as i64;
        let end = end_timestamp.max(0) as i64;
        let asc = ascending != 0;
        let batch = (batch_size.max(1)) as usize;

        // 消息库缓存为空 → -3（TS 触发 forceReopen 重试）
        if account.layout.message_dbs.is_empty() {
            push_log("open_message_cursor: no message db");
            return ERR_NO_MESSAGE_DB;
        }

        // 物化时间窗内全部命中行（含 Rust 侧时间过滤与全局排序）
        let rows = match collect_rows(account, sid, begin, end, asc) {
            Ok(r) => r,
            Err(e) => {
                let msg = e.to_string();
                push_log(&format!("open_message_cursor: {msg}"));
                // schema mismatch（无消息表/无法路由）→ -7
                return if msg.contains("schema mismatch") || msg.contains("无消息表") || msg.contains("消息分表") {
                    ERR_SCHEMA_MISMATCH
                } else {
                    ERR_GENERIC
                };
            }
        };

        // 自增发号并登记游标快照；锁 poisoned 时放弃登记返回 -1（不留半初始化游标）
        let id = NEXT_CURSOR.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut map) = cursors().lock() {
            map.insert(id, Cursor { rows, pos: 0, batch });
            unsafe { *out_cursor = id };
            OK
        } else {
            ERR_GENERIC
        }
    })
    // ① 实现方式：with_account → 出参清零 → 参数钳制 → message_dbs 空短路 -3 →
    //    collect_rows 物化快照 → NEXT_CURSOR 发号并写入全局游标表 → 出参回填 id。
    // ② 关键程序解析：错误码有意分流——"无消息库"是缓存未建（-3，TS 会 forceReopen
    //    重试），schema mismatch/无消息表（-7）与内部错误（-1）则不重试；物化快照把
    //    "跨批次单调推进"简化为纯下标移动（fetch 只动 pos），batch 最小 1 保证推进有效。
    // ③ 功能：游标协议 open 端点——建立会话消息快照游标并返回句柄 id。
}

// ── wcdb_fetch_message_batch ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_fetch_message_batch(
    handle: i64,
    cursor: i64,
    out_json: *mut *mut c_void,
    out_has_more: *mut i32,
) -> i32 {
    // 游标表是全局的；handle 仅用于 panic 防护与一致性校验（不强制匹配）。
    let _ = handle;
    guard(|| {
        // has_more 出参先清 0：任何失败路径调用方都读到"没有更多"
        if !out_has_more.is_null() {
            unsafe { *out_has_more = 0 };
        }
        // 锁 poisoned 视为游标表不可用，返回 -1（不触碰游标数据）
        let mut map = match cursors().lock() {
            Ok(m) => m,
            Err(_) => return ERR_GENERIC,
        };
        // 未知游标（未 open / 已 close / 已被清）记日志，便于定位调用方时序问题
        let c = match map.get_mut(&cursor) {
            Some(c) => c,
            None => {
                push_log(&format!("fetch_message_batch: unknown cursor {cursor}"));
                return ERR_GENERIC;
            }
        };
        // 按 batch 切片并推进 pos；推进后仍有剩余则 has_more=1（跨批次单调的保证）
        let start = c.pos;
        let end = (start + c.batch).min(c.rows.len());
        let slice: Vec<Value> = c.rows[start..end].to_vec();
        c.pos = end;
        let has_more = if c.pos < c.rows.len() { 1 } else { 0 };
        // 切片已深拷贝完成，先放锁再写 JSON，缩短临界区、不阻塞其他游标并发 fetch
        drop(map);
        write_out(out_json, &Value::Array(slice).to_string());
        if !out_has_more.is_null() {
            unsafe { *out_has_more = has_more };
        }
        OK
        // ① 实现方式：guard 防护 → 锁游标表 → 校验游标存在 → [pos, pos+batch) 深拷贝
        //    切片 → pos 推进 → 释放锁 → write_out 写 JSON 数组 → 回填 has_more。
        // ② 关键程序解析：end 用 min(rows.len()) 收口，pos 永不超过 len，切片不会越界
        //    panic；pos 到达 len 后切片为空数组 + has_more=0，满足协议"空数组收尾"；
        //    返回的是 to_vec 深拷贝，游标内部数据不受调用方影响；pos 只进不退，
        //    回看历史批次需重新 open。
        // ③ 功能：游标协议 fetch 端点——顺序取下一批消息并给出是否还有更多。
    })
}

// ── wcdb_close_message_cursor ──

#[no_mangle]
pub unsafe extern "C" fn wcdb_close_message_cursor(handle: i64, cursor: i64) -> i32 {
    let _ = handle;
    guard(|| {
        // 容忍未知/重复 close：一律返回 OK，绝不崩溃。
        if let Ok(mut map) = cursors().lock() {
            map.remove(&cursor);
        }
        OK
        // ① 实现方式：guard 防护下锁游标表 remove 该 id 并返回 OK；锁失败静默跳过。
        // ② 关键程序解析：remove 不存在的键是 no-op，天然幂等——重复 close、close 未知
        //    游标都安全；锁 poisoned 时残留游标由 clear_cursors/shutdown 兜底回收；
        //    handle 与 open 时不强校验，与 fetch 同一约定（游标表全局共享）。
        // ③ 功能：游标协议 close 端点——释放快照内存，防止大列表长期驻留。
    })
}

/// shutdown 时清空全部游标。
pub(crate) fn clear_cursors() {
    if let Ok(mut map) = cursors().lock() {
        map.clear();
    }
    // ① 实现方式：锁全局游标表一次性 clear()，锁失败静默跳过。
    // ② 关键程序解析：shutdown 后账号/连接不可用，残留游标只会白占内存且 fetch 已无
    //    意义，clear 让下次 open 从干净状态开始；拿不到锁说明正被并发持有，放弃清理
    //    不影响正确性（close/shutdown 仍有回收机会）。
    // ③ 功能：shutdown 阶段兜底回收全部游标快照（lib.rs 的 wcdb_shutdown 调用）。
}

// ── wcdb_get_aggregate_stats ──

/// 解析 session_ids_json：既接受字符串数组也接受数字数组（协议双形态）。
fn parse_session_ids(json_str: &str) -> Vec<String> {
    // 仅数组形态处理；null/字符串/对象等一律按"无会话"空列表
    match serde_json::from_str::<Value>(json_str) {
        Ok(Value::Array(arr)) => arr
            // 元素三形态归一：字符串原样、数字 to_string 保序转文本、其余 JSON 值字符串化
            .into_iter()
            .map(|v| match v {
                Value::String(s) => s,
                Value::Number(n) => n.to_string(),
                other => other.to_string(),
            })
            // 滤掉空白项（TS 可能传 ""/空白占位）
            .filter(|s| !s.trim().is_empty())
            .collect(),
        _ => Vec::new(),
    }
    // ① 实现方式：serde_json 解析 → 仅收数组 → 元素统一转 String → 滤空白 → collect；
    //    解析失败/非数组一律空 Vec。
    // ② 关键程序解析：协议允许字符串数组与数字数组双形态（session id 可能是数字），
    //    数字走 to_string 而非整轮序列化，避免浮点尾巴；宽松解析保证任何入参不 panic
    //    不报错，最坏只是统计 0 个会话（上层按空结果输出全 0 JSON）。
    // ③ 功能：把统计端点的会话列表入参解析成 Vec<String>。
}

fn local_parts(secs: i64) -> (u32, u32, String, String) {
    // 返回 (hour 0-23, weekday 0=周日..6=周六, "YYYY-MM", "YYYY-MM-DD")，按本地时区。
    // .single() 为 None（秒级戳越界等非法值）时退 epoch 0，保证任何输入不 panic
    let dt: DateTime<Local> = match Utc.timestamp_opt(secs, 0).single() {
        Some(d) => d.with_timezone(&Local),
        None => Utc.timestamp_opt(0, 0).unwrap().with_timezone(&Local),
    };
    // %H 两位小时回读 u32，解析失败兜底 0
    let hour = dt.format("%H").to_string().parse::<u32>().unwrap_or(0);
    // chrono weekday: Mon=0..Sun=6；协议要 SQLite strftime('%w'): Sun=0..Sat=6
    let wd = dt.format("%w").to_string().parse::<u32>().unwrap_or(0);
    let month = dt.format("%Y-%m").to_string();
    let day = dt.format("%Y-%m-%d").to_string();
    (hour, wd, month, day)
    // ① 实现方式：Utc.timestamp_opt 秒级戳 → with_timezone 转本地时区 → 用 %H/%w/
    //    %Y-%m/%Y-%m-%d 四个格式串产出四个分桶键。
    // ② 关键程序解析：%w 恰为 SQLite strftime 的"周日=0"约定（与 chrono weekday 的
    //    周一=0 差异已在行内注释说明），直方图口径与 SQL 习惯一致；非法时间戳退 0 会让
    //    1970-01-01 出现一条计数，属可接受的脏数据落桶而非崩溃。
    // ③ 功能：把秒级时间戳转成本地时区的 (小时, 星期, 月, 日) 统计分桶键。
}

#[no_mangle]
pub unsafe extern "C" fn wcdb_get_aggregate_stats(
    handle: i64,
    session_ids_json: *const c_char,
    begin_timestamp: i32,
    end_timestamp: i32,
    out_json: *mut *mut c_void,
) -> i32 {
    with_account(handle, |account| {
        let raw = cstr(session_ids_json);
        // 时间窗与游标同约定：负值钳 0，0 表示该侧不限
        let begin = begin_timestamp.max(0) as i64;
        let end = end_timestamp.max(0) as i64;
        // 立即闭包：统计主逻辑集中返回 Result，错误收敛给 emit_json
        let result = (|| -> Result<String> {
            let sids = parse_session_ids(raw);
            // 方向判定依赖的 my_id；未设置时收发统计全靠 computed_is_send
            let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());

            // 一次性建齐全部累加器：总数/收发/首末时间/类型/24 小时/星期/日/发送日/月
            let mut total: i64 = 0;
            let mut sent: i64 = 0;
            let mut received: i64 = 0;
            let mut first_time: i64 = 0;
            let mut last_time: i64 = 0;
            let mut type_counts: HashMap<String, i64> = HashMap::new();
            let mut hourly = [0i64; 24];
            let mut weekday: HashMap<String, i64> = HashMap::new();
            let mut daily: HashMap<String, i64> = HashMap::new();
            let mut sent_daily: HashMap<String, i64> = HashMap::new();
            let mut monthly: HashMap<String, i64> = HashMap::new();
            // sessions 存会话级明细；idMap 协议要求恒存在（当前无映射数据源，恒空对象）
            let mut sessions: Map<String, Value> = Map::new();
            let id_map: Map<String, Value> = Map::new();

            for sid in &sids {
                // 升序物化（时间过滤后范围确定）；失败按空会话跳过（见下行行尾注释）
                let rows = match collect_rows(account, sid, begin, end, true) {
                    Ok(r) => r,
                    Err(_) => continue, // 单会话无表/路由失败按空处理，不整体失败
                };
                // 会话内小计：仅该会话的计数，s_total>0 才写入 sessions 明细
                let mut s_total: i64 = 0;
                let mut s_sent: i64 = 0;
                let mut s_recv: i64 = 0;
                let mut s_last: i64 = 0;
                for row in &rows {
                    // 无时间戳的行（ct<=0）不进任何桶，保证直方图与 total 口径一致
                    let ct = row_create_time(row);
                    if ct <= 0 {
                        continue;
                    }
                    // 方向判定一次，供收发计数与 sentDaily 共用
                    let is_send = row_is_send(row, my_id.as_deref());
                    // 收发二分：恒等式 total==sent+received 由这条 if-else 保证
                    total += 1;
                    s_total += 1;
                    if is_send {
                        sent += 1;
                        s_sent += 1;
                    } else {
                        received += 1;
                        s_recv += 1;
                    }
                    // 全局首/末时间 + 会话内末时间（升序遍历，first_time 只会越写越小）
                    if first_time == 0 || ct < first_time {
                        first_time = ct;
                    }
                    if ct > last_time {
                        last_time = ct;
                    }
                    if ct > s_last {
                        s_last = ct;
                    }
                    // 类型计数：类型码转字符串作键（JSON 对象键必须是字符串）
                    *type_counts.entry(row_local_type(row).to_string()).or_insert(0) += 1;
                    // 时间分桶：小时直方图 + 星期/日/月三张映射，发送方向的日计数单独累计
                    let (h, wd, mk, dk) = local_parts(ct);
                    hourly[h as usize] += 1;
                    *weekday.entry(wd.to_string()).or_insert(0) += 1;
                    *daily.entry(dk.clone()).or_insert(0) += 1;
                    *monthly.entry(mk).or_insert(0) += 1;
                    if is_send {
                        *sent_daily.entry(dk).or_insert(0) += 1;
                    }
                }
                // 会话明细：有消息的会话才出现在 sessions 里（空会话不占键）
                if s_total > 0 {
                    sessions.insert(
                        sid.clone(),
                        json!({
                            "total": s_total,
                            "sent": s_sent,
                            "received": s_recv,
                            "lastTime": s_last,
                        }),
                    );
                }
            }

            // 汇总输出：字段名与协议逐一对应；hourly 是 24 槽数组，其余维度为映射
            let out = json!({
                "total": total,
                "sent": sent,
                "received": received,
                // 首末活跃时间（秒级戳；0 = 窗口内没有带有效时间戳的行）
                "firstTime": first_time,
                "lastTime": last_time,
                "typeCounts": type_counts,
                "hourly": hourly,
                "weekday": weekday,
                "daily": daily,
                "sentDaily": sent_daily,
                "monthly": monthly,
                // 会话级明细与保留字段：idMap 当前无数据源，恒输出空对象
                "sessions": Value::Object(sessions),
                "idMap": Value::Object(id_map),
            });
            // 序列化整体交给 emit_json；本闭包只负责产出 JSON 字符串
            Ok(out.to_string())
        })();
        emit_json(out_json, result, ERR_GENERIC)
        // ① 实现方式：with_account → 解析会话列表 → 逐会话 collect_rows 升序物化 →
        //    单遍循环同时累计全局与会话两级计数器 → json! 组装 13 个字段 → emit_json 写出。
        // ② 关键程序解析：单遍循环 O(n) 完成全部维度（类型/小时/星期/日/月/首末时间），
        //    sentDaily 只在 is_send 时累加故恒 ≤ daily；ct<=0 的行不进任何桶，口径与
        //    total 一致；单会话 collect 失败按空跳过，不让一个坏库拖垮整体统计；本端点
        //    失败码为 ERR_GENERIC(-1)，与消息组 schema 类失败（-7）区分。
        // ③ 功能：聚合统计端点——总量/收发/首末时间/类型分布/时间直方图/会话明细 JSON。
    })
}
