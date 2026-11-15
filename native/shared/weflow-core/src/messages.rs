//! 会话消息读取（跨分表合并 + 归一），供 wcdb.dll 与 welive-export 共用。

use crate::db::Account;
use crate::schema::{find_candidate, sanitize_ident, SessionTables};
use anyhow::Result;
use rusqlite::types::ValueRef;
use rusqlite::Connection;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};

const C_LOCAL_ID: &[&str] = &["local_id", "localId", "id", "msg_local_id", "rowid"];
// ① 实现方式：有序候选列名数组，供 find_candidate 在 PRAGMA 结果里按优先级匹配。
// ② 关键程序解析：`rowid` 放最后兜底（它是 SQLite 隐式列，PRAGMA table_info 通常不列出，
//    但 SELECT * 也不会返回它，所以实际很少命中）；`id` 这种泛化名排在中段，避免抢走更明确的 local_id。
// ③ 功能：定位"库内自增消息号"列，归一成 local_id。
const C_SERVER_ID: &[&str] = &["server_id", "serverId", "svr_id", "svrid", "msg_server_id", "server_msg_id"];
// ① 实现方式：服务器消息号的多版本候选列名。
// ② 关键程序解析：server_id 是 u64 量级大数，SQLite 以 INTEGER（i64）存储，超过 i64::MAX 会变负数；
//    本模块只透传不解释，welive-export 另附 serverIdRaw 字符串保真。
// ③ 功能：定位服务器消息号列，归一成 server_id。
const C_LOCAL_TYPE: &[&str] = &["local_type", "localType", "type", "msg_type", "message_type"];
// ① 实现方式：消息类型列候选（1=文本、3=图片、34=语音、43=视频、47=表情、49=引用/链接等）。
// ② 关键程序解析：本模块**不按类型分支处理内容**，只把类型值原样归一透传；
//    图片/语音正文是 zstd 压缩 BLOB，解压与类型语义判定都在上层（reply-exporter / TS）做。
// ③ 功能：定位消息类型列，归一成 local_type。
const C_STATUS: &[&str] = &["status", "msg_status", "local_status"];
// ① 实现方式：消息状态列候选（发送中/成功/失败/已读等位标记）。
// ② 关键程序解析：只透传原始整数，不解释位含义；上层据此过滤未成功发送的消息。
// ③ 功能：定位状态列，归一成 status。
const C_CREATE_TIME: &[&str] = &["create_time", "createTime", "msg_create_time", "create_timestamp", "time", "msg_time", "timestamp"];
// ① 实现方式：创建时间列候选，顺序即优先级，create_time 最先。
// ② 关键程序解析：本模块假定该列是**秒级** Unix 时间戳（begin/end 入参也是秒，sort_key 里 ×1000）；
//    代码里没有任何毫秒/秒的单位归一，若某分表存的是毫秒，时间窗过滤会把整表消息判为"超出 end"而全部丢弃。
// ③ 功能：定位时间列，用于排序、LIMIT 下推与时间窗过滤。
const C_SORT_SEQ: &[&str] = &["sort_seq", "sortSeq", "sequence", "seq", "msg_seq"];
// ① 实现方式：排序序号列候选，在 stream_table 里**优先于** create_time 被选作 ORDER BY 依据。
// ② 关键程序解析：sort_seq 是微信内部的单调序号（量级通常远大于秒级时间戳），
//    sort_key 把它直接当 i128 用、而 create_time 要 ×1000——两者混排时单位口径必须一致，否则跨表排序错乱。
// ③ 功能：定位比时间戳更精确的排序键（同一秒内多条消息的先后）。
const C_CONTENT: &[&str] = &["message_content", "messageContent", "content", "str_content", "msg_content"];
// ① 实现方式：正文列候选，message_content 优先。
// ② 关键程序解析：微信 4.x 图片/语音消息的 message_content 是 **BLOB（zstd 压缩）**，
//    value_ref_to_json 会把它 hex 编码成字符串透传，解压在上层做；本模块不触碰内容语义。
// ③ 功能：定位消息正文列，归一成 message_content。
const C_COMPRESS: &[&str] = &["compress_content", "compressContent", "compressed_content"];
// ① 实现方式：压缩正文列候选（WCDB 可能把正文放在独立的压缩列）。
// ② 关键程序解析：与 message_content 并列输出，**上层解码时优先用 compress_content**；
//    本模块不做优先级取舍，两列都归一保留。
// ③ 功能：定位压缩正文列，归一成 compress_content。
const C_IS_SEND: &[&str] = &["is_send", "isSend", "send", "is_sender"];
// ① 实现方式：「是否自己发出」标记列候选。
// ② 关键程序解析：该列存在时直接用作 computed_is_send；不存在才回退到 sender_username 与 my_id 比对。
// ③ 功能：定位发送方向标记列。
const C_SENDER: &[&str] = &["sender_username", "sender", "str_sender", "sender_id", "talker"];
// ① 实现方式：发送者列候选，群聊里用于区分具体发言人。
// ② 关键程序解析：`talker` 在单聊里是对方 id、在群聊里可能是发言人，语义随布局而变，故排在最后；
//    它同时是 my_id 比对判方向的依据，my_id 为 None 时 computed_is_send 只能是 Null。
// ③ 功能：定位发送者列，归一成 sender_username。

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
// ① 实现方式：(归一后的协议字段名, 候选列名数组) 的静态映射表，normalize_row 按此逐项补齐别名。
// ② 关键程序解析：这是"多候选列名 → 统一 snake_case 协议字段"的唯一真值表；
//    归一时**只新增别名键、不删除原始列键**，所以出参 JSON 里同一份内容可能出现两次
//    （如 message_content 与 msg_content），上层按协议名读即可。新增字段必须同步这里与上面的常量。
// ③ 功能：把微信各版本异构列名收敛成上层稳定消费的 10 个协议字段。

struct TableRef {
    db_path: PathBuf,
    table: String,
    filter: Option<(String, String)>,
    // ① 实现方式：私有结构体，描述"到哪个库的哪张表读、要不要加等值过滤"。
    // ② 关键程序解析：filter=Some((列名, 值)) 只在共享表模式（SessionTables::Shared）下出现，
    //    列名来自 talker 列探测、值是 session_id；stream_table 会把列名过 sanitize_ident 后拼进 WHERE，
    //    值走 ? 参数绑定。Located 模式下 filter=None（表本身就是会话专属的）。
    // ③ 功能：把路由结果转成可执行的单表读取任务。
}

fn ci_get<'a>(map: &'a Map<String, Value>, key_lower: &str) -> Option<&'a Value> {
    map.iter().find(|(k, _)| k.eq_ignore_ascii_case(key_lower)).map(|(_, v)| v)
    // ① 实现方式：在 JSON 对象里做大小写不敏感的键查找，返回值的借用（生命周期绑定 map）。
    // ② 关键程序解析：参数名叫 key_lower 但实际用 eq_ignore_ascii_case，传任何大小写都能匹配；
    //    线性扫描 O(键数)，每行都要调多次，是 normalize_row/sort_key 的热点之一。
    //    若同一对象里有多个仅大小写不同的键，返回**第一个**（serde_json Map 默认保插入序）。
    // ③ 功能：屏蔽数据库列名大小写差异，统一按协议名取值。
}

fn as_i64(v: &Value) -> i64 {
    match v {
        Value::Number(n) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        Value::String(s) => s.trim().parse::<i64>().unwrap_or(0),
        _ => 0,
    }
    // ① 实现方式：三分支归一——数字优先 as_i64，失败退 as_f64 再 as i64；字符串 trim 后 parse；其余一律 0。
    // ② 关键程序解析：as_f64 中转会对超过 2^53 的大数**丢精度**（server_id 量级），
    //    但本函数只用于 create_time/sort_seq（远小于 2^53），所以实际无损；
    //    所有失败路径都静默返回 0 而非报错——0 在时间窗过滤里会被当成"1970 年"，
    //    若 begin>0 则该行被丢弃，这是"缺时间列的消息静默消失"的根源之一。
    // ③ 功能：把异构 JSON 值稳健地转成 i64 时间戳/序号。
}

fn value_ref_to_json(v: ValueRef) -> Value {
    match v {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(i) => Value::Number(i.into()),
        ValueRef::Real(f) => serde_json::Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null),
        ValueRef::Text(t) => Value::String(String::from_utf8_lossy(t).into_owned()),
        ValueRef::Blob(b) => Value::String(hex::encode(b)),
        // ① 实现方式：按 SQLite 五种存储类逐一映射成 serde_json::Value，用 get_ref 避免提前拷贝。
        // ② 关键程序解析：三个关键分支——
        //    Real：from_f64 对 NaN/Inf 返回 None，故 unwrap_or(Null)，保证 JSON 合法（JSON 不允许 NaN）；
        //    Text：from_utf8_lossy 把非法 UTF-8 字节替换成 U+FFFD，**静默**不报错（微信正文正常是 UTF-8）；
        //    Blob：hex 编码成小写字符串——这是与上层的 wire 约定，图片/语音的 zstd BLOB 靠它透传，
        //    上层再 hex→bytes→zstd 解压；本模块**不做任何解压**，也不解释内容。
        // ③ 功能：把一行里的每个单元格无损（BLOB 以 hex 形式）转成 JSON 值。
    }
}

pub fn path_str(p: &Path) -> String {
    p.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/")
    // ① 实现方式：路径转字符串后把平台主分隔符（Windows 为 `\`）统一替换成 `/`。
    // ② 关键程序解析：只替换 MAIN_SEPARATOR 一种字符，不处理已经混用的斜杠；
    //    目的是让出参 JSON 里的 db_path 在 Windows/Unix 与 TS 侧都有一致写法，便于按路径去重/比对。
    // ③ 功能：产出跨平台稳定的路径字符串（用于 db_path 字段）。
}

fn normalize_row(raw: Value, db_path: &str, table: &str, my_id: Option<&str>) -> Value {
    let mut map = match raw {
        Value::Object(m) => m,
        other => {
            let mut m = Map::new();
            m.insert("value".to_string(), other);
            m
        }
    };
    // ① 实现方式：把入参拆成 JSON 对象；若不是对象（理论上不会），包一层 {"value": 原值} 兜底。
    // ② 关键程序解析：match 移动而非借用 raw，避免整行克隆；兜底分支保证后续 map 操作永远成立，
    //    不会 panic。正常路径下 map 就是 stream_table 产出的"列名→值"对象。
    // ③ 功能：取得可变的行对象，准备往里补协议别名。
    let keys: Vec<String> = map.keys().cloned().collect();
    // ① 实现方式：先把当前所有键克隆成快照 Vec。
    // ② 关键程序解析：必须快照——下面的循环会往 map 里 insert 新键，若直接迭代 map.keys()
    //    会同时借用又修改，编译不过且语义上会让新增别名参与后续匹配（造成自我覆盖）。
    // ③ 功能：固定"原始列名集合"，让别名匹配只针对数据库真实列。
    for (proto, cands) in FIELD_MAP {
        if ci_get(&map, proto).is_some() {
            continue;
        }
        // ① 实现方式：遍历 FIELD_MAP，若对象里已有该协议名（大小写不敏感）就跳过。
        // ② 关键程序解析：这个守卫保证"数据库本来就有 message_content 列"时不会被候选别名覆盖；
        //    也保证幂等——对同一行重复归一不会改变结果。
        // ③ 功能：避免覆盖已存在的协议字段。
        let mut found: Option<Value> = None;
        for cand in *cands {
            if let Some(k) = keys.iter().find(|k| k.eq_ignore_ascii_case(cand)) {
                if let Some(v) = map.get(k) {
                    found = Some(v.clone());
                    break;
                }
            }
        }
        // ① 实现方式：按候选序在键快照里找大小写不敏感匹配，命中则克隆其值并 break。
        // ② 关键程序解析：候选序即优先级（如 create_time 优先于 time）；clone 值是因为
        //    后面要 insert 进同一个 map（不能持有借用）；找不到保持 None，该协议字段就不出现在出参里。
        // ③ 功能：为每个协议字段挑出最合适的原始列值。
        if let Some(v) = found {
            map.insert(proto.to_string(), v);
        }
    }
    // ① 实现方式：命中才 insert 协议名键，原始列键**保留不删**。
    // ② 关键程序解析：因此出参里同一份内容可能同时以原始列名和协议名出现（体积翻倍但兼容性最好）；
    //    insert 用的是标准 snake_case 协议名，上层 TS/导出器按它消费。
    // ③ 功能：完成"异构列名 → 统一协议字段"的归一。
    let computed = match ci_get(&map, "is_send") {
        Some(v) => v.clone(),
        None => match (ci_get(&map, "sender_username"), my_id) {
            (Some(Value::String(s)), Some(me)) if !me.is_empty() => Value::Number(((s == me) as i64).into()),
            _ => Value::Null,
        },
    };
    // ① 实现方式：优先直接取 is_send 列；没有则用 sender_username 与 my_id 做字符串相等比较推出 0/1。
    // ② 关键程序解析：守卫 `!me.is_empty()` 防止把空账号 id 当成有效身份（否则所有空发送者都会被判成"我发的"）；
    //    任一条件不满足（无发送者列 / my_id 未设置 / 发送者非字符串）结果为 Null，
    //    上层据此知道"方向未知"，而不是误判成 0（对方发的）。s == me 是**大小写敏感**精确比较。
    // ③ 功能：给出一个尽量可靠、不可判定时如实为 Null 的发送方向。
    map.insert("computed_is_send".to_string(), computed);
    map.insert("db_path".to_string(), Value::String(db_path.to_string()));
    map.insert("table_name".to_string(), Value::String(table.to_string()));
    // ① 实现方式：无条件写入三个派生字段——方向判定结果、来源库路径、来源表名。
    // ② 关键程序解析：computed_is_send 与原始 is_send 并存（前者是判定结论，后者是数据库原值）；
    //    db_path/table_name 是跨分库合并后追溯"这条消息来自哪张表"的唯一线索，
    //    调试路由错误（如映射到别的会话表）时全靠它们。
    // ③ 功能：给每行打上来源与方向标签。
    if let Some(name) = PathBuf::from(db_path).file_stem().and_then(|s| s.to_str()) {
        map.insert("db_name".to_string(), Value::String(name.to_string()));
    }
    // ① 实现方式：从库路径取 file_stem（去掉 .db 后缀）作为 db_name，取不到就不写该字段。
    // ② 关键程序解析：file_stem 对 `message_0.db` 得到 `message_0`；非 UTF-8 文件名时 to_str 为 None，
    //    静默跳过而不是写乱码。db_name 比 db_path 短，适合做日志/分组键。
    // ③ 功能：补一个便于人读的分库标识。
    Value::Object(map)
    // ① 实现方式：把补齐后的 map 重新包成 Value::Object 返回。
    // ② 关键程序解析：整个过程只读原始值 + 新增键，不修改任何数据库内容；
    //    出参行数与输入行数一一对应，过滤（时间窗）由调用方在回调里做。
    // ③ 功能：交付归一后的消息行 JSON。
}

fn sort_key(v: &Value) -> i128 {
    let map = match v {
        Value::Object(m) => m,
        _ => return 0,
    };
    // ① 实现方式：取出对象引用，非对象直接返回排序键 0。
    // ② 关键程序解析：用 i128 是为了容纳"秒级时间戳 ×1000"以及大序号而不溢出 i64；
    //    返回 0 的行会被排到最前（升序）或最后（降序），属异常行的确定性兜底。
    // ③ 功能：准备计算跨表合并后的排序键。
    if let Some(sq) = ci_get(map, "sort_seq") {
        let n = as_i64(sq);
        if n != 0 {
            return n as i128;
        }
    }
    // ① 实现方式：优先用 sort_seq 原值作为排序键，非 0 即直接返回。
    // ② 关键程序解析：`n != 0` 守卫让"序号缺失/为 0"的行退回时间戳分支；
    //    **单位口径坑**：这里直接用 sort_seq 原值，而下面 create_time 要 ×1000，
    //    若 sort_seq 是小计数器（1,2,3…）而另一张表只有 create_time（秒×1000≈1.7e12），
    //    两类行的键量级悬殊，跨表合并排序会把它们完全分到两端（待实测 sort_seq 的真实量级）。
    // ③ 功能：用最精确的序号决定先后。
    if let Some(ct) = ci_get(map, "create_time") {
        return as_i64(ct) as i128 * 1000;
    }
    // ① 实现方式：无可用序号时退回 create_time，×1000 换算到与序号假定的同一毫秒量级。
    // ② 关键程序解析：×1000 隐含"create_time 是秒、sort_seq 是毫秒"的假设；
    //    若 create_time 本身已是毫秒，这里会再放大 1000 倍，排序仍单调但与序号行不可比。
    // ③ 功能：给出时间兜底排序键。
    0
    // ① 实现方式：两个字段都没有时返回 0。
    // ② 关键程序解析：这类行在时间窗过滤里通常已被丢弃（ct=0 < begin），能走到排序说明 begin=0；
    //    返回 0 保证比较器全序、不会 panic。
    // ③ 功能：异常行的确定性排序兜底。
}

fn session_table_refs(account: &Account, session_id: &str) -> Result<Vec<TableRef>> {
    session_table_refs_routed(account, session_id).map(|(v, _)| v)
    // ① 实现方式：转调 routed 版本，map 丢掉路由名只留 TableRef 列表。
    // ② 关键程序解析：流式全量导出走这个入口（不关心路由名）；"最近 n 条"路径走 routed 版本，
    //    因为它要把 route 与 time_fallback 一起回报给诊断。
    // ③ 功能：把会话定位结果转成可执行的单表读取任务列表。
}

/// 同 `session_table_refs`，把命中的路由（md5/mapping/shared/none）一起带出来。
fn session_table_refs_routed(account: &Account, session_id: &str) -> Result<(Vec<TableRef>, &'static str)> {
    let index = account.msg_index()?;
    let mut out = Vec::new();
    let (tables, route) = index.session_tables_routed(account, session_id)?;
    // ① 实现方式：取（懒构建并缓存的）分表索引，再调路由拿到 SessionTables + 路由名。
    // ② 关键程序解析：两处 `?` 分别冒泡"索引构建失败"与"路由硬错误"；
    //    路由返回空列表（route="none"）不是错误，out 会是空 Vec，上层据此读 0 条。
    // ③ 功能：确定该会话要读哪些表、走的是哪一级路由。
    match tables {
        SessionTables::Located(list) => {
            for (db_idx, table) in list {
                if let Some(db) = index.db_path(db_idx) {
                    out.push(TableRef { db_path: db.to_path_buf(), table, filter: None });
                }
            }
        }
        // ① 实现方式：模式 A——逐个 (db_idx, 表名) 还原库路径，filter=None（表本身就是会话专属）。
        // ② 关键程序解析：db_path 越界返回 None 时**静默跳过**该表（理论上不会发生，索引内部对齐）；
        //    to_path_buf 克隆路径，让 TableRef 脱离索引借用独立存在。
        // ③ 功能：生成不加过滤的精确表任务。
        SessionTables::Shared { tables, talker_column } => {
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
        // ① 实现方式：模式 B——每张表都带上 (talker 列名, session_id) 等值过滤。
        // ② 关键程序解析：talker_column 对每张表都克隆一份（列名相同），session_id 作为**绑定参数值**
        //    而非拼接进 SQL，因此 sid 里有引号/特殊字符也安全；漏加这个 filter 就会读到所有会话的消息。
        // ③ 功能：生成带 talker 过滤的共享表任务。
    }
    Ok((out, route))
}

/// 单表游标：每条原始行立刻交给 `on_row`，只在内存里保留当前一行。
/// `limit` 仅在该表**有时间列**时才可安全下推（否则 LIMIT 取到的是任意前 n 行，不是最近 n 行）。
fn stream_table<F>(account: &Account, t: &TableRef, ascending: bool, limit: Option<u64>, mut on_row: F) -> Result<u64>
where
    F: FnMut(Value) -> Result<()>,
{
    let cols = account.columns(&t.db_path, &t.table)?;
    let time_col = find_candidate(&cols, C_SORT_SEQ).or_else(|| find_candidate(&cols, C_CREATE_TIME));
    let safe_table = sanitize_ident(&t.table)?;
    // ① 实现方式：先取该表列名（走缓存），按「sort_seq 优先、create_time 兜底」选时间列，再把表名过白名单。
    // ② 关键程序解析：columns 失败（非法表名/库打不开）直接 `?` 冒泡；时间列为 None 时后面既不加 ORDER BY
    //    也不下推 LIMIT（否则 LIMIT 取到的是任意前 n 行而非最近 n 行）；sanitize_ident 是表名拼接的注入防线。
    // ③ 功能：备齐拼 SQL 所需的三要素——列结构、排序键、安全表名。
    let mut wheres: Vec<String> = Vec::new();
    let mut params: Vec<String> = Vec::new();
    // ① 实现方式：两个平行 Vec——wheres 存 SQL 片段（含 ? 占位），params 存对应的绑定值。
    // ② 关键程序解析：片段与值必须一一对应且顺序一致，后面 refs 按 params 顺序绑定；
    //    当前最多只有一个过滤条件（talker = sid），用 Vec 是为了将来可扩展多条件。
    // ③ 功能：准备 WHERE 子句的片段与参数容器。
    if let Some((col, val)) = &t.filter {
        wheres.push(format!("\"{}\" = ?", sanitize_ident(col)?));
        params.push(val.clone());
    }
    // ① 实现方式：有 filter 时把列名过 sanitize_ident 后拼成 `"<col>" = ?`，值克隆进 params 走绑定。
    // ② 关键程序解析：列名来自 talker 列探测（数据库内省结果），仍不可信，必须过白名单；
    //    值（session_id）**绝不拼进 SQL**，只作为 ? 参数，因此 sid 含引号/百分号都安全。
    // ③ 功能：为共享表模式构造安全的等值过滤。
    let where_sql = if wheres.is_empty() { String::new() } else { format!(" WHERE {}", wheres.join(" AND ")) };
    // ① 实现方式：无片段则空串，有则用 " AND " 连接并加 WHERE 前缀。
    // ② 关键程序解析：空串而非 "WHERE 1=1"，保证 Located 模式下的 SQL 与无过滤全表读完全一致；
    //    join 不会引入多余 AND（只有一个片段时就是它本身）。
    // ③ 功能：产出可直接拼进主 SQL 的 WHERE 子句。
    let order_sql = match &time_col {
        Some(tc) => format!(" ORDER BY \"{}\" {}", sanitize_ident(tc)?, if ascending { "ASC" } else { "DESC" }),
        None => String::new(),
    };
    // ① 实现方式：有时间列才拼 `ORDER BY "<col>" ASC|DESC`，列名过 sanitize_ident，方向由 ascending 决定。
    // ② 关键程序解析：time_col 优先是 sort_seq，所以"最近 n 条"实际是按序号取而非按时间戳取——
    //    若序号与时间不单调（如消息被编辑/迁移），取到的未必是时间上最近的 n 条；
    //    无时间列时整个 ORDER BY 省略，行序由 SQLite 决定（通常是 rowid 序），此时 LIMIT 也必须省略。
    // ③ 功能：决定库内行序，并为 LIMIT 下推提供安全前提。
    // limit 是 u64，格式化出来只可能是数字，不参与注入
    let limit_sql = match (limit, &time_col) {
        (Some(l), Some(_)) => format!(" LIMIT {l}"),
        _ => String::new(),
    };
    // ① 实现方式：二元组匹配——只有「调用方给了 limit」且「该表有时间列」时才拼 `LIMIT {l}`。
    // ② 关键程序解析：l 是 u64，格式化结果只可能是十进制数字，无注入面；
    //    缺时间列时即使传了 limit 也**忽略**（退回全量读），宁可慢也不能取错行——这是注释里强调的安全约束。
    // ③ 功能：在安全前提下把"取前 n 行"下推到 SQL，避免整表进内存。
    let sql = format!("SELECT * FROM \"{safe_table}\"{where_sql}{order_sql}{limit_sql}");
    // ① 实现方式：把四段拼成 `SELECT * FROM "<表>"[ WHERE …][ ORDER BY …][ LIMIT n]`。
    // ② 关键程序解析：SELECT * 而非具名列——因为列结构是运行时内省的，无法预知；
    //    表名/列名都已过白名单且用双引号包裹，值走 ? 绑定，整条 SQL 无注入面。
    //    没有显式事务，整轮游标读取处于 autocommit 读状态。
    // ③ 功能：产出该表的最终只读查询语句。
    account.with_conn(&t.db_path, move |conn: &Connection| {
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
        let names: Vec<String> = stmt.column_names().iter().map(|s| s.to_string()).collect();
        let mut rows = stmt.query(refs.as_slice())?;
        // ① 实现方式：在受保护连接上 prepare，把 params 转成 &dyn ToSql 切片绑定，取列名快照后开游标。
        // ② 关键程序解析：`move` 闭包取得 sql/params 所有权；column_names 必须在 query 前取一次并克隆成
        //    自有 Vec（借用的列名随 stmt 生命周期，逐行再取会有借用冲突）；refs 与 wheres 的 ? 顺序一一对应。
        //    整个游标期间连接 Mutex 一直被持有，回调里若再查同一个库会死锁（不可重入）。
        // ③ 功能：建立可逐行推进的只读游标。
        let mut count = 0u64;
        while let Some(row) = rows.next()? {
            let mut obj = Map::with_capacity(names.len());
            for (idx, name) in names.iter().enumerate() {
                obj.insert(name.clone(), value_ref_to_json(row.get_ref(idx)?));
            }
            // ① 实现方式：逐行推进游标，按列名快照把每个单元格 get_ref 后转 JSON，装进预分配的对象。
            // ② 关键程序解析：rows.next()? 的 `?` 让"游标中途出错"（如库被锁/损坏）立即冒泡并终止整表；
            //    get_ref 是零拷贝借用，转换时才克隆；内存里始终只有当前一行，与表总行数无关。
            // ③ 功能：把一行原始数据物化成"列名→JSON 值"对象。
            on_row(Value::Object(obj))?;
            count += 1;
        }
        // ① 实现方式：每行立刻交给回调，回调返回 Err 则 `?` 终止；成功才计数 +1。
        // ② 关键程序解析：count 统计的是**交给回调的行数**，不是过滤后保留的行数
        //    （时间窗过滤在回调内部做，被丢弃的行也计入这里的 count）；调用方自己另计 kept。
        // ③ 功能：流式消费，避免整表进内存。
        Ok(count)
    })
    // ① 实现方式：把本表读到的行数作为 with_conn 的返回值原样传出。
    // ② 关键程序解析：游标与 stmt 在闭包结束时 Drop，释放读锁；连接本身仍留在 Account 缓存里复用。
    // ③ 功能：交付单表读取行数，供上层累加。
}

/// 流式读取会话在 [begin,end]（秒，0=不限）内的消息，逐行回调，内存与整会话大小无关。
///
/// 顺序为「分库顺序 + 库内 sort_seq/create_time 顺序」，**不再做跨分库全局排序**。
/// 这不影响最终导出：WeFlow 读 raw JSONL 后总会按 create_time→local_id 重排
/// （ExportContext.ts::collectMessagesFromWeliveRaw 末尾的 rows.sort）。
/// 返回真正交给 `on_row` 的行数（已按 [begin,end] 过滤）。
pub fn stream_session_message_rows<F>(
    account: &Account,
    session_id: &str,
    begin: i64,
    end: i64,
    ascending: bool,
    mut on_row: F,
) -> Result<u64>
where
    F: FnMut(Value) -> Result<()>,
{
    let refs = session_table_refs(account, session_id)?;
    let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());
    let mut emitted = 0u64;
    // ① 实现方式：先定位该会话的所有表任务，再快照一份 my_account_id，初始化累计计数器。
    // ② 关键程序解析：my_id 用 `.lock().ok()` 容忍 poisoned，取不到就是 None（方向判定退化为 Null）；
    //    克隆成自有 String 是因为下面闭包要 move 捕获它，不能持有 MutexGuard。
    // ③ 功能：备齐跨表流式读取所需的上下文。
    for t in &refs {
        let dbp = path_str(&t.db_path);
        let table = t.table.clone();
        let mut kept = 0u64;
        // ① 实现方式：按分库顺序遍历表任务，每张表先把来源路径/表名克隆成闭包可捕获的自有值。
        // ② 关键程序解析：dbp 用 path_str 统一成斜杠写法；kept 是**本表过滤后保留**的行数，
        //    与 stream_table 返回的"读到的行数"不同，必须分开计。
        // ③ 功能：为每张表准备独立的来源标签与计数。
        stream_table(account, t, ascending, None, |raw| {
            let row = normalize_row(raw, &dbp, &table, my_id.as_deref());
            let ct = match &row {
                Value::Object(m) => ci_get(m, "create_time").map(as_i64).unwrap_or(0),
                _ => 0,
            };
            // ① 实现方式：limit 传 None（全量读该表），回调里先归一再取 create_time 转 i64。
            // ② 关键程序解析：取不到时间列时 ct=0——这会让下面 begin>0 的过滤把该行**静默丢弃**，
            //    即"没有可识别时间列的分表，在带时间窗的导出里贡献 0 条"；
            //    本模块不做秒/毫秒单位归一，ct 直接按秒与 begin/end 比较。
            // ③ 功能：把原始行归一并取出用于时间窗比较的时间戳。
            if (begin > 0 && ct < begin) || (end > 0 && ct > end) {
                return Ok(());
            }
            // ① 实现方式：闭区间过滤——begin>0 且 ct<begin，或 end>0 且 ct>end，则跳过该行（返回 Ok 继续游标）。
            // ② 关键程序解析：0 表示"该端不限"，所以 begin=0/end=0 时对应条件短路；
            //    过滤在**回调内**做而非 SQL WHERE，因此即使只要最近几分钟也要扫全表（无时间下推）。
            // ③ 功能：实现 [begin,end] 秒级时间窗筛选。
            on_row(row)?;
            kept += 1;
            Ok(())
        })?;
        // ① 实现方式：通过过滤的行交给上层回调，回调 Err 则 `?` 中断整表；成功才 kept+1。
        // ② 关键程序解析：stream_table 外层也有 `?`，游标错误/回调错误都会冒泡终止整个导出；
        //    被过滤掉的行不计入 kept，但 stream_table 的返回值（读到行数）在这里被忽略。
        // ③ 功能：逐行流式输出并统计真正保留的行数。
        emitted += kept;
    }
    // ① 实现方式：每张表读完后把 kept 累加进 emitted。
    // ② 关键程序解析：表间顺序 = 分库编号升序 + 路由结果顺序，**不做跨库全局排序**
    //    （上层 WeFlow 读 raw JSONL 后会自行按 create_time→local_id 重排）。
    // ③ 功能：累计整个会话的输出行数。
    Ok(emitted)
    // ① 实现方式：返回真正交给 on_row 的总行数（已过滤）。
    // ② 关键程序解析：内存占用与整会话大小无关，只与当前一行有关，适合百万级消息导出。
    // ③ 功能：交付流式读取的行数统计。
}

/// 读取会话在 [begin,end]（秒，0=不限）内的全部消息，归一并按时间排序。
/// 整会话进内存，只适合小批量/校验用途；导出大流请用 [`stream_session_message_rows`]。
pub fn session_message_rows(
    account: &Account,
    session_id: &str,
    begin: i64,
    end: i64,
    ascending: bool,
) -> Result<Vec<Value>> {
    let mut all: Vec<Value> = Vec::new();
    stream_session_message_rows(account, session_id, begin, end, ascending, |row| {
        all.push(row);
        Ok(())
    })?;
    // ① 实现方式：复用流式接口，回调只做一件事——把行 push 进内存 Vec。
    // ② 关键程序解析：时间窗过滤仍由 stream_session_message_rows 内部完成，这里拿到的已是过滤后的行；
    //    整会话进内存，只适合小批量/校验，大流必须用流式接口。`?` 冒泡任何读取错误（此时 all 被丢弃）。
    // ③ 功能：把流式结果物化成完整向量。
    all.sort_by(|a, b| {
        if ascending {
            sort_key(a).cmp(&sort_key(b))
        } else {
            sort_key(b).cmp(&sort_key(a))
        }
    });
    // ① 实现方式：用 sort_key 做跨表全局排序，ascending 决定正序/逆序（交换比较双方实现降序）。
    // ② 关键程序解析：sort_by 是稳定排序，键相同的行保持原有（分库）顺序；
    //    sort_key 优先 sort_seq、否则 create_time×1000，两类键量级若不一致会导致跨表错序（见 sort_key 注释）。
    // ③ 功能：产出按时间/序号有序的消息列表。
    Ok(all)
    // ① 实现方式：返回排好序的向量。
    // ② 关键程序解析：与流式接口不同，这里**做了**跨分库全局排序，因为结果要整体消费。
    // ③ 功能：交付有序的整会话消息（小批量用途）。
}

fn table_has_time(account: &Account, t: &TableRef) -> bool {
    match account.columns(&t.db_path, &t.table) {
        Ok(cols) => {
            find_candidate(&cols, C_SORT_SEQ).is_some() || find_candidate(&cols, C_CREATE_TIME).is_some()
        }
        Err(_) => false,
    }
    // ① 实现方式：取列名后判「有 sort_seq 或 有 create_time」，列内省失败一律返回 false。
    // ② 关键程序解析：判据与 stream_table 选 time_col 的口径完全一致（同样两套候选、同样优先级），
    //    保证"这里说有时间列"⇒"那里一定会下推 ORDER BY/LIMIT"；Err→false 是保守降级，
    //    让上层退回全量读而不是错误地下推 LIMIT。
    // ③ 功能：为"最近 n 条"路径判定该表能否安全下推 LIMIT。
}

/// 只取会话**最近 n 条**（新→旧）。
///
/// 每个分库各自 `ORDER BY 时间 DESC LIMIT n` 取回，再合并排序截断：全局最近 n 条必然落在
/// 各库最近 n 条里，所以结果与「整会话读进内存再截」一致，但内存与耗时的量级从
/// 「整会话行数」（本机实测单会话可到数万行）降到「库数 × n」。
/// 表没有时间列时（异常结构）LIMIT 下推不安全 ⇒ 退回全量读，宁可慢也不能取错行。
pub fn session_message_rows_recent(account: &Account, session_id: &str, n: usize) -> Result<Vec<Value>> {
    session_message_rows_recent_routed(account, session_id, n).map(|(v, _, _)| v)
    // ① 实现方式：转调 routed 版本，map 丢掉 route 与 time_fallback 只留消息向量。
    // ② 关键程序解析：这是导出器主用的入口（取最近 n 条喂 LLM）；诊断路径用 routed 版本看路由与是否走了慢路。
    // ③ 功能：给出会话最近 n 条消息（新→旧）。
}

/// 同 `session_message_rows_recent`，额外返回：
/// - `route`：会话→消息表命中的路由（md5/mapping/shared/none）
/// - `time_fallback`：是否因为分表缺时间列而退回"全量读再截断"（慢路）
pub fn session_message_rows_recent_routed(
    account: &Account,
    session_id: &str,
    n: usize,
) -> Result<(Vec<Value>, &'static str, bool)> {
    if n == 0 {
        return Ok((Vec::new(), "none", false));
    }
    // ① 实现方式：n=0 直接短路返回空向量 + 路由名 "none" + 未走慢路。
    // ② 关键程序解析：避免为"取 0 条"去构建索引/开库；这里的 "none" 是短路占位，
    //    与真实路由失败得到的 "none" 语义不同，但调用方拿到的都是空结果，不影响行为。
    // ③ 功能：零条请求的快速出口。
    let (refs, route) = session_table_refs_routed(account, session_id)?;
    if refs.iter().any(|t| !table_has_time(account, t)) {
        let all = session_message_rows(account, session_id, 0, 0, false)?;
        return Ok((all.into_iter().take(n).collect(), route, true));
    }
    // ① 实现方式：定位表任务并带出路由名；只要**任一张表**没有时间列，就整体退回"全量读再截断"。
    // ② 关键程序解析：用 any 而非 all——一张表不能安全下推 LIMIT，就不能保证"各库最近 n 条含全局最近 n 条"
    //    这个前提，必须全量读；begin/end 传 0 表示不做时间窗过滤，降序读后 take(n)。
    //    time_fallback=true 一路回报给诊断，让人知道这次走了慢路（整会话进内存）。
    // ③ 功能：在结构异常时保证结果正确性，宁可慢也不取错行。
    let my_id = account.my_account_id.lock().ok().and_then(|g| g.clone());
    let cap = n as u64;
    let mut all: Vec<Value> = Vec::new();
    // ① 实现方式：快路准备——快照 my_id、把 n 转成 u64 上限、建累积向量。
    // ② 关键程序解析：cap 就是下推给每张表的 LIMIT 值；各库都取 n 条，合并后最多 库数×n 行，
    //    内存量级远小于整会话。my_id 取不到时方向判定退化为 Null（不影响条数）。
    // ③ 功能：备齐快路所需上下文。
    for t in &refs {
        let dbp = path_str(&t.db_path);
        let table = t.table.clone();
        stream_table(account, t, false, Some(cap), |raw| {
            all.push(normalize_row(raw, &dbp, &table, my_id.as_deref()));
            Ok(())
        })?;
    }
    // ① 实现方式：逐表以 ascending=false（DESC）+ Some(cap) 调 stream_table，回调里归一后直接入向量。
    // ② 关键程序解析：这里**不做时间窗过滤**（最近 n 条按条数取，不按时间）；每表最多贡献 cap 行；
    //    任一表读取错误即 `?` 中断，已收集的行随 all 一起丢弃。
    // ③ 功能：把"各库最近 n 条"收集到一起。
    all.sort_by(|a, b| sort_key(b).cmp(&sort_key(a)));
    all.truncate(n);
    // ① 实现方式：按 sort_key 降序全局排序，再截断到 n 条。
    // ② 关键程序解析：正确性依据——全局最近 n 条必然落在各库最近 n 条的并集里，所以先各取 n 再合并截断
    //    与"整会话排序后取 n"等价；排序键若 sort_seq/create_time 量级不一致会影响这里的先后（见 sort_key）。
    // ③ 功能：产出真正的全局最近 n 条（新→旧）。
    Ok((all, route, false))
    // ① 实现方式：返回消息向量 + 路由名 + time_fallback=false（走的是快路）。
    // ② 关键程序解析：route 让调用方区分 md5/mapping/shared/none，是"微信改布局后从哪一级退化"的第一现场证据。
    // ③ 功能：交付最近 n 条消息及路由诊断信息。
}
